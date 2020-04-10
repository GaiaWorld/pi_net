use std::sync::Arc;
use std::str::FromStr;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind, Write};

use bytes::BufMut;
use url::form_urlencoded;
use mime::{MULTIPART,
           FORM_DATA,
           BOUNDARY,
           AUDIO,
           VIDEO,
           IMAGE,
           FONT,
           APPLICATION,
           OCTET_STREAM,
           PDF,
           Mime,
           Name};
use twoway::{find_bytes, rfind_bytes};
use httparse::{EMPTY_HEADER, Result as ParseResult, Status, parse_headers};
use https::{header::{CONTENT_TYPE, CONTENT_DISPOSITION, HeaderName, HeaderValue}, StatusCode};
use futures::future::{FutureExt, BoxFuture};

use tcp::driver::{Socket, AsyncIOWait};
use handler::SGenType;

use crate::{gateway::GatewayContext,
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::HttpResponse,
            util::HttpRecvResult};

/*
* 多部分请求分隔符通用前后缀
*/
const MULTI_PARTS_COMMON_PREFIX_SUFFIX_BIN: &str = "--";

/*
* 多部分请求换行符
*/
const MULTI_PARTS_LINE_BREAK: &str = "\r\n";

/*
* 多部分的参数分隔符
*/
const MULTI_PARTS_PARAM_SPILT_CHAR: &str = ";";

/*
* 多部分的键值对分隔符
*/
const MULTI_PARTS_PAIR_SPILT_CHAR: &str = "=";

/*
* 需要过滤的无效字符集
*/
const MULTI_PARTS_FILTER_CHARS: &[char] = &[' ', '\t', '\"'];

/*
* 多部分请求时允许的最大Http头数量
*/
const MAX_MUTIL_PARTS_HTTP_HEADER_LIMIT: usize = 8;

/*
* 特殊的Mime子类型
*/
const X_MSDOWNLOAD_MIME_SUBTYPE: &str = "x-msdownload";

/*
* 多部分请求的表单参数名
*/
const MULTI_PARTS_NAME_PARAM: &str = "name";
const MULTI_PARTS_FILE_NAME_PARAM: &str = "filename";

/*
* Http请求的多部分请求体分析器
*/
pub struct MutilParts {
    block_size: usize,  //每次读取的请求体块大小
}

unsafe impl Send for MutilParts {}
unsafe impl Sync for MutilParts {}

impl<S: Socket, W: AsyncIOWait> Middleware<S, W, GatewayContext> for MutilParts {
    fn request<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>)
                   -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let block_size = self.block_size;
        let mut request = req;
        let future = async move {
            if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
                if let Ok(str) = content_type.to_str() {
                    if let Ok(mime) = Mime::from_str(str) {
                        if mime.type_() == MULTIPART && mime.subtype() == FORM_DATA {
                            //当前请求体使用了多部分请求体，则分析，并写入参数表
                            if let Some(param) = mime.get_param(BOUNDARY) {
                                //获取本次Http多部分请求体的分隔符
                                let boundary_str = (MULTI_PARTS_COMMON_PREFIX_SUFFIX_BIN.to_string() + param.as_str() + MULTI_PARTS_LINE_BREAK);
                                let boundary = boundary_str.as_bytes();
                                loop {
                                    if let Some(bin) = request.next_body(block_size).await {
                                        //读取到指定块大小的下个部分的请求体
                                        match parse_part(context, boundary, bin) {
                                            Err(e) => {
                                                //解析部分数据时错误，则抛出错误
                                                return MiddlewareResult::Throw(e);
                                            },
                                            _ => {
                                                //继续读取请求体
                                                continue;
                                            },
                                        }
                                    }

                                    //退出多部分的读取
                                    break;
                                }

                                //完成请求体的剩余部分的处理
                                if let Err(e) = parse_part_end(context, param.as_str().as_bytes()) {
                                    //解析部分数据时错误，则抛出错误
                                    return MiddlewareResult::Throw(e);
                                }
                            }
                        }
                    }
                }
            }

            //继续请求处理
            MiddlewareResult::ContinueRequest(request)
        };
        future.boxed()
    }

    fn response<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>, resp: HttpResponse<S, W>)
                    -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let future = async move {
            //继续响应处理
            MiddlewareResult::ContinueResponse((req, resp))
        };
        future.boxed()
    }
}

