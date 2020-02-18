use std::sync::Arc;
use std::io::Error;
use std::collections::VecDeque;

use futures::future::{FutureExt, BoxFuture};
use https::{StatusCode, header::CONTENT_LENGTH};

use tcp::driver::{Socket, AsyncIOWait};

use crate::{request::HttpRequest, response::HttpResponse};

/*
* Http中间件处理结果
*/
pub enum MiddlewareResult<S: Socket, W: AsyncIOWait> {
    ContinueRequest(HttpRequest<S, W>),                         //继续请求中间件的处理
    ContinueResponse((HttpRequest<S, W>, HttpResponse<S, W>)),  //继续响应中间件的处理
    Break(HttpResponse<S, W>),                                  //退出请求或响应中间件的处理，并立即返回Http响应，退出会跳过剩余中间件的处理，由用户创建响应
    Finish((HttpRequest<S, W>, HttpResponse<S, W>)),            //完成请求或响应中间件的处理
    Throw(Error),                                               //中止请求或响应中间件的处理，并抛出错误，抛出错误会跳过剩余中间件的处理，但会根据错误自动创建响应
}

/*
* Http中间件
*/
pub trait Middleware<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static>: Send + Sync + 'static {
    //处理指定请求，需要继续处理请求，则返回Http请求，需要需要中止处理请求，则返回Http响应
    fn request<'a>(&'a self, context: &'a mut Context, req: HttpRequest<S, W>)
        -> BoxFuture<'a, MiddlewareResult<S, W>>;

    //处理指定响应
    fn response<'a>(&'a self, context: &'a mut Context, req: HttpRequest<S, W>, resp: HttpResponse<S, W>)
        -> BoxFuture<'a, MiddlewareResult<S, W>>;
}

/*
* Http中间件链
*/
pub struct MiddlewareChain<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static> {
    buf:    Option<VecDeque<Arc<dyn Middleware<S, W, Context>>>>,   //中间件缓冲
    chain:  Vec<Arc<dyn Middleware<S, W, Context>>>,                //处理链
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static> Middleware<S, W, Context> for Arc<MiddlewareChain<S, W, Context>> {
    fn request<'a>(&'a self, context: &'a mut Context, req: HttpRequest<S, W>)
               -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let chain = &self.chain;
        let future = async move {
            let mut request = req; //请求缓冲
            for middleware in chain {
                match middleware.request(context, request).await {
                    MiddlewareResult::ContinueRequest(req) => {
                        //继续下一个中间件的请求处理
                        request = req;
                    },
                    result@MiddlewareResult::Finish(_) => {
                        //完成了所有中间件的请求处理，则返回
                        return result;
                    },
                    result@MiddlewareResult::Break(_) => {
                        //退出请求处理，则返回
                        return result;
                    },
                    invalid_result => {
                        //无效的请求处理结果，则立即返回
                        return invalid_result;
                    }
                }
            }

            //所有请求中间件没有返回完成请求处理，则强制完成并返回
            let response = HttpResponse::empty(request.get_handle().clone(), request.get_waits().clone());
            MiddlewareResult::Finish((request, response))
        };
        future.boxed()
    }

    fn response<'a>(&'a self, context: &'a mut Context, req: HttpRequest<S, W>, resp: HttpResponse<S, W>)
                -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let chain = &self.chain;
        let future  = async move {
            let mut request = req; //请求缓冲
            let mut response = resp; //响应缓冲
            let mut middlewares = chain.to_vec();
            middlewares.reverse(); //以相反方向执行响应处理
            for middleware in middlewares {
                match middleware.response(context, request, response).await {
                    MiddlewareResult::ContinueResponse((req, resp)) => {
                        //继续下一个中间件的响应处理
                        request = req;
                        response = resp;
                    },
                    result@MiddlewareResult::Finish(_) => {
                        //完成了所有中间件的响应处理，则返回
                        return result;
                    },
                    result@MiddlewareResult::Break(_) => {
                        //退出响应处理，则返回
                        return result;
                    },
                    invalid_result => {
                        //无效的响应处理结果，则立即返回
                        return invalid_result;
                    },
                }
            }

            //所有响应中间件没有返回完成响应处理，则强制完成并返回
            MiddlewareResult::Finish((request, response))
        };
        future.boxed()
    }
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static> MiddlewareChain<S, W, Context> {
    //构建Http中间件链
    pub fn new() -> Self {
        MiddlewareChain {
            buf: Some(VecDeque::new()),
            chain: Vec::new(),
        }
    }

    //在链头增加中间件，靠前的中间件，将在处理请求时先执行，并在处理响应时后执行
    pub fn push_front(&mut self, ware: Arc<dyn Middleware<S, W, Context>>) {
        if let Some(buf) = &mut self.buf {
            buf.push_front(ware);
        }
    }

    //在链尾增加中间件，靠后的中间件，将在处理请求时后执行，并在处理响应时先执行
    pub fn push_back(&mut self, ware: Arc<dyn Middleware<S, W, Context>>) {
        if let Some(buf) = &mut self.buf {
            buf.push_back(ware);
        }
    }

    //完成中间件链
    pub fn finish(&mut self) {
        if let Some(buf) = self.buf.take() {
            self.chain = buf.into();
        }
    }
}

