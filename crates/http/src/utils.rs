use std::thread;
use std::pin::Pin;
use std::sync::Arc;
use std::path::PathBuf;
use std::time::Duration;
use std::future::Future;
use std::result::Result as GenResult;
use std::task::{Context, Poll, Waker};
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::Buf;
use futures::{future::{FutureExt, LocalBoxFuture}};
use flume::{Sender, Receiver, TrySendError, bounded};

use tcp::{Socket, SocketHandle,
          utils::Ready};

///
/// 默认支持的Http协议版本号
///
pub const DEFAULT_SUPPORT_HTTP_VERSION: u8 = 1;

///
/// 默认支持的Http协议版本号字符串
///
pub const DEFAULT_SUPPORT_HTTP_VERSION_STR: &str = "1.1";

///
/// Http请求的默认Scheme
///
pub const DEFAULT_HTTP_SCHEME: &str = "http";
pub const DEFAULT_HTTPS_SCHEME: &str = "https";

///
/// Http请求的默认端口
///
pub const DEFAULT_HTTP_PORT: u16 = 80;
pub const DEFAULT_HTTPS_PORT: u16 = 443;

///
/// 默认块大小，单位字节
///
pub const DEAFULT_CHUNK_SIZE: u64 = 512 * 1024;

///
/// Http连接异步发送器
///
pub struct HttpSender<T: Send + Sync + 'static> {
    sender: Sender<Option<T>>,  //内部发送器
}

unsafe impl<T: Send + Sync + 'static> Send for HttpSender<T> {}
unsafe impl<T: Send + Sync + 'static> Sync for HttpSender<T> {}

impl<T: Send + Sync + 'static> Clone for HttpSender<T> {
    fn clone(&self) -> Self {
        HttpSender {
            sender: self.sender.clone(),
        }
    }
}

impl<T: Send + Sync + 'static> HttpSender<T> {
    /// 异步发送消息，无论当前Http连接是否挂起，都会发送消息
    /// 如果对应的Http连接已挂起，则会在发送成功后唤醒对应的Http连接
    /// 发送空消息，表示消息发送结束
    pub async fn send(&self, msg: Option<T>) -> Result<()> {
        if let Err(e) = self.sender.send_async(msg).await {
            //发送失败
            Err(Error::new(ErrorKind::InvalidInput,
                           format!("Http channel send failed, reason: {:?}",
                                   e)))
        } else {
            //发送成功
            Ok(())
        }
    }

    /// 同步非阻塞地尝试发送消息。
    ///
    /// 本方法用于 `ResponseHandler::try_*` 和 `pi_http::sse::SseSender::try_*`。
    /// 它不会阻塞当前线程，也不会启动异步运行时；当内部 bounded channel 已满时，
    /// 立即返回 `ErrorKind::WouldBlock`。发送空消息表示消息发送结束。
    ///
    /// 时间复杂度为 `O(1)`；空间复杂度为 `O(1)`，不额外复制 `msg`。本方法有副作用：
    /// 成功时会把消息放入 HTTP 响应体队列。它不是幂等操作，重复调用会重复发送。
    /// 线程安全和异步安全由 flume sender 保证。
    ///
    /// 测试入口：生产侧 `ResponseHandler::try_write`、`SseSender::try_send` 和
    /// `SseHub::try_send_to_id` 间接覆盖该路径；真实网络 SSE 测试验证非阻塞写入后的
    /// 响应体最终能被 HTTP/1.1 chunked 流发送。
    pub fn try_send(&self, msg: Option<T>) -> Result<()> {
        match self.sender.try_send(msg) {
            Ok(_) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err(Error::new(ErrorKind::WouldBlock,
                               "Http channel try send failed, reason: channel full"))
            },
            Err(TrySendError::Disconnected(_)) => {
                Err(Error::new(ErrorKind::BrokenPipe,
                               "Http channel try send failed, reason: channel disconnected"))
            },
        }
    }
}

///
/// Http连接异步接收结果
///
pub enum HttpRecvResult<T> {
    Err(Error), //接收错误
    Ok(T),      //接收成功
    Fin(T),     //接收完成
}

///
/// Http连接异步接收器
///
pub struct HttpReceiver<T: Send + Sync + 'static> {
    receiver:   Receiver<Option<T>>,    //内部接收器
}

unsafe impl<T: Send + Sync + 'static> Send for HttpReceiver<T> {}
unsafe impl<T: Send + Sync + 'static> Sync for HttpReceiver<T> {}

impl<T: Send + Sync + 'static> HttpReceiver<T> {
    /// 异步接收消息
    pub async fn recv(&self) -> HttpRecvResult<Vec<T>> {
        let mut buf = Vec::new();
        loop {
            match self.receiver.recv_async().await {
                Err(e) => {
                    return HttpRecvResult::Err(Error::new(ErrorKind::BrokenPipe,
                                                          format!("Async recv http response failed, reason: {:?}",
                                                                  e)));
                },
                Ok(None) => {
                    //消息为空，则立即关闭当前通道，并返回消息缓冲
                    return HttpRecvResult::Fin(buf);
                },
                Ok(Some(data)) => {
                    //消息不为空，则加入消息缓冲
                    buf.push(data);
                },
            }
        }
    }

    /// 异步接收下个消息
    pub async fn next(&self) -> HttpRecvResult<Option<T>> {
        match self.receiver.recv_async().await {
            Err(e) => {
                HttpRecvResult::Err(Error::new(ErrorKind::BrokenPipe,
                                               format!("Async recv http response failed, reason: {:?}",
                                                       e)))
            },
            Ok(None) => {
                //消息为空，则立即关闭当前通道，并返回消息缓冲
                HttpRecvResult::Fin(None)
            },
            Ok(item) => {
                //消息不为空，则加入消息缓冲
                HttpRecvResult::Ok(item)
            },
        }
    }
}

///
/// 内容编码
///
#[derive(Debug, Clone)]
pub enum ContentEncode {
    Emtpy,          //无编码
    Deflate(u32),   //Deflate编码
    Gzip(u32),      //Gzip编码
    Br(u32),        //Br编码
}

///
/// 创建指定缓冲区长度的Http连接异步通道，缓冲区长度为0表示发送会等待对端接收后完成
///
pub fn channel<T>(size: usize) -> (HttpSender<T>, HttpReceiver<T>)
    where T: Send + Sync + 'static {
    let (sender, receiver) = bounded(size);

    (HttpSender {
        sender,
    },
     HttpReceiver {
         receiver,
    })
}

///
/// 路径修剪
///
pub fn trim_path<P: Into<PathBuf>>(path: P) -> Result<PathBuf> {
    let input: PathBuf = path.into();
    let mut path = PathBuf::new();
    for e in input.iter() {
        path = path.join(e);
    }

    Ok(path)
}