impl MutilParts {
    //构建指定块大小的Http多部分请求体分析器
    pub fn with(block_size: usize) -> Self {
        MutilParts {
            block_size,
        }
    }
}

//解析二进制数据，从中分析出多部分请求中的部分，中间数据存储在当前Http连接的网关上下文中，返回false，表示没有分析到完整的部分，需要继续读取数据后再次解析，否则返回已解析的部分
fn parse_part<'a>(context: &'a mut GatewayContext, boundary: &'a [u8], mut bin: &'a [u8]) -> Result<bool> {
    if let Some(mut part_buf) = context.take_part_buf() {
        //上次有未解析完的部分数据，则与本次的部分数据合并，并继续解析
        part_buf.put(bin);
        return parse_part(context, boundary, &part_buf[..]);
    }

    let mut offset;
    let mut r = Ok(false); //是否已解析
    loop {
        if let Some(index) = find_bytes(bin, boundary) {
            //查找到指定boundary分隔的负载，则继续查找负载
            if let Err(e) = parse_part_headers_body(context, &bin[0..index]) {
                return Err(e);
            }

            r = Ok(true); //设置为已解析

            offset = index + boundary.len();
            if offset >= bin.len() {
                //已经解析到数据尾，则结束本次解析
                return r;
            }

            //继续解析剩余数据
            bin = &bin[(index + boundary.len())..];
        } else {
            //查找不到指定boundary分隔的负载，则中止本次解析
            context.push_part_buf(bin); //加入请求体未解析部分
            return r;
        }
    }
}

//解析二进制数据，从中分析出多部分请求的结尾部分
fn parse_part_end<'a>(context: &'a mut GatewayContext, boundary: &'a [u8]) -> Result<()> {
    if let Some(mut part_buf) = context.take_part_buf() {
        //解析剩余的非结尾部分
        let boundary_bin = [MULTI_PARTS_COMMON_PREFIX_SUFFIX_BIN.as_bytes(),
            boundary, MULTI_PARTS_LINE_BREAK.as_bytes()].concat();
        let boundary_ = boundary_bin.as_slice();
        if let Err(e) = parse_part(context, boundary_, &part_buf[..]) {
            return Err(e);
        }
    }

    if let Some(mut part_buf) = context.take_part_buf() {
        //解析结尾部分
        let boundary_end_bin = [MULTI_PARTS_COMMON_PREFIX_SUFFIX_BIN.as_bytes(),
            boundary, MULTI_PARTS_COMMON_PREFIX_SUFFIX_BIN.as_bytes(), MULTI_PARTS_LINE_BREAK.as_bytes()].concat();
        let boundary_end = boundary_end_bin.as_slice();
        if let Some(index) = rfind_bytes(&part_buf[..], boundary_end) {
            //查找到指定boundary分隔的结尾负载
            if let Err(e) = parse_part_headers_body(context, &part_buf[0..index]) {
                return Err(e);
            }
        }
    }

    Ok(())
}

