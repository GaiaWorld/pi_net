use std::io::{Write, Result, Error};
use std::sync::atomic::{AtomicUsize, Ordering};

use https::{status::StatusCode,
            header::{CONTENT_LENGTH}};
use flate2::{Compression, write::GzEncoder};
use bytes::BufMut;

use tcp::{Socket, SocketHandle, SocketEvent,
          utils::{SocketContext, Ready}};
use log::debug;

use crate::{service::HttpService,
            request::HttpRequest,
            response::HttpResponse,
            utils::{HttpRecvResult, ContentEncode}};
use crate::service::ServiceFactory;

// Http连接唯一ID分配器
static HTTP_CONNECT_UID_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);

/*
* Http连接
*/
pub struct HttpConnect<S: Socket, H: ServiceFactory<S, Service = HS>, HS: HttpService<S>> {
    uid:            usize,                  //唯一ID
    handle:         SocketHandle<S>,        //当前连接的Tcp连接句柄
    host:           H,                      //当前连接对应的主机
    service:        HS,                     //当前连接的服务
    keep_alive:     usize,                  //连接保持时间
}

unsafe impl<S: Socket, H: ServiceFactory<S, Service = HS>, HS: HttpService<S, >> Send for HttpConnect<S, H, HS> {}
unsafe impl<S: Socket, H: ServiceFactory<S, Service = HS>, HS: HttpService<S>> Sync for HttpConnect<S, H, HS> {}

impl<S: Socket, H: ServiceFactory<S, Service = HS>, HS: HttpService<S>> Clone for HttpConnect<S, H, HS> {
    // 重新分配当前主机的服务，并复制其它资源
    fn clone(&self) -> Self {
        HttpConnect {
            uid: self.uid,
            handle: self.handle.clone(),
            host: self.host.clone(),
            service: self.get_host().new_service(),
            keep_alive: self.keep_alive,
        }
    }
}

impl<S: Socket, H: ServiceFactory<S, Service = HS>, HS: HttpService<S>> Drop for HttpConnect<S, H, HS> {
    fn drop(&mut self) {
        debug!("Drop http connect, http_uid: {:?}, token: {:?}, tcp_uid: {:?}, remote: {:?}, local: {:?}, closed: {:?}",
            self.uid,
            self.handle.get_token(),
            self.handle.get_uid(),
            self.handle.get_remote(),
            self.handle.get_local(),
            self.handle.is_closed());
    }
}

/*
* Http连接同步方法
*/
impl<S: Socket, H: ServiceFactory<S, Service = HS>, HS: HttpService<S>> HttpConnect<S, H, HS> {
    /// 构建指定Tcp连接句柄、异步任务等待队列、Http服务和Http连接保持时长的Http连接
    pub fn new(handle: SocketHandle<S>,
               host: H,
               service: HS,
               keep_alive: usize) -> Self {
        HttpConnect {
            uid: HTTP_CONNECT_UID_ALLOCATOR.fetch_add(1, Ordering::Relaxed),
            handle,
            host,
            service,
            keep_alive,
        }
    }

    /// 获取Http连接的唯一ID
    pub fn get_uid(&self) -> usize {
        self.uid
    }

    /// 获取Http连接对应的主机
    pub fn get_host(&self) -> &H {
        &self.host
    }

    /// 异步回应指定的Http响应
    pub fn reply<B>(&self, buf: B) -> Result<()>
        where B: AsRef<[u8]> + Send + 'static {
        //首先回应本次Http请求
        self.handle.write_ready(buf)
    }

    /// 更新Http连接的超时时长
    pub fn update_timeout(&self) {
        let mut event = SocketEvent::empty();
        event.set::<usize>(self.keep_alive);
        self.handle.set_timeout(self.keep_alive, event);
    }

    /// 抛出服务器端指定错误，将通过本次Http请求的响应返回给客户端，并关闭当前Http连接
    pub fn throw(&self,
                 mut resp: HttpResponse,
                 mut error: StatusCode,
                 reason: Error) -> Result<()> {
        if !error.is_server_error() {
            //不是服务器端错误，则强制为服务器端未知错误
            error = StatusCode::INTERNAL_SERVER_ERROR;
        }

        //设置错误原因
        let body = reason.to_string().into_bytes();
        let len = body.len();

        //设置启始行和响应头
        resp.header(CONTENT_LENGTH.as_str(), len.to_string().as_str());
        resp.status(error.as_u16());
        let header: Vec<u8> = resp.into();

        //首先回应本次Http请求
        self.reply(vec![header, body].concat())?;

        //关闭当前Http连接
        self.close(Err(reason))
    }

    /// 关闭当前Http连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        //TODO 需要处理Http连接关闭时的相关问题...
        self.handle.close(reason)
    }
}

