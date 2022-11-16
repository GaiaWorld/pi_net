use std::fs::File;
use std::pin::Pin;
use std::sync::Arc;
use std::path::Path;
use std::cell::RefCell;
use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::sync::atomic::{AtomicU8, Ordering};
use std::io::{BufReader, Result, Error, ErrorKind};

use pi_async::{lock::spin_lock::SpinLock,
               rt::serial::AsyncWaitResult};
use crossbeam_channel::Sender;
use flume::Sender as AsyncSender;
use futures::future::{FutureExt, LocalBoxFuture};
use quinn_proto::{Connection, ConnectionEvent, ConnectionHandle, StreamId};
use rustls::{Certificate, PrivateKey};

use udp::{Socket,
          connect::UdpSocket};

use crate::{SocketHandle,
            connect::QuicSocket};
use crate::client::QuicClient;

///
/// 连接令牌
///
#[derive(Debug, Clone, Copy)]
pub struct Token(pub u64);

unsafe impl Send for Token {}
unsafe impl Sync for Token {}

///
/// Quic连接状态
///
#[derive(Debug, Clone, Copy)]
pub enum QuicSocketStatus {
    UdpAccepted = 0,    //Udp连接已建立
    Handshaking,        //正在进行Quic握手
    Connecting,         //Quic连接已建立，正在打开初始化流
    Connected,          //Quic连接已建立，并打开初始化流
    Closeing,           //正在关闭连接
    Disconnect,         //连接已关闭
}

impl From<u8> for QuicSocketStatus {
    fn from(src: u8) -> Self {
        match src {
            0 => QuicSocketStatus::UdpAccepted,
            1 => QuicSocketStatus::Handshaking,
            2 => QuicSocketStatus::Connecting,
            3 => QuicSocketStatus::Connected,
            4 => QuicSocketStatus::Closeing,
            _ => QuicSocketStatus::Disconnect,
        }
    }
}

impl From<QuicSocketStatus> for u8 {
    fn from(src: QuicSocketStatus) -> Self {
        match src {
            QuicSocketStatus::UdpAccepted => 0,
            QuicSocketStatus::Handshaking => 1,
            QuicSocketStatus::Connecting => 2,
            QuicSocketStatus::Connected => 3,
            QuicSocketStatus::Closeing => 4,
            QuicSocketStatus::Disconnect => 5,
        }
    }
}

///
/// Quic连接就绪状态
///
#[derive(Debug, Clone, Copy)]
pub enum QuicSocketReady {
    Readable = 1,   //可读
    Writable = 2,   //可写
    ReadWrite = 3,  //可读可写
}

impl From<u8> for QuicSocketReady {
    fn from(src: u8) -> Self {
        match src {
            1 => QuicSocketReady::Readable,
            2 => QuicSocketReady::Writable,
            3 => QuicSocketReady::ReadWrite,
            _ => panic!("Invalid number, {}", src),
        }
    }
}

impl From<QuicSocketReady> for u8 {
    fn from(src: QuicSocketReady) -> Self {
        match src {
            QuicSocketReady::Readable => 1,
            QuicSocketReady::Writable => 2,
            QuicSocketReady::ReadWrite => 3,
        }
    }
}

impl QuicSocketReady {
    /// 判断是否读就绪
    pub fn is_readable(&self) -> bool {
        let x: u8 = self.clone().into();
        let y: u8 = QuicSocketReady::Readable.into();
        (x & y) != 0
    }

    /// 判断是否写就绪
    pub fn is_writable(&self) -> bool {
        let x: u8 = self.clone().into();
        let y: u8 = QuicSocketReady::Writable.into();
        (x & y) != 0
    }

    /// 增加Quic连接就绪状态
    pub fn add(self, other: Self) -> Self {
        let x: u8 = self.into();
        let y: u8 = other.into();
        (x | y).into()
    }

    /// 移除Quic连接就绪状态
    pub fn remove(self, other: Self) -> Self {
        let x: u8 = self.into();
        let y: u8 = other.into();
        (x & !y).into()
    }
}

/// Quic连接关闭事件
pub enum QuicCloseEvent {
    CloseStream(ConnectionHandle, Option<StreamId>, u32),   //关闭流
    CloseConnect(ConnectionHandle),                         //关闭连接
}

// 加载指定Pem文件的证书
pub fn load_certs<P: AsRef<Path>>(file_path: P) -> Result<Vec<Certificate>> {
    let certfile = match File::open(&file_path) {
        Err(e) => {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Open certs failed, path: {:?}, reason: {:?}",
                                          file_path.as_ref(), e)));
        },
        Ok(file) => {
            file
        },
    };

    let mut reader = BufReader::new(certfile);
    match rustls_pemfile::certs(&mut reader) {
        Err(e) => {
            Err(Error::new(ErrorKind::Other,
                           format!("Load certs failed, path: {:?}, reason: {:?}",
                                   file_path.as_ref(), e)))
        },
        Ok(certs) => {
            Ok(certs.iter()
                .map(|v| {
                    Certificate(v.clone())
                })
                .collect())
        },
    }
}

