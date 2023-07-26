use std::str::FromStr;
use std::io::{Error, Result, ErrorKind, Write, Read};

use url::form_urlencoded;
use mime::{APPLICATION, WWW_FORM_URLENCODED, JSON, OCTET_STREAM, PDF, TEXT, CHARSET, UTF_8, IMAGE, AUDIO, VIDEO, Mime};
use https::{Method, header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, CONTENT_LENGTH}, StatusCode};
use flate2::{Compression, FlushCompress, Compress, Status, write::GzEncoder};
use brotli::{CompressorReader, CompressorWriter};
use serde_json::{Result as JsonResult, Value};
use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded};

use pi_handler::SGenType;
use tcp::Socket;

use crate::{gateway::GatewayContext,
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::HttpResponse};

/*
* 默认支持的压缩算法
*/
pub const DEFLATE_ENCODING: &str = "deflate";
pub const GZIP_ENCODING: &str = "gzip";
pub const BROTLI_ENCODING: &str = "br";

///
/// Http请求和响应的默认分析器，处理Http请求的默认头和Http响应的默认头
/// 处理Http请求的查询
/// 处理Content-Type中的application/x-www-form-urlencoded、application/json、text，且只处理charset为utf8
/// 处理Accept-Encoding和Content-Encoding
/// 处理Content-Length
///
#[derive(Clone)]
pub struct DefaultParser {
    min_plain_limit:    usize,                          //支持压缩的最小Http响应体明文大小
    level:              Compression,                    //压缩级别
    buf_size:           usize,                          //压缩缓冲区大小
    flush:              FlushCompress,                  //刷新选项
    deflate_producor:   Sender<Compress>,               //deflate编码器生产者
    deflate_consumer:   Receiver<Compress>,             //deflate编码器消费者
}

unsafe impl Send for DefaultParser {}
unsafe impl Sync for DefaultParser {}

