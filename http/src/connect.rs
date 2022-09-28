use std::io::{Error, Result, ErrorKind};

use https::{status::StatusCode,
            header::{CONTENT_LENGTH, HeaderValue}};

use tcp::{Socket, SocketHandle, SocketEvent,
          utils::{SocketContext, Ready}};

use crate::{service::{ServiceFactory, HttpService},
            request::HttpRequest,
            packet::DEFAULT_READ_READY_HTTP_REQUEST_BYTE_LEN};
use crate::response::HttpResponse;

/*
* Http连接
*/
pub struct HttpConnect<S: Socket, HS: HttpService<S>> {
    handle:         SocketHandle<S>,        //当前连接的Tcp连接句柄
    service:        HS,                     //当前连接的服务
    keep_alive:     usize,                  //连接保持时
}

unsafe impl<S: Socket, HS: HttpService<S, >> Send for HttpConnect<S, HS> {}
unsafe impl<S: Socket, HS: HttpService<S>> Sync for HttpConnect<S, HS> {}

/*
* Http连接同步方法
*/
impl<S: Socket, HS: HttpService<S>> HttpConnect<S, HS> {
    /// 构建指定Tcp连接句柄、异步任务等待队列、Http服务和Http连接保持时长的Http连接
    pub fn new(handle: SocketHandle<S>,
               service: HS,
               keep_alive: usize) -> Self {
        HttpConnect {
            handle,
            service,
            keep_alive,
        }
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
impl<S: Socket, HS: HttpService<S>> HttpConnect<S, HS> {
    /// 运行连接上的服务
    pub async fn run_service(&mut self,
                             req: HttpRequest<S>) {
        self.update_timeout(); //在调用服务前，更新当前Http连接的超时时长
        match self.service.call(req).await {
            Err(e) => {
                //服务调用异常
                let resp = HttpResponse::empty();
                self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e.into());
            },
            Ok(resp) => {
                //服务调用完成，则序列化响应，并回应本次Http请求
                let buf: Vec<u8> = resp.into();
                if let Err(e) = self.reply(buf) {
                    //回应错误，则立即抛出回应异常
                    let resp = HttpResponse::empty();
                    self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e);
                }
            },
        }
    }
}