// 加载指定Pem文件的私钥
pub fn load_private_key<P: AsRef<Path>>(file_path: P) -> Result<PrivateKey> {
    let keyfile = match File::open(&file_path) {
        Err(e) => {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Open private key failed, path: {:?}, reason: {:?}",
                                          file_path.as_ref(),
                                          e)));
        },
        Ok(file) => {
            file
        },
    };

    let mut reader = BufReader::new(keyfile);
    loop {
        match rustls_pemfile::read_one(&mut reader) {
            Ok(Some(rustls_pemfile::Item::RSAKey(key))) => return Ok(PrivateKey(key)),
            Ok(Some(rustls_pemfile::Item::PKCS8Key(key))) => return Ok(PrivateKey(key)),
            Ok(Some(rustls_pemfile::Item::ECKey(key))) => return Ok(PrivateKey(key)),
            Ok(None) => break,
            Err(e) => {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Load private key failed, path: {:?}, reason: {:?}",
                                              file_path.as_ref(),
                                              e)));
            },
            _ => {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Load private key failed, path: {:?}, reason: cannot parse private key .pem file",
                                              file_path.as_ref())))
            },
        }
    }

    Err(Error::new(ErrorKind::Other,
                   format!("no keys found in {:?} (encrypted keys not supported)",
                           file_path.as_ref())))
}

/// 线程安全的异步休眠对象
pub struct Hibernate<S: Socket>(Arc<InnerHibernate<S>>);

impl<S: Socket> Clone for Hibernate<S> {
    fn clone(&self) -> Self {
        Hibernate(self.0.clone())
    }
}

impl<S: Socket> Future for Hibernate<S> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut locked = self.0.waker.lock(); //由锁保护异步休眠对象的唤醒

        //获取唤醒的结果值
        let result = self
            .0
            .result
            .0
            .borrow_mut()
            .take();

        if let Some(result) = result {
            //已设置唤醒的结果值，则立即返回就绪
            self
                .0
                .handle
                .set_ready(self.0.ready); //在唤醒后重置当前流感兴趣的事件
            self.0.handle.run_hibernated_tasks(); //立即异步运行连接休眠时的所有任务

            Poll::Ready(result)
        } else {
            //需要休眠，则立即返回挂起
            self
                .0
                .handle
                .set_ready(self.0.ready); //在休眠时重置当前流感兴趣的事件为只写

            if self.0.handle.set_hibernate(self.clone()) {
                //设置当前连接的休眠对象成功，则设置当前休眠的唤醒器
                *locked = Some(cx.waker().clone());
            } else {
                //当前连接已设置休眠对象，则将当前休眠对象写入当前连接的休眠对象的唤醒器
                self
                    .0
                    .handle
                    .set_hibernate_wakers(cx.waker().clone());
                self.0.waker_status.store(1, Ordering::Relaxed); //设置唤醒器状态为被阻塞
            }

            Poll::Pending
        }
    }
}

impl<S: Socket> Hibernate<S> {
    /// 构建一个异步休眠对象
    pub fn new(handle: SocketHandle<S>,
               ready: QuicSocketReady) -> Self {
        let inner = InnerHibernate {
            handle,
            ready,
            waker: SpinLock::new(None),
            waker_status: AtomicU8::new(0),
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        };

        Hibernate(Arc::new(inner))
    }

    /// 线程安全的设置结果值，并唤醒正在休眠的异步休眠对象，如果当前异步休眠对象未休眠则忽略，唤醒成功返回真
    /// 唤醒过程可能会被阻塞，这不会导致线程阻塞而是返回假，调用者可以继续尝试唤醒，直到返回真
    pub(crate) fn wakeup(&self, result: Result<()>) -> bool {
        let mut locked = self
            .0
            .waker
            .lock(); //由锁保护异步休眠对象的唤醒

        if let Some(waker) = locked.take() {
            //当前异步休眠对象正在休眠中，则设置结果值，并立即唤醒当前异步休眠对象
            if self.0.waker_status.load(Ordering::Relaxed) > 0 {
                //唤醒过程被阻塞，则立即返回假，需要调用者继续尝试唤醒
                self.0.waker_status.store(0, Ordering::Relaxed); //设置唤醒器状态为可以完成唤醒
                waker.wake();

                false
            } else {
                //唤醒过程完成，则立即返回真
                *self.0.result.0.borrow_mut() = Some(result);
                waker.wake();

                true
            }
        } else {
            if self.0.result.0.borrow().is_none() {
                //当前异步休眠对象还未休眠，则立即返回假，需要调用者继续尝试唤醒
                false
            } else {
                //当前异步休眠对象已被唤醒，则立即返回真
                true
            }
        }
    }
}

// 内部线程安全的异步休眠对象
struct InnerHibernate<S: Socket> {
    handle:         SocketHandle<S>,            //当前Quic连接的句柄
    ready:          QuicSocketReady,            //唤醒后感兴趣的事件
    waker:          SpinLock<Option<Waker>>,    //休眠唤醒器
    waker_status:   AtomicU8,                   //唤醒状态
    result:         AsyncWaitResult<()>,        //结果值
}