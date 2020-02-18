use std::rc::Rc;
use std::pin::Pin;
use std::io::Error;
use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::error::Error as StdError;
use std::task::{Context, Poll, Waker};

use mio::Token;
use futures::future::{FutureExt, BoxFuture};

use tcp::driver::{Socket, AsyncIOWait};

use crate::{request::HttpRequest, response::HttpResponse};

/*
* Http服务工厂
*/
pub trait ServiceFactory<S: Socket, W: AsyncIOWait>: Clone + Send + Sync + 'static {
    type Service: HttpService<S, W>;

    //构建Http服务
    fn new_service(&self) -> Self::Service;
}

/*
* Http服务
*/
pub trait HttpService<S: Socket, W: AsyncIOWait>: Send + Sync + 'static {
    type Error:     Into<Error> + Send;
    type Future:    Future<Output = Result<HttpResponse<S, W>, Self::Error>> + Send;

    //调用服务
    fn call(&mut self, req: HttpRequest<S, W>) -> Self::Future;
}

/*
* 调用阻塞函数，返回Future
*/
pub fn block_call<S, W, F, Error>(fun: F, req: HttpRequest<S, W>) -> BoxFuture<'static, Result<HttpResponse<S, W>, Error>>
    where S: Socket,
          W: AsyncIOWait,
          F: FnOnce(HttpRequest<S, W>) -> Result<HttpResponse<S, W>, Error> + Send + 'static {
    let future = async move {
        fun(req)
    };
    future.boxed()
}