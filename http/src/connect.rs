use std::io::{Error, Result, ErrorKind};

use https::{status::StatusCode,
            header::{CONTENT_LENGTH, HeaderValue}};

use tcp::{driver::{Socket, SocketHandle, AsyncIOWait},
          buffer_pool::WriteBuffer,
          util::{SocketContext, SocketEvent}};

use crate::{service::{ServiceFactory, HttpService},
            request::HttpRequest,
            packet::DEFAULT_READ_READY_HTTP_REQUEST_BYTE_LEN,
            util::{HttpSender, HttpReceiver, channel}};
use crate::response::HttpResponse;

/*
* Http连接
*/
pub struct HttpConnect<S: Socket, W: AsyncIOWait, HS: HttpService<S, W>> {
    handle:     SocketHandle<S>,        //当前连接的Tcp连接句柄
    waits:      W,                      //异步任务等待队列
    service:    HS,                     //当前连接的服务
    context:    SocketContext,          //连接上下文
}

unsafe impl<S: Socket, W: AsyncIOWait, HS: HttpService<S, W>> Send for HttpConnect<S, W, HS> {}
unsafe impl<S: Socket, W: AsyncIOWait, HS: HttpService<S, W>> Sync for HttpConnect<S, W, HS> {}

/*
* Http连接同步方法
*/
impl<S: Socket, W: AsyncIOWait, HS: HttpService<S, W>> HttpConnect<S, W, HS> {
    //构建指定Tcp连接句柄、异步任务等待队列和Http服务的Http连接
    pub fn new(handle: SocketHandle<S>, waits: W, service: HS) -> Self {
        HttpConnect {
            handle,
            waits,
            service,
            context: SocketContext::empty(),
        }
    }

    //分配一个用于发送的写缓冲区
    pub fn alloc(&self) -> Option<WriteBuffer> {
        match self.handle.alloc() {
            Ok(None) => None,
            Ok(buffer) => buffer,
            _ => None,
        }
    }

    //异步回应指定的Http响应
    pub fn reply(&self, buf: WriteBuffer) -> Result<()> {
        if let Some(handle) = buf.finish() {
            return self.handle.write_ready(handle);
        }

        Err(Error::new(ErrorKind::InvalidData, "invalid response"))
    }

    //获取Http会话上下文的只读引用
    pub fn get_context(&self) -> &SocketContext {
        &self.context
    }

    //获取Http会话上下文的可写引用
    pub fn get_context_mut(&mut self) -> &mut SocketContext {
        &mut self.context
    }

    //抛出服务器端指定错误，将通过本次Http请求的响应返回给客户端，并关闭当前Http连接
    pub fn throw(&self, mut resp: HttpResponse<S, W>, mut error: StatusCode, reason: Error) -> Result<()> {
        if !error.is_server_error() {
            //不是服务器端错误，则强制为服务器端未知错误
            error = StatusCode::INTERNAL_SERVER_ERROR;
        }

        if let Ok(Some(mut buf)) = self.handle.alloc() {
            //设置错误原因
            let body = reason.to_string().into_bytes();
            let len = body.len();
            buf.get_iolist_mut().push_back(body.into());

            //设置启始行和响应头
            resp.header(CONTENT_LENGTH.as_str(), len.to_string().as_str());
            resp.status(error.as_u16());
            let vec: Vec<u8> = resp.into();
            buf.get_iolist_mut().push_front(vec.into());

            //回应本次Http请求
            self.reply(buf);
        }

        //关闭当前Http连接
        self.close(Err(reason))
    }

    //关闭当前Http连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        //TODO 需要处理Http连接关闭时的相关问题...
        self.handle.close(reason)
    }
}

/*
* Http连接异步方法
*/
impl<S: Socket, W: AsyncIOWait, HS: HttpService<S, W>> HttpConnect<S, W, HS> {
    //运行连接上的服务
    pub async fn run_service(&mut self, req: HttpRequest<S, W>) {
        match self.service.call(req).await {
            Err(e) => {
                //服务调用异常
                let resp = HttpResponse::empty(self.handle.clone(), self.waits.clone());
                self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e.into());
            },
            Ok(resp) => {
                //服务调用完成
                if let Ok(Some(mut buf)) = self.handle.alloc() {
                    //序列化响应
                    let vec: Vec<u8> = resp.into();
                    buf.get_iolist_mut().push_back(vec.into());

                    //回应本次Http请求
                    if let Err(e) = self.reply(buf) {
                        //回应错误，则立即抛出回应异常
                        let resp = HttpResponse::empty(self.handle.clone(), self.waits.clone());
                        self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e);
                    } else {
                        //回应成功，则继续读当前Http连接的后续请求
                        if let Err(e) = self.handle.read_ready(DEFAULT_READ_READY_HTTP_REQUEST_BYTE_LEN) {
                            //继续读失败，则立即关闭Http连接
                            let resp = HttpResponse::empty(self.handle.clone(), self.waits.clone());
                            self.throw(resp, StatusCode::INTERNAL_SERVER_ERROR, e);
                        }
                    }
                }
            },
        }
    }
}