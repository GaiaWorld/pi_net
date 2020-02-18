use std::pin::Pin;
use std::sync::Arc;
use std::future::Future;
use std::result::Result as GenResult;
use std::task::{Context, Poll, Waker};
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::Buf;
use futures::{future::{FutureExt, BoxFuture}};
use crossbeam_channel::{Sender, Receiver, bounded};

use tcp::driver::{Socket, SocketHandle, AsyncIOWait, PendSocket};

/*
* 默认支持的Http协议版本号
*/
pub const DEFAULT_SUPPORT_HTTP_VERSION: u8 = 1;

/*
* 默认支持的Http协议版本号字符串
*/
pub const DEFAULT_SUPPORT_HTTP_VERSION_STR: &str = "1.1";

/*
* Http请求的默认Scheme
*/
pub const DEFAULT_HTTP_SCHEME: &str = "http";
pub const DEFAULT_HTTPS_SCHEME: &str = "https";

/*
* Http请求的默认端口
*/
pub const DEFAULT_HTTP_PORT: u16 = 80;
pub const DEFAULT_HTTPS_PORT: u16 = 443;

/*
* Http连接异步发送器
*/
pub struct HttpSender<S: Socket, T: Send + Sync + 'static> {
    closed: Arc<AtomicBool>,    //是否已关闭当前Http连接异步通道
    status: Arc<AtomicBool>,    //当前Http连接任务执行状态
    handle: SocketHandle<S>,    //Http连接的Tcp连接句柄
    sender: Sender<Option<T>>,  //内部发送器
}

unsafe impl<S: Socket, T: Send + Sync + 'static> Send for HttpSender<S, T> {}
unsafe impl<S: Socket, T: Send + Sync + 'static> Sync for HttpSender<S, T> {}

impl<S: Socket, T: Send + Sync + 'static> Clone for HttpSender<S, T> {
    fn clone(&self) -> Self {
        HttpSender {
            closed: self.closed.clone(),
            status: self.status.clone(),
            handle: self.handle.clone(),
            sender: self.sender.clone(),
        }
    }
}

impl<S: Socket, T: Send + Sync + 'static> HttpSender<S, T> {
    //异步发送消息，无论当前Http连接是否挂起，都会发送消息
    //如果对应的Http连接已挂起，则会在发送成功后唤醒对应的Http连接
    //发送空消息，表示消息发送结束
    pub fn send(&self, msg: Option<T>) -> Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            //通道已关闭
            return Err(Error::new(ErrorKind::BrokenPipe, "http channel closed"));
        }

        match self.sender.send(msg) {
            Err(e) => {
                //发送失败，则不会唤醒
                Err(Error::new(ErrorKind::InvalidInput, e))
            },
            Ok(_) => {
                //发送成功
                if self.status.compare_and_swap(true, false, Ordering::SeqCst) {
                    //已挂起，则唤醒已挂起的对应Http连接
                    return self.handle.wake();
                }

                Ok(())
            }
        }
    }
}

/*
* Http连接异步接收结果
*/
pub enum HttpRecvResult<T> {
    Err(Error), //接收错误
    Ok(T),      //接收成功
    Fin(T),     //接收完成
}

/*
* Http连接异步接收器
*/
pub struct HttpReceiver<S: Socket, W: AsyncIOWait, T: Send + Sync + 'static> {
    closed:     Arc<AtomicBool>,        //是否已关闭当前Http连接异步通道
    status:     Arc<AtomicBool>,        //当前Http连接任务执行状态
    handle:     SocketHandle<S>,        //Http连接的Tcp连接句柄
    waits:      W,                      //异步任务等待队列
    receiver:   Receiver<Option<T>>,    //内部接收器
}

unsafe impl<S: Socket, W: AsyncIOWait, T: Send + Sync + 'static> Send for HttpReceiver<S, W, T> {}
unsafe impl<S: Socket, W: AsyncIOWait, T: Send + Sync + 'static> Sync for HttpReceiver<S, W, T> {}

impl<S: Socket, W: AsyncIOWait, T: Send + Sync + 'static> HttpReceiver<S, W, T> {
    //异步接收消息
    pub async fn recv(&self) -> HttpRecvResult<Vec<T>> {
        if self.closed.load(Ordering::Relaxed) {
            //通道已关闭
            return HttpRecvResult::Err(Error::new(ErrorKind::BrokenPipe, "http channel closed"));
        }

        let mut buf = Vec::new();

        //尝试接收消息
        for msg in self.receiver.try_iter() {
            //接收到消息
            if let Some(data) = msg {
                //消息不为空，则加入消息缓冲
                buf.push(data);
                continue;
            }

            //消息为空，则立即关闭当前通道，并返回消息缓冲
            self.closed.store(true, Ordering::Relaxed);
            return HttpRecvResult::Fin(buf);
        }

        //未接收到消息或已接收到部分消息，但未关闭通道，则挂起当前Http连接，等待接收后续消息
        if self.status.compare_and_swap(false, true, Ordering::SeqCst) {
            //已挂起，则立即返回错误原因
            HttpRecvResult::Err(Error::new(ErrorKind::WouldBlock, "receiveing"))
        } else {
            //未挂起，则挂起
            PendSocket::pending(self.handle.get_token().clone(), self.waits.clone()).await;

            //唤醒后，接收后续消息
            for msg in self.receiver.try_iter() {
                //接收到后续消息
                if let Some(data) = msg {
                    //消息不为空，则加入消息缓冲
                    buf.push(data);
                    continue;
                }

                //消息为空，则立即关闭当前通道，并返回消息缓冲
                self.closed.store(true, Ordering::Relaxed);
                return HttpRecvResult::Fin(buf);
            }

            //返回消息缓冲
            HttpRecvResult::Ok(buf)
        }
    }
}

/*
* 创建指定缓冲区长度的Http连接异步通道，缓冲区长度为0表示发送会等待对端接收后完成
*/
pub fn channel<S, W, T: Send + Sync + 'static>(handle: SocketHandle<S>, waits: W, size: usize) -> (HttpSender<S, T>, HttpReceiver<S, W, T>)
    where S: Socket, W: AsyncIOWait {
    let is_closed = Arc::new(AtomicBool::new(false));
    let status = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = bounded(size);

    (HttpSender {
        closed: is_closed.clone(),
        status: status.clone(),
        handle: handle.clone(),
        sender,
    },
     HttpReceiver {
         closed: is_closed,
         status,
         handle,
         waits,
         receiver,
    })
}