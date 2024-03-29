use std::sync::Arc;
use std::str::FromStr;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::hash_map::Entry;
use std::io::{Error, Result, ErrorKind};

use https::{StatusCode, header::CONTENT_LENGTH, HeaderMap};
use futures::future::{FutureExt, LocalBoxFuture};

use pi_gray::GrayVersion;
use parking_lot::RwLock;
use pi_handler::{Args, Handler, SGenType};
use pi_atom::Atom;
use pi_hash::XHashMap;

use tcp::Socket;

use crate::{gateway::GatewayContext,
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::{ResponseHandler, HttpResponse},
            utils::HttpRecvResult};

/*
* Http连接句柄，用于为Http端口中间件中的handler提供灰度
*/
pub struct HttpGray {
    uid:        usize,          //Http连接的唯一id
    gray:       Option<usize>,  //灰度
}

unsafe impl Send for HttpGray {}
unsafe impl Sync for HttpGray {}

impl Clone for HttpGray {
    fn clone(&self) -> Self {
        HttpGray {
            uid: self.uid,
            gray: self.gray.clone(),
        }
    }
}

impl GrayVersion for HttpGray {
    fn get_gray(&self) -> &Option<usize> {
        &self.gray
    }

    fn set_gray(&mut self, gray: Option<usize>) {
        self.gray = gray;
    }

    fn get_id(&self) -> usize {
        self.uid
    }
}

///
/// Http端口中间件，连接Http与js虚拟机，用于将Http请求转递给js逻辑代码，并在js逻辑代码处理完成后将返回的值转换为Http响应
///
pub struct HttpPort {
    gray:       RwLock<Option<usize>>,                  //Http端口的灰度
    handler:    Arc<dyn Handler<
        A = SocketAddr,                                 //对端地址
        B = String,                                     //方法名
        C = Arc<HeaderMap>,                             //请求头
        D = Arc<RefCell<XHashMap<String, SGenType>>>,   //请求参数或请求Body
        E = ResponseHandler,                            //响应句柄
        F = (),
        G = (),
        H = (),
        HandleResult = ()
    >>,                                                 //Http请求服务异步处理器
}

unsafe impl Send for HttpPort {}
unsafe impl Sync for HttpPort {}

impl<S: Socket> Middleware<S, GatewayContext> for HttpPort {
    fn request<'a>(&'a self,
                   context: &'a mut GatewayContext,
                   req: HttpRequest<S>)
                   -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let future = async move {
            //处理请求
            let uid = req.get_handle().get_uid(); //获取当前http连接的唯一id
            let gray = self.get_gray(); //获取当前灰度
            let remote_addr = req.get_handle().get_remote().clone(); //获取当前http连接的对端地址
            let method = req.method().as_str().to_string();
            let headers = req.share_headers(); //获取当前http请求头
            let args = context.as_params().clone(); //获取http请求参数或请求体

            //检查是否有表单分段数据
            if !context.as_parts().is_empty() {
                //请求中有表单分段数据，则填充到参数中
                let parts = context.as_mut_parts();
                let keys = parts
                    .keys()
                    .map(|key| key.clone())
                    .collect::<Vec<String>>();
                let map = &mut *args.borrow_mut();
                for key in keys {
                    if let Some(value) = parts.remove(&key) {
                        map.insert(key, value);
                    }
                }
            }

            let resp = HttpResponse::new(2);
            if let Some(resp_handler) = resp.get_response_handler() {
                let http_gray = HttpGray {
                    uid,
                    gray,
                };
                self
                    .handler
                    .handle(Arc::new(http_gray),
                            Atom::from(req.url().path()),
                            Args::FiveArgs(remote_addr,
                                           method,
                                           headers,
                                           args,
                                           resp_handler)).await;
            }

            //完成请求处理
            MiddlewareResult::Finish((req, resp))
        };
        future.boxed_local()
    }

    fn response<'a>(&'a self,
                    context: &'a mut GatewayContext,
                    req: HttpRequest<S>,
                    resp: HttpResponse)
                    -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let mut body_bufs: Vec<Vec<u8>> = Vec::new();
        let mut response = resp;
        let future = async move {
            if response.is_stream() {
                //使用流响应，立即完成请求处理，并立即返回当前请求的流响应
                MiddlewareResult::Break(response)
            } else {
                //使用块响应
                if let Some(body) = response.as_mut_body() {
                    //当前响应有响应体，则持续获取响应体的内容
                    loop {
                        match body.body().await {
                            HttpRecvResult::Err(e) => {
                                //获取Http响应体错误
                                return MiddlewareResult::Throw(e);
                            },
                            HttpRecvResult::Ok(bodys) => {
                                //获取到的是Http响应体块的后继
                                for (_index, bin) in bodys {
                                    body_bufs.push(bin);
                                }
                            },
                            HttpRecvResult::Fin(bodys) => {
                                //获取到的是Http响应体块的尾部，处理后退出循环
                                body.init(); //未初始化，则初始化响应体
                                for buf in body_bufs {
                                    body.push(buf.as_slice());
                                }
                                for (_index, bin) in bodys {
                                    body.push(bin.as_slice());
                                }
                                break;
                            },
                        }
                    }
                }

                //继续响应处理
                MiddlewareResult::ContinueResponse((req, response))
            }
        };
        future.boxed_local()
    }
}

impl HttpPort {
    /// 构建指定异步请求处理器的Http端口中间件
    pub fn with_handler(gray: Option<usize>, handler: Arc<dyn Handler<
        A = SocketAddr,
        B = String,
        C = Arc<HeaderMap>,
        D = Arc<RefCell<XHashMap<String, SGenType>>>,
        E = ResponseHandler,
        F = (),
        G = (),
        H = (),
        HandleResult = ()
    >>) -> Self {
        let gray = RwLock::new(gray);

        HttpPort {
            gray,
            handler,
        }
    }

    //获取Http端口中间件的灰度
    pub fn get_gray(&self) -> Option<usize> {
        self.gray.read().as_ref().cloned()
    }

    //设置Http端口中间件的灰度，返回上一个灰度
    pub fn set_gray(&self, gray: Option<usize>) -> Option<usize> {
        let last = self.gray.write().take();
        *self.gray.write() = gray;
        last
    }
}
