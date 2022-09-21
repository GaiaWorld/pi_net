use std::rc::Rc;
use std::pin::Pin;
use std::io::Error;
use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::error::Error as StdError;
use std::task::{Context, Poll, Waker};

use mio::Token;
use futures::future::{FutureExt, LocalBoxFuture};

use tcp::Socket;

use crate::{request::HttpRequest, response::HttpResponse};

///
/// Http服务工厂
///
pub trait ServiceFactory<S: Socket>: Clone + Send + Sync + 'static {
    type Service: HttpService<S>;

    //构建Http服务
    fn new_service(&self) -> Self::Service;
}

///
/// Http服务
///
pub trait HttpService<S: Socket>: Send + Sync + 'static {
    type Error:     Into<Error> + Send;

    //调用服务
    fn call(&mut self, req: HttpRequest<S>)
        -> LocalBoxFuture<'static, Result<HttpResponse, Self::Error>>;
}

///
/// 调用阻塞函数，返回Future
///
pub fn block_call<S, F, Error>(fun: F,
                               req: HttpRequest<S>)
    -> LocalBoxFuture<'static, Result<HttpResponse, Error>>
    where S: Socket,
          F: FnOnce(HttpRequest<S>) -> Result<HttpResponse, Error> + Send + 'static {
    let future = async move {
        fun(req)
    };
    future.boxed_local()
}