/*
* Http连接异步方法
*/
impl<S: Socket, H: ServiceFactory<S, Service = HS>, HS: HttpService<S>> HttpConnect<S, H, HS> {
    /// 运行连接上的服务
    pub async fn run_service(&mut self,
                             req: HttpRequest<S>) {
        self.update_timeout(); //在调用服务前，更新当前Http连接的超时时长
        match self.service.call(req).await {
            Err(e) => {
                //服务调用异常
                let resp = HttpResponse::empty();
                let _ = self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e.into());
            },
            Ok(resp) => {
                //服务调用完成，则序列化响应，并回应本次Http请求
                if resp.is_stream() {
                    //流响应
                    let content_encoding = resp.get_content_encode_by_stream().clone();
                    let (header_buf, body) = resp.into_header_and_body();

                    //首先发送响应头
                    if let Err(e) = self.reply(header_buf) {
                        //发送响应头错误，则立即抛出回应异常
                        let resp = HttpResponse::empty();
                        let _ = self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e);
                    } else {
                        //发送响应头成功
                        if let Some(body) = body {
                            //响应体存在，则异步获取响应体流
                            loop {
                                match body.next().await {
                                    HttpRecvResult::Err(e) => {
                                        //获取Http响应体错误，则立即发送回应异常
                                        let error_info = format!("{:?}", e);
                                        let error_info_len = error_info.as_bytes().len();
                                        let _ = self.reply(error_info_len.to_string() + "\r\n" + error_info.as_str() + "\r\n");
                                    },
                                    HttpRecvResult::Ok(Some((_index, part))) => {
                                        //获取到的是Http响应体块的后继，则立即向对端发送
                                        match encode_content(&content_encoding, part) {
                                            Err(e) => {
                                                //编码响应体块的后续失败，则立即发送回应异常
                                                let error_info = format!("{:?}", e);
                                                let error_info_len = error_info.as_bytes().len();
                                                let _ = self.reply(format!("{:x}", error_info_len) + "\r\n" + error_info.as_str() + "\r\n");
                                            },
                                            Ok(encoded) => {
                                                //编码响应体块的后续成功
                                                let mut buf = Vec::with_capacity(encoded.len() + 16);
                                                buf.put((format!("{:x}", encoded.len()) + "\r\n").as_bytes());
                                                buf.put(encoded.as_slice());
                                                buf.put("\r\n".as_bytes());

                                                let _ = self.reply(buf);
                                            },
                                        }
                                    },
                                    HttpRecvResult::Ok(None) => {
                                        //获取到的是Http响应体块的尾部，则发送流响应结束帧，并退出循环
                                        let _ = self.reply("0\r\n\r\n");
                                        break;
                                    },
                                    HttpRecvResult::Fin(_) => {
                                        //获取到的是Http响应体块的尾部，则发送流响应结束帧，并退出循环
                                        let _ = self.reply("0\r\n\r\n");
                                        break;
                                    },
                                }
                            }
                        } else {
                            //响应体不存在，则立即发送流响应结束帧
                            let _ = self.reply("0\r\n\r\n");
                        }
                    }
                } else {
                    //块响应
                    let buf: Vec<u8> = resp.into();
                    if let Err(e) = self.reply(buf) {
                        //回应错误，则立即抛出回应异常
                        let resp = HttpResponse::empty();
                        let _ = self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e);
                    }
                }
            },
        }
    }
}

// 编码Http内容
fn encode_content(encoding: &ContentEncode,
                  content: Vec<u8>) -> Result<Vec<u8>> {
    match encoding {
        ContentEncode::Gzip(level) => {
            //接受gzip编码
            let gzip = new_gzip(Vec::new(),
                                Compression::new(*level));
            match encode_gzip(gzip, content.as_slice()) {
                Err(e) => {
                    //编码错误，则立即返回错误
                    Err(e)
                },
                Ok(output) => {
                    //编码成功
                    Ok(output)
                },
            }
        },
        _ => {
            //暂时不支持其它内容编码，则忽略
            Ok(content)
        }
    }
}

// 创建指定流压缩级别的gzip编码器
fn new_gzip(writer: Vec<u8>, level: Compression) -> GzEncoder<Vec<u8>> {
    GzEncoder::new(writer, level)
}

// 进行gzip编码
fn encode_gzip(mut gzip: GzEncoder<Vec<u8>>, input: &[u8]) -> Result<Vec<u8>> {
    if let Err(e) = gzip.write_all(input) {
        //写入失败，则返回错误
        return Err(e);
    }

    gzip.finish()
}