impl<S: Socket> Middleware<S, GatewayContext> for DefaultParser {
    fn request<'a>(&'a self,
                   context: &'a mut GatewayContext,
                   req: HttpRequest<S>)
                   -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let mut request = req;
        let future = async move {
            //当前请求有查询，则分析查询，并写入参数表
            for (key, value) in request.url().query_pairs() {
                context
                    .as_params()
                    .borrow_mut()
                    .insert(key.into_owned(),
                            SGenType::Str(value.into_owned()));
            }

            if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
                //当前请求有表单数据
                if let Ok(str) = content_type.to_str() {
                    if let Ok(mime) = Mime::from_str(str) {
                        if let Some(charset) = mime.get_param(CHARSET) {
                            //如果指定了请求体的字符集，则检查字符集是否满足要求
                            if charset != UTF_8 {
                                //请求体的字符集不满足要求，则立即退出请求
                                let mut resp = HttpResponse::empty();
                                resp.status(StatusCode::UNSUPPORTED_MEDIA_TYPE.as_u16());
                                return MiddlewareResult::Break(resp);
                            }
                        }

                        if mime.type_() == APPLICATION && mime.subtype() == WWW_FORM_URLENCODED {
                            //当前请求体使用了经过Url编码的表单结构，则分析，并写入参数表
                            if let Some(body) = request.take_body().await {
                                for (key, value) in form_urlencoded::parse(body.as_ref()) {
                                    context
                                        .as_params()
                                        .borrow_mut()
                                        .insert(key.into_owned(),
                                                SGenType::Str(value.into_owned()));
                                }
                            }
                        } else if mime.type_() == APPLICATION && mime.subtype() == JSON {
                            //当前请求体使用了Json，则分析，并写入参数表
                            if let Some(body) = request.take_body().await {
                                let opt: JsonResult<Value> = serde_json::from_slice(body.as_ref());
                                if let Ok(json) = opt {
                                    //Json对象，则直接写入关键字为空串，值为Json字符串的参数
                                    context
                                        .as_params()
                                        .borrow_mut()
                                        .insert("".to_string(),
                                                SGenType::Str(json.to_string()));
                                }
                            }
                        } else if mime.type_() == APPLICATION && mime.subtype() == OCTET_STREAM {
                            //当前请求体使用了二进制类型，则直接写入关键字为空串，值为二进制的参数
                            if let Some(body) = request.take_body().await {
                                context
                                    .as_params()
                                    .borrow_mut()
                                    .insert("".to_string(),
                                            SGenType::Bin(Vec::from(body.as_ref())));
                            }
                        } else if mime.type_() == TEXT {
                            //当前请求体使用了文本类型，则直接写入关键字为空串，值为文本的参数
                            if let Some(body) = request.take_body().await {
                                context
                                    .as_params()
                                    .borrow_mut()
                                    .insert("".to_string(),
                                            SGenType::Str(String::from_utf8_lossy(body.as_ref()).to_string()));
                            }
                        }
                    }
                }
            }

            //继续请求处理
            MiddlewareResult::ContinueRequest(request)
        };
        future.boxed_local()
    }

    fn response<'a>(&'a self,
                    context: &'a mut GatewayContext,
                    req: HttpRequest<S>,
                    resp: HttpResponse)
                    -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let mut response = resp;
        let future = async move {
            if response.as_body().is_none() {
                //本次Http响应没有响应体，则忽略编码
                return MiddlewareResult::ContinueResponse((req, response));
            }

            let mut is_codable = true; //响应体是否可编码
            if let Some(content_type) = response.get_header(CONTENT_TYPE) {
                //当前请求有表单数据
                if let Ok(str) = content_type.to_str() {
                    if let Ok(mime) = Mime::from_str(str) {
                        if mime.type_() == APPLICATION && (mime.subtype() == OCTET_STREAM || mime.subtype() == PDF) {
                            //当前响应体使用了二进制类型，则不可编码
                            is_codable = false;
                        } else if mime.type_() == IMAGE {
                            //当前响应体是图片，则不可编码
                            is_codable = false;
                        } else if mime.type_() == AUDIO {
                            //当前响应体是音频，则不可编码
                            is_codable = false;
                        } else if mime.type_() == VIDEO {
                            //当前响应体是视频，则不可编码
                            is_codable = false;
                        }
                    }
                }
            }

            if is_codable {
                //响应体可编码
                if let Some(accept_encoding) = req.headers().get(ACCEPT_ENCODING) {
                    if let Ok(value) = accept_encoding.to_str() {
                        for val in value.split(',') {
                            if let Some(encoding) = val.trim().split(';').next() {
                                match encoding.trim() {
                                    DEFLATE_ENCODING => {
                                        //接受deflate编码
                                        if let Some(body) = response.as_mut_body() {
                                            if body.len().is_none() || body.len().unwrap() < self.min_plain_limit {
                                                //响应体明文数据过小，则忽略编码
                                                break;
                                            }

                                            match self.deflate_consumer.try_recv() {
                                                Err(ref e) if e.is_disconnected() => {
                                                    //编码器通道错误，则立即抛出错误
                                                    return MiddlewareResult::Throw(Error::new(ErrorKind::Other,
                                                                                              format!("Http response body deflate encode failed, reason: {:?}",
                                                                                                      e)));
                                                },
                                                Err(_) => {
                                                    //没有空闲编码器，则创建新的编码器
                                                    if let Some(input) = body.as_slice() {
                                                        let mut deflate = new_deflate(self.level);
                                                        let mut output = Vec::with_capacity(input.len());
                                                        unsafe { output.set_len(output.capacity()); }
                                                        if let Err(e) = encode_deflate(&mut deflate,
                                                                                       input,
                                                                                       &mut output,
                                                                                       self.flush) {
                                                            //编码错误，则立即抛出错误
                                                            return MiddlewareResult::Throw(e);
                                                        }

                                                        //编码成功
                                                        if req.method() == &Method::HEAD {
                                                            //是HEAD方法请求的响应，则忽略响应体
                                                            body.reset(&[]);
                                                        } else {
                                                            //非HEAD方法请求的响应，则替换为编码成功后的响应体
                                                            body.reset(output.as_slice());
                                                        }

                                                        //设置响应头，并将创建的编码器加入空闲编码器队列中
                                                        response
                                                            .header(CONTENT_ENCODING.as_str(),
                                                                    DEFLATE_ENCODING);
                                                        response
                                                            .header(CONTENT_LENGTH.as_str(),
                                                                    deflate.total_out().to_string().as_str());
                                                        deflate.reset();
                                                        produce_deflate(self.deflate_producor.clone(),
                                                                        deflate);
                                                    }
                                                },
                                                Ok(mut deflate) => {
                                                    //有空闲编码器，则开始编码
                                                    if let Some(input) = body.as_slice() {
                                                        let cap = (input.len() as f64 * 0.75) as usize;
                                                        let mut output = Vec::with_capacity(input.len());
                                                        unsafe { output.set_len(output.capacity()); }
                                                        if let Err(e) = encode_deflate(&mut deflate,
                                                                                       input,
                                                                                       &mut output,
                                                                                       self.flush) {
                                                            //编码错误，则立即抛出错误
                                                            return MiddlewareResult::Throw(e);
                                                        }

                                                        //编码成功
                                                        if req.method() == &Method::HEAD {
                                                            //是HEAD方法请求的响应，则忽略响应体
                                                            body.reset(&[]);
                                                        } else {
                                                            //非HEAD方法请求的响应，则替换为编码成功后的响应体
                                                            body.reset(output.as_slice());
                                                        }

                                                        //设置响应头，并将使用后的编码器放入空闲编码器队列中
                                                        response
                                                            .header(CONTENT_ENCODING.as_str(),
                                                                    DEFLATE_ENCODING);
                                                        response
                                                            .header(CONTENT_LENGTH.as_str(),
                                                                    deflate.total_out().to_string().as_str());
                                                        deflate.reset();
                                                        produce_deflate(self.deflate_producor.clone(),
                                                                        deflate);
                                                    }
                                                },
                                            }
                                        }

                                        //已编码，则中止其它类型的编码
                                        break;
                                    },
                                    GZIP_ENCODING => {
                                        //接受gzip编码
                                        if let Some(body) = response.as_mut_body() {
                                            if body.len().is_none() || body.len().unwrap() < self.min_plain_limit {
                                                //响应体明文数据过小，则忽略编码
                                                break;
                                            }

                                            if let Some(input) = body.as_slice() {
                                                let gzip = new_gzip(Vec::new(),
                                                                    self.level);
                                                match encode_gzip(gzip, input) {
                                                    Err(e) => {
                                                        //编码错误，则立即抛出错误
                                                        return MiddlewareResult::Throw(e);
                                                    },
                                                    Ok(output) => {
                                                        //编码成功
                                                        if req.method() == &Method::HEAD {
                                                            //是HEAD方法请求的响应，则忽略响应体
                                                            body.reset(&[]);
                                                        } else {
                                                            //非HEAD方法请求的响应，则替换为编码成功后的响应体
                                                            body.reset(output.as_slice());
                                                        }

                                                        //设置响应头
                                                        response
                                                            .header(CONTENT_ENCODING.as_str(),
                                                                    GZIP_ENCODING);
                                                        response
                                                            .header(CONTENT_LENGTH.as_str(),
                                                                    output.len().to_string().as_str());
                                                    },
                                                }
                                            }
                                        }

                                        //已编码，则中止其它类型的编码
                                        break;
                                    },
                                    BROTLI_ENCODING => {
                                        //接受brotli编码
                                        if let Some(body) = response.as_mut_body() {
                                            if body.len().is_none() || body.len().unwrap() < self.min_plain_limit {
                                                //响应体明文数据过小，则忽略编码
                                                break;
                                            }

                                            if let Some(input) = body.as_slice() {
                                                let brotli = new_brotli(input, 8192, self.level);
                                                match encode_brotli(brotli, input.len()) {
                                                    Err(e) => {
                                                        //编码错误，则立即抛出错误
                                                        return MiddlewareResult::Throw(e);
                                                    },
                                                    Ok(output) => {
                                                        //编码成功
                                                        if req.method() == &Method::HEAD {
                                                            //是HEAD方法请求的响应，则忽略响应体
                                                            body.reset(&[]);
                                                        } else {
                                                            //非HEAD方法请求的响应，则替换为编码成功后的响应体
                                                            body.reset(output.as_slice());
                                                        }

                                                        //设置响应头
                                                        response
                                                            .header(CONTENT_ENCODING.as_str(),
                                                                    BROTLI_ENCODING);
                                                        response
                                                            .header(CONTENT_LENGTH.as_str(),
                                                                    output.len().to_string().as_str());
                                                    },
                                                }
                                            }
                                        }

                                        //已编码，则中止其它类型的编码
                                        break;
                                    },
                                    _ => {
                                        //服务器不支持客户端接受的编码，则继续
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            //继续响应处理
            if !response.contains_header(CONTENT_LENGTH) {
                //如果未设置内容长度，则设置内容长度
                if let Some(body_len) = response.as_body().unwrap().len() {
                    //当前响应有响应体
                    response
                        .header(CONTENT_LENGTH.as_str(),
                                body_len.to_string().as_str());
                } else {
                    response
                        .header(CONTENT_LENGTH.as_str(),
                                "0");
                }

                if req.method() == &Method::HEAD {
                    //是HEAD方法请求的响应，则忽略响应体
                    if let Some(body) = response.as_mut_body() {
                        body.reset(&[]);
                    }
                }
            }
            MiddlewareResult::ContinueResponse((req, response))
        };
        future.boxed_local()
    }
}

impl DefaultParser {
    /// 构建指定最小压缩明文大小和压缩级别的Http响应体的编码处理器
    pub fn with(min_plain_limit: usize,
                level: Option<u32>,
                buf_size: Option<usize>) -> Self {
        let (deflate_producor, deflate_consumer) = unbounded();

        //初始化编码器
        let level = if let Some(level) = level {
            if level > 9 {
                //如果压缩级别大于9，则设置为最大压缩
                Compression::best()
            } else if level > 0 {
                Compression::new(level)
            } else {
                //如果压缩级别为0，则设置为快速压缩
                Compression::fast()
            }
        } else {
            //默认快速压缩
            Compression::fast()
        };
        produce_deflate(deflate_producor.clone(), new_deflate(level));

        let buf_size = if let Some(size) = buf_size {
            if size < 4096 {
                4096
            } else {
                size.next_power_of_two()
            }
        } else {
            //默认压缩缓冲区大小
            8192
        };

        DefaultParser {
            min_plain_limit,
            level,
            buf_size,
            flush: FlushCompress::Finish, //默认的刷新选项
            deflate_producor,
            deflate_consumer,
        }
    }
}

// 创建指定压缩级别的deflate编码器
fn new_deflate(level: Compression) -> Compress {
    Compress::new(level, false)
}

// 创建指定流压缩级别的gzip编码器
fn new_gzip(writer: Vec<u8>, level: Compression) -> GzEncoder<Vec<u8>> {
    GzEncoder::new(writer, level)
}

// 创建指定流压缩级别的brotli编码器
fn new_brotli(reader: &[u8],
              buf_size: usize,
              level: Compression) -> CompressorReader<&[u8]> {
    CompressorReader::new(reader, buf_size, level.level(), 22)
}

// 线程安全的生成指定压缩级别的deflate编码器
fn produce_deflate(producor: Sender<Compress>, deflate: Compress) -> Result<()> {
    if let Err(e) = producor.send(deflate) {
        //发送编码器失败
        return Err(Error::new(ErrorKind::Other,
                              format!("New deflate encoding failed, reason: {:?}",
                                      e)));
    }

    Ok(())
}

/// 进行deflate编码
fn encode_deflate(deflate: &mut Compress,
                  input: &[u8],
                  output: &mut Vec<u8>,
                  flush: FlushCompress) -> Result<()> {
    match deflate.compress(input, output.as_mut_slice(), flush) {
        Err(e) => {
            //编码错误
            Err(Error::new(ErrorKind::Other,
                           format!("Http response body deflate encode failed, reason: {:?}",
                                   e)))
        },
        Ok(status) => {
            match status {
                Status::BufError => {
                    //输入缓冲区错误
                    Err(Error::new(ErrorKind::Other,
                                   format!("Http response body deflate encode failed, reason: buf error")))
                },
                Status::Ok => {
                    //输出缓冲区已满
                    let limit = input.len() * 2;
                    if (deflate.total_out() as usize) < limit {
                        //如果当前已输出的数据总长度小于输出缓冲区限制大小，则将输出缓冲区长度设置为限制大小，并继续解码
                        output.resize(limit, 0);
                        return encode_deflate(deflate, input, output, flush);
                    }

                    Err(Error::new(ErrorKind::Other,
                                   format!("Http response body deflate encode failed, reason: buf full")))
                },
                Status::StreamEnd => {
                    //因输入流结束，强制完成编码，则调整输出缓冲大小，并返回编码成功
                    output.truncate(deflate.total_out() as usize);
                    Ok(())
                },
            }
        },
    }
}

// 进行gzip编码
fn encode_gzip(mut gzip: GzEncoder<Vec<u8>>, input: &[u8]) -> Result<Vec<u8>> {
    if let Err(e) = gzip.write_all(input) {
        //写入失败，则返回错误
        return Err(e);
    }

    gzip.finish()
}

// 进行brotli编码
fn encode_brotli(mut brotli: CompressorReader<&[u8]>, init_capacity: usize) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(init_capacity);
    let _size = brotli.read_to_end(&mut result)?;
    Ok(result)
}
