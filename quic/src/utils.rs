use std::ptr;
use std::fs::File;
use std::pin::Pin;
use std::sync::Arc;
use std::path::Path;
use std::cell::RefCell;
use std::future::Future;
use std::result::Result as GenResult;
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

use crate::{SocketHandle,
            connect::QuicSocket,
            client::QuicClient};

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
pub fn load_certs_file<P: AsRef<Path>>(file_path: P) -> Result<Vec<Certificate>> {
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

// 加载指定Pem证书数据
#[inline]
pub fn load_cert(bin: Vec<u8>) -> Certificate {
    Certificate(bin)
}

// 加载指定Pem文件的私钥
pub fn load_key_file<P: AsRef<Path>>(file_path: P) -> Result<PrivateKey> {
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

// 加载指定Pem私钥数据
#[inline]
pub fn load_key(bin: Vec<u8>) -> PrivateKey {
    PrivateKey(bin)
}

/// 线程安全的异步休眠对象
pub struct Hibernate(Arc<InnerHibernate>);

impl Clone for Hibernate {
    fn clone(&self) -> Self {
        Hibernate(self.0.clone())
    }
}

impl Future for Hibernate {
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
                .set_ready(self.0.stream_id,
                           self.0.ready); //在唤醒后重置当前流感兴趣的事件
            self.0.handle.run_hibernated_tasks(); //立即异步运行连接休眠时的所有任务

            Poll::Ready(result)
        } else {
            //需要休眠，则立即返回挂起
            self
                .0
                .handle
                .set_ready(self.0.stream_id,
                           self.0.ready); //在休眠时重置当前流感兴趣的事件为只写

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

impl Hibernate {
    /// 构建一个异步休眠对象
    pub fn new(handle: SocketHandle,
               stream_id: StreamId,
               ready: QuicSocketReady) -> Self {
        let inner = InnerHibernate {
            handle,
            stream_id,
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
struct InnerHibernate {
    handle:         SocketHandle,               //当前Quic连接的句柄
    stream_id:      StreamId,                   //当前Quicl连接的指定流
    ready:          QuicSocketReady,            //唤醒后感兴趣的事件
    waker:          SpinLock<Option<Waker>>,    //休眠唤醒器
    waker_status:   AtomicU8,                   //唤醒状态
    result:         AsyncWaitResult<()>,        //结果值
}

///
/// Quic客户端定时器事件
///
#[derive(Debug)]
pub enum QuicClientTimerEvent {
    UdpConnect(usize),  //建立Udp连接
    QuicConnect(usize), //建立Quic连接
}

///
/// 上下文句柄
///
pub struct ContextHandle<T: 'static>(Option<Arc<T>>);

unsafe impl<T: 'static> Send for ContextHandle<T> {}

impl<T: 'static> Drop for ContextHandle<T> {
    fn drop(&mut self) {
        if let Some(shared) = self.0.take() {
            //当前上下文指针存在，则释放
            Arc::into_raw(shared);
        }
    }
}

impl<T: 'static> ContextHandle<T> {
    /// 获取上下文只读引用
    pub fn as_ref(&self) -> &T {
        self.0.as_ref().unwrap().as_ref()
    }

    /// 获取上下文可写引用
    pub fn as_mut(&mut self) -> Option<&mut T> {
        if let Some(shared) = self.0.as_mut() {
            return Arc::get_mut(shared);
        }

        None
    }
}

///
/// 通用上下文
/// 注意，设置上下文后，需要移除当前上下文，上下文才会自动释放
///
pub struct SocketContext {
    inner: *const (), //内部上下文
}

unsafe impl Send for SocketContext {}

impl SocketContext {
    /// 创建空的上下文
    pub fn empty() -> Self {
        SocketContext {
            inner: ptr::null(),
        }
    }

    /// 判断上下文是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.is_null()
    }

    /// 获取上下文的句柄
    pub fn get<T>(&self) -> Option<ContextHandle<T>> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { ContextHandle(Some(Arc::from_raw(self.inner as *const T))) })
    }

    /// 设置上下文，如果当前上下文不为空，则设置失败
    pub fn set<T>(&mut self, context: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner = Arc::into_raw(Arc::new(context)) as *const T as *const ();
        true
    }

    /// 移除上下文，如果当前还有未释放的上下文句柄，则返回移除错误，如果当前有上下文，则返回被移除的上下文，否则返回空
    pub fn remove<T>(&mut self) -> GenResult<Option<T>, &str> {
        if self.is_empty() {
            return Ok(None);
        }

        let inner = unsafe { Arc::from_raw(self.inner as *const T) };
        if Arc::strong_count(&inner) > 1 {
            Arc::into_raw(inner); //释放临时共享指针
            Err("Remove context failed, reason: context shared exist")
        } else {
            match Arc::try_unwrap(inner) {
                Err(inner) => {
                    Arc::into_raw(inner); //释放临时共享指针
                    Err("Remove context failed, reason: invalid shared")
                },
                Ok(context) => {
                    //将当前内部上下文设置为空，并返回上下文
                    self.inner = ptr::null();
                    Ok(Some(context))
                },
            }
        }
    }
}