use std::sync::Arc;
use std::cell::RefCell;
use std::time::SystemTime;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};

use mime::Mime;
use bytes::BufMut;
use futures::future::{FutureExt, LocalBoxFuture};

use pi_hash::XHashMap;
use pi_handler::SGenType;
use tcp::Socket;

use crate::{service::{HttpService, ServiceFactory},
            route::{RouterTab, HttpRoute},
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::HttpResponse};

///
/// Http路由服务上下文
///
#[derive(Clone)]
pub struct GatewayContext {
    params:     Arc<RefCell<XHashMap<String, SGenType>>>,   //Http连接请求参数表
    parts:      XHashMap<String, SGenType>,                 //Http连接的请求体已解析部分
    cache_args: Option<(String, Mime, SystemTime)>,         //Http缓存参数
    files_size: u64,                                        //Http批量加载文件大小
    files_len:  usize,                                      //Http批量加载文件数量
    attrs:      XHashMap<String, SGenType>,                 //Http连接属性表
    part_buf:   Option<Vec<u8>>,                            //Http连接的请求体未解析部分缓冲
}

unsafe impl Send for GatewayContext {}
unsafe impl Sync for GatewayContext {}

impl GatewayContext {
    /// 构建Http路由服务上下文
    pub fn new() -> Self {
        GatewayContext {
            params: Arc::new(RefCell::new(XHashMap::default())),
            parts: XHashMap::default(),
            cache_args: None,
            files_size: 0,
            files_len: 0,
            attrs: XHashMap::default(),
            part_buf: None,
        }
    }

    /// 获取请求参数表的引用
    pub fn as_params(&self) -> &Arc<RefCell<XHashMap<String, SGenType>>> {
        &self.params
    }

    /// 清空请求参数表
    pub fn clear_params(&mut self) {
        self.params.borrow_mut().clear();
    }

    /// 获取请求体已解析部分的只读引用
    pub fn as_parts(&self) -> &XHashMap<String, SGenType> {
        &self.parts
    }

    /// 获取请求体已解析部分的可写引用
    pub fn as_mut_parts(&mut self) -> &mut XHashMap<String, SGenType> {
        &mut self.parts
    }

    /// 清空请求体已解析部分
    pub fn clear_parts(&mut self) {
        self.parts.clear();
    }

    /// 获取Http请求缓存参数
    pub fn get_cache_args(&self) -> Option<(String, Mime, SystemTime)> {
        if let Some((file_path, mime, last_modified)) = &self.cache_args {
            return Some((file_path.clone(), mime.clone(), last_modified.clone()));
        }

        None
    }

    /// 设置Http请求缓存参数
    pub fn set_cache_args(&mut self, args: Option<(String, Mime, SystemTime)>) {
        self.cache_args = args;
    }

    /// 获取Http批量加载文件大小
    pub fn get_files_size(&self) -> u64 {
        self.files_size
    }

    /// 设置Http批量加载文件大小
    pub fn set_files_size(&mut self, size: u64) {
        self.files_size = size;
    }

    /// 获取Http批量加载文件数量
    pub fn get_files_len(&self) -> usize {
        self.files_len
    }

    /// 设置Http批量加载文件数量
    pub fn set_files_len(&mut self, len: usize) {
        self.files_len = len;
    }

    /// 获取属性数量
    pub fn len(&self) -> usize {
        self.attrs.len()
    }

    /// 检查是否有指定属性
    pub fn contains_key(&self, key: &String) -> bool {
        self.attrs.contains_key(key)
    }

    /// 获取指定属性的值
    pub fn get(&self, key: &String) -> Option<&SGenType> {
        self.attrs.get(key)
    }

    /// 设置指定属性的值，返回属性的上一个值
    pub fn set(&mut self, key: String, value: SGenType) -> Option<SGenType> {
        self.attrs.insert(key, value)
    }

    /// 移除指定属性的值，返回被移除的值
    pub fn remove(&mut self, key: &String) -> Option<SGenType> {
        self.attrs.remove(key)
    }

    /// 获取请求体未解析部分缓冲
    pub fn take_part_buf(&mut self) -> Option<Vec<u8>> {
        self.part_buf.take()
    }

    /// 设置请求体未解析部分缓冲
    pub fn push_part_buf(&mut self, buf: &[u8]) {
        if let Some(part_buf) = &mut self.part_buf {
            return part_buf.put_slice(buf);
        }

        self.part_buf = Some(Vec::from(buf));
    }
}

///
/// Http网关，每个Http连接和一个Http网关绑定
///
pub struct HttpGateway<S: Socket, H: Middleware<S, GatewayContext>> {
    context:    GatewayContext,                     //上下文
    router_tab: RouterTab<S, GatewayContext, H>,    //路由器表
}

unsafe impl<S: Socket, H: Middleware<S, GatewayContext>> Send for HttpGateway<S, H> {}
unsafe impl<S: Socket, H: Middleware<S, GatewayContext>> Sync for HttpGateway<S, H> {}

impl<S: Socket, H: Middleware<S, GatewayContext>> HttpService<S> for HttpGateway<S, H> {
    type Error = Error;

    fn call(&mut self, req: HttpRequest<S>)
            -> LocalBoxFuture<'static, GenResult<HttpResponse, Self::Error>> {
        let middleware = self
            .router_tab
            .match_route(req.method(),
                         req.url().path());
        let mut context = self.context.clone();

        let future = async move {
            if let Some(ware) = middleware {
                //路由到指定方法和路径的Http请求处理器
                context.clear_params(); //每次请求处理前，清空网关上下文内的请求参数表
                context.set_cache_args(None); //每次请求处理前，重置网关上下文内的缓存参数
                match ware.request(&mut context, req).await {
                    MiddlewareResult::Break(resp) => {
                        //中止请求处理，则立即返回响应
                        Ok(resp)
                    },
                    MiddlewareResult::Finish((req, resp)) => {
                        //请求处理完成，则开始响应处理
                        match ware.response(&mut context, req, resp).await {
                            MiddlewareResult::Break(resp) => {
                                //退出响应处理，则立即返回响应
                                Ok(resp)
                            },
                            MiddlewareResult::Finish((_, resp)) => {
                                //响应处理完成，则返回响应
                                Ok(resp)
                            },
                            MiddlewareResult::Throw(reason) => {
                                //中止响应处理，并立即返回错误
                                Err(reason)
                            },
                            _ => {
                                //无效的请求返回，则立即返回错误
                                Err(Error::new(ErrorKind::Other,
                                               "Invalid middleware result"))
                            }
                        }
                    },
                    MiddlewareResult::Throw(reason) => {
                        //中止请求处理，并立即返回错误
                        Err(reason)
                    },
                    _ => {
                        //无效的请求返回，则立即返回错误
                        Err(Error::new(ErrorKind::Other,
                                       "Invalid middleware result"))
                    },
                }
            } else {
                //路由错误，则立即返回错误原因
                Err(Error::new(ErrorKind::Other,
                               "Invalid route"))
            }
        };
        future.boxed_local()
    }
}

impl<S: Socket, H: Middleware<S, GatewayContext>> HttpGateway<S, H> {
    /// 创建指定路由器表的Http路由服务
    pub fn with(tab: RouterTab<S, GatewayContext, H>) -> Self {
        HttpGateway {
            context: GatewayContext::new(),
            router_tab: tab,
        }
    }
}