//解析部分数据的头和体
fn parse_part_headers_body<'a>(context: &'a mut GatewayContext, part: &'a [u8]) -> Result<()> {
    if part.len() == 0 {
        //部分数据长度为0，则忽略
        return Ok(());
    }

    let mut headers = [EMPTY_HEADER; MAX_MUTIL_PARTS_HTTP_HEADER_LIMIT];
    match parse_headers(part, &mut headers) {
        Err(e) => {
            //解析部分数据的头错误，则立即返回错误原因
            Err(Error::new(ErrorKind::Other, format!("parse multi parts headers failed, reason: {:?}", e)))
        },
        Ok(status) if status.is_complete() => {
            //解析部分数据的头完成
            let (offset, headers) = status.unwrap();

            //将头数据写入参数表
            let mut name_key = None;
            let context_parts = context.as_mut_parts();
            for header in headers {
                match HeaderName::try_from(header.name) {
                    Err(e) => {
                        return Err(Error::new(ErrorKind::InvalidData, format!("parse multi parts headers name failed, reason: {:?}", e)));
                    },
                    Ok(key) => {
                        match HeaderValue::from_bytes(header.value) {
                            Err(e) => {
                                return Err(Error::new(ErrorKind::InvalidData, format!("parse multi parts headers value failed, reason: {:?}", e)));
                            },
                            Ok(value) => {
                                //构建值成功
                                if let Ok(value_str) = value.to_str() {
                                    if key == CONTENT_DISPOSITION {
                                        //部分数据默认的头
                                        let params: Vec<&str> = value_str.split(MULTI_PARTS_PARAM_SPILT_CHAR).collect();
                                        for param in params {
                                            let pair: Vec<&str> = param.split(MULTI_PARTS_PAIR_SPILT_CHAR).collect();
                                            if let Some(param_key) = pair.get(0) {
                                                if let Some(param_val) = pair.get(1) {
                                                    //只解析键值对
                                                    let k = param_key.trim_matches(MULTI_PARTS_FILTER_CHARS);
                                                    let v = param_val.trim_matches(MULTI_PARTS_FILTER_CHARS);
                                                    match k {
                                                        MULTI_PARTS_NAME_PARAM => {
                                                            //有名为name的参数，则需要读取部分的体数据
                                                            name_key = Some(v.to_string());
                                                        },
                                                        MULTI_PARTS_FILE_NAME_PARAM => {
                                                            //有名为filename的参数，则写入网产在上下文的部分数据中
                                                            context_parts.insert(String::from_utf8_lossy(k.as_bytes()).to_string(), SGenType::Str(String::from_utf8_lossy(v.as_bytes()).to_string()));
                                                        },
                                                        _ => {
                                                            //其它参数，则忽略
                                                            continue;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    } else if key == CONTENT_TYPE {
                                        //表示当前部分的体数据有指定类型
                                        if let Some(name_key) = name_key.take() {
                                            if let Ok(mime) = Mime::from_str(value_str) {
                                                println!("!!!!!!mime: {:?}", mime);
                                                if mime.type_() == AUDIO
                                                    || mime.type_() == VIDEO
                                                    || mime.type_() == IMAGE
                                                    || mime.type_() == FONT
                                                    || (mime.type_() == APPLICATION
                                                    && (mime.subtype() == OCTET_STREAM || mime.subtype() == PDF || mime.subtype().as_str() == X_MSDOWNLOAD_MIME_SUBTYPE)) {
                                                    //写入二进制的体数据
                                                    context_parts.insert(name_key, SGenType::Bin(Vec::from(&part[offset..(part.len() - MULTI_PARTS_LINE_BREAK.len())])));
                                                } else {
                                                    //写入文本的体数据
                                                    context_parts.insert(name_key, SGenType::Str(String::from_utf8_lossy(&part[offset..(part.len() - MULTI_PARTS_LINE_BREAK.len())]).to_string()));
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                        }
                    },
                }
            }

            if let Some(name_key) = name_key.take() {
                //当前部分的体数据没有指定类型，则写入明文的体数据
                context_parts.insert(name_key, SGenType::Str(String::from_utf8_lossy(&part[offset..(part.len() - MULTI_PARTS_LINE_BREAK.len())]).to_string()));
            }
            Ok(())
        },
        _ => {
            //解析部分数据不完整，则立即返回错误原因
            Err(Error::new(ErrorKind::Other, "parse multi parts headers failed, reason: part not enough"))
        }
    }
}
