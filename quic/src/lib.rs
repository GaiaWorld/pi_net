use std::ptr;
use std::rc::Rc;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use std::net::SocketAddr;
use std::cell::UnsafeCell;
use std::task::{ready, Waker};
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicU8, Ordering};

use futures::future::LocalBoxFuture;
use quinn_proto::{ConnectionHandle, ConnectionEvent, Dir, StreamId, Transmit};
use crossbeam_channel::Sender;
use crossbeam_utils::atomic::AtomicCell;
use bytes::BytesMut;
use pi_async::prelude::SpinLock;
use pi_async::rt::AsyncValueNonBlocking;
use pi_async::rt::serial_local_thread::LocalTaskRuntime;

use udp::SocketHandle as UdpSocketHandle;

pub mod acceptor;
pub mod connect;
pub mod connect_pool;
pub mod server;
pub mod client;
pub mod utils;

use crate::connect::QuicSocket;
use crate::utils::{QuicSocketStatus, QuicSocketReady, QuicCloseEvent, Hibernate, ContextHandle};

///
/// Quic连接异步服务
///
pub trait AsyncService: Send + Sync + 'static {
    /// 异步处理已连接
    fn handle_connected(&self,
                        handle: SocketHandle,
                        result: Result<()>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已打开扩展流
    fn handle_opened_expanding_stream(&self,
                                      handle: SocketHandle,
                                      stream_id: StreamId,
                                      stream_type: Dir,
                                      result: Result<()>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已读
    fn handle_readed(&self,
                     handle: SocketHandle,
                     stream_id: StreamId,
                     result: Result<usize>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已写
    fn handle_writed(&self,
                     handle: SocketHandle,
                     stream_id: StreamId,
                     result: Result<()>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已关闭
    fn handle_closed(&self,
                     handle: SocketHandle,
                     stream_id: Option<StreamId>,
                     code: u32,
                     result: Result<()>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已超时
    fn handle_timeouted(&self,
                        handle: SocketHandle,
                        result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()>;
}

///
/// Quic连接句柄
///
pub struct SocketHandle(Arc<InnerSocketHandle>);

unsafe impl Send for SocketHandle {}
unsafe impl Sync for SocketHandle {}

impl Clone for SocketHandle {
    fn clone(&self) -> Self {
        SocketHandle(self.0.clone())
    }
}

impl SocketHandle {
    /// 构建一个Quic连接句柄
    pub fn new(uid: usize,
               status: Arc<AtomicU8>,
               socket: Arc<UnsafeCell<QuicSocket>>,
               local: SocketAddr,
               remote: SocketAddr,
               sender: Sender<QuicEvent>) -> Self {
        let inner = InnerSocketHandle {
            uid,
            status,
            inner: socket,
            local: AtomicCell::new(local),
            remote,
            sender,
        };

        SocketHandle(Arc::new(inner))
    }

    /// 线程安全的异步广播指定负载
    pub fn broadcast(handles: &[SocketHandle],
                     payload: Vec<u8>) -> Result<()> {
        if handles.len() == 0 {
            //连接为空，则忽略
            return Ok(());
        }

        for handle in handles {
            for stream_id in handle.get_stream_ids() {
                handle.write_ready(stream_id, payload.clone())?;
            }
        }

        Ok(())
    }

    /// 获取连接唯一id
    pub fn get_uid(&self) -> usize {
        self.0.uid
    }

    /// 获取连接状态
    pub fn get_status(&self) -> QuicSocketStatus {
        self
            .0
            .status
            .load(Ordering::Acquire)
            .into()
    }

    /// 获取本地Udp连接地址
    pub fn get_local_udp(&self) -> &SocketAddr {
        unsafe {
            (&*self.0.inner.get())
                .get_udp_handle()
                .get_local()
        }
    }

    /// 获取远端Udp连接地址
    pub fn get_remote_udp(&self) -> Option<&SocketAddr> {
        unsafe {
            (&*self.0.inner.get())
                .get_udp_handle()
                .get_remote()
        }
    }

    /// 获取本地连接地址
    pub fn get_local(&self) -> SocketAddr {
        self
            .0
            .local
            .load()
    }

    /// 设置本地连接地址
    pub fn set_local(&self, local: SocketAddr) {
        self
            .0
            .local
            .store(local);
    }

    /// 获取远端连接地址
    pub fn get_remote(&self) -> SocketAddr {
        self.0.remote
    }

    /// 获取Quic连接对应的Udp连接的唯一id
    pub fn get_udp_uid(&self) -> usize {
        unsafe {
            (&*self.0.inner.get()).get_udp_handle().get_uid()
        }
    }

    /// 判断当前连接是否是客户端连接
    pub fn is_client(&self) -> bool {
        unsafe {
            (&*self.0.inner.get()).is_client()
        }
    }

    /// 获取内部连接句柄
    pub fn get_connection_handle(&self) -> &ConnectionHandle {
        unsafe {
            (&*self.0.inner.get()).get_connection_handle()
        }
    }

    /// 获取Quic连接会话的句柄
    pub fn get_session<T>(&self) -> Option<ContextHandle<T>> {
        unsafe {
            (&*(&*self.0.inner.get()).get_context().get()).get::<T>()
        }
    }

    /// 设置Quic连接会话的句柄
    pub fn set_session<T>(&self, context: T) {
        unsafe {
            (&mut *(&*self.0.inner.get()).get_context().get()).set::<T>(context);
        }
    }

    /// 移除Quic连接会话的句柄
    pub fn remove_session<T>(&self) -> Result<Option<T>> {
        unsafe {
            match (&mut *(&*self.0.inner.get()).get_context().get()).remove::<T>() {
                Err(e) => Err(Error::new(ErrorKind::Other, e)),
                Ok(r) => Ok(r)
            }
        }
    }

    /// 判断当前连接是否通过0rtt建立连接
    pub fn is_0rtt(&self) -> bool {
        unsafe {
            (&*self.0.inner.get()).is_0rtt()
        }
    }

    /// 判断当前连接是否已关闭
    pub fn is_closed(&self) -> bool {
        unsafe {
            (&*self.0.inner.get()).is_closed()
        }
    }

    /// 获取当前连接的延迟估计
    pub fn get_latency(&self) -> Duration {
        unsafe {
            (&*self.0.inner.get()).get_latency()
        }
    }

    /// 获取连接的主流唯一id
    pub fn get_main_stream_id(&self) -> Option<&StreamId> {
        unsafe {
            (&*self.0.inner.get()).get_main_stream_id()
        }
    }

    /// 打开连接的主流，主流一定是双向流
    pub(crate) fn open_main_streams(&self) -> Result<()> {
        unsafe {
            (&mut *self.0.inner.get()).open_main_streams()
        }
    }

    /// 打开连接的扩展流，可以指定流的类型
    pub async fn open_expanding_stream(&self, stream_type: Dir) -> Result<StreamId> {
        unsafe {
            (&*self.0.inner.get()).open_expanding_stream(stream_type).await
        }
    }

    /// 设置当前连接感兴趣的事件
    pub fn set_ready(&self,
                     stream_id: StreamId,
                     ready: QuicSocketReady) {
        unsafe {
            (&*self.0.inner.get()).set_ready(stream_id, ready);
        }
    }

    /// 获取连接的所有流唯一id
    pub fn get_stream_ids(&self) -> Vec<StreamId> {
        unsafe {
            (&*self.0.inner.get()).get_stream_ids()
        }
    }

    /// 通知连接的指定流读就绪
    pub fn read_ready(&self,
                      stream_id: &StreamId,
                      adjust: usize) -> GenResult<AsyncValueNonBlocking<usize>, usize> {
        unsafe {
            (&mut *self.0.inner.get()).read_ready(stream_id, adjust)
        }
    }

    /// 获取连接的输入缓冲区的剩余未读字节数
    pub fn read_buffer_remaining(&self, stream_id: &StreamId) -> Option<usize> {
        unsafe {
            (&*self.0.inner.get()).read_buffer_remaining(stream_id)
        }
    }

    /// 获取连接的输入缓冲区的只读引用
    pub fn get_read_buffer(&self,
                           stream_id: &StreamId) -> Option<Arc<SpinLock<Option<BytesMut>>>> {
        unsafe {
            (&*self.0.inner.get()).get_read_buffer(stream_id)
        }
    }

    /// 通知连接写就绪，可以开始发送指定的数据
    pub fn write_ready<B>(&self,
                          stream_id: StreamId,
                          buf: B) -> Result<()>
        where B: AsRef<[u8]> + 'static {
        unsafe {
            (&*self.0.inner.get()).write_ready(stream_id, buf)
        }
    }

    /// 设置连接的超时定时器，同时只允许设置一个定时器，新的定时器会覆盖未超时的旧定时器
    pub fn set_timeout(&self, timeout: usize, event: SocketEvent) {
        unsafe {
            (&*self.0.inner.get()).set_timeout(timeout, event);
        }
    }

    /// 取消连接的未超时超时定时器
    pub fn unset_timeout(&self) {
        unsafe {
            (&*self.0.inner.get()).unset_timeout();
        }
    }

    /// 开始执行连接休眠时加入的任务，当前任务执行完成后自动执行下一个任务，直到任务队列为空
    pub fn run_hibernated_tasks(&self) {
        unsafe {
            (&*self.0.inner.get()).run_hibernated_tasks();
        }
    }

    /// 获取当前连接的休眠对象，返回空表示连接已关闭
    pub fn hibernate(&self,
                     stream_id: StreamId,
                     ready: QuicSocketReady) -> Option<Hibernate> {
        unsafe {
            (&*self.0.inner.get()).hibernate(self.clone(),
                                             stream_id,
                                             ready)
        }
    }

    /// 设置当前连接的休眠对象，设置成功返回真
    pub fn set_hibernate(&self, hibernate: Hibernate) -> bool {
        unsafe {
            (&*self.0.inner.get()).set_hibernate(hibernate)
        }
    }

    /// 设置当前连接在休眠时挂起的其它休眠对象的唤醒器
    pub fn set_hibernate_wakers(&self, waker: Waker) {
        unsafe {
            (&*self.0.inner.get()).set_hibernate_wakers(waker);
        }
    }

    /// 非阻塞的唤醒被休眠的当前连接，如果当前连接未被休眠，则忽略
    /// 还会唤醒当前连接正在休眠时，当前连接的所有其它休眠对象的唤醒器
    /// 唤醒过程可能会被阻塞，这不会导致线程阻塞而是返回假，调用者可以继续尝试唤醒，直到返回真
    pub fn wakeup(&self, result: Result<()>) -> bool {
        unsafe {
            (&mut *self.0.inner.get()).wakeup(result)
        }
    }

    /// 线程安全的获取连接所在运行时
    pub fn get_runtime(&self) -> Option<LocalTaskRuntime<()>> {
        unsafe {
            (&*self.0.inner.get()).get_runtime()
        }
    }

    /// 线程安全的派发一个异步任务到连接所在运行时
    pub fn spawn(&self, task: LocalBoxFuture<'static, ()>) {
        unsafe {
            (&*self.0.inner.get()).spawn(task);
        }
    }

    /// 关闭指定的流
    pub fn close_expanding_stream(&self, stream_id: StreamId) -> Result<()> {
        unsafe {
            (&*self.0.inner.get()).close_expanding_stream(stream_id)
        }
    }

    /// 线程安全的关闭Quic连接
    pub fn close(&self, code: u32, reason: Result<()>) -> Result<()> {
        unsafe {
            (&mut *self.0.inner.get()).close(code, reason)
        }
    }

    /// 强制关闭当前连接
    pub fn force_close(&mut self, code: u32, reason: String) {
        unsafe {
            (&mut *self.0.inner.get()).force_close(code, reason);
        }
    }
}

// 内部Quic连接句柄
struct InnerSocketHandle {
    uid:            usize,                                                      //Quic连接唯一id
    status:         Arc<AtomicU8>,                                              //Quic连接状态
    inner:          Arc<UnsafeCell<QuicSocket>>,                                //Quic连接指针
    local:          AtomicCell<SocketAddr>,                                     //Quic连接本地地址
    remote:         SocketAddr,                                                 //Quic连接远端地址
    sender:         Sender<QuicEvent>,                                          //Quic事件发送器
}

///
/// Quic连接的事件
///
#[derive(Debug)]
pub struct SocketEvent {
    inner: *mut (), //内部事件
}

unsafe impl Send for SocketEvent {}

impl SocketEvent {
    /// 创建空的事件
    pub fn empty() -> Self {
        SocketEvent {
            inner: ptr::null_mut(),
        }
    }

    /// 判断事件是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.is_null()
    }

    /// 获取事件
    pub fn get<T: 'static>(&self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { *Box::from_raw(self.inner as *mut T) })
    }

    /// 设置事件，如果当前事件不为空，则设置失败
    pub fn set<T: 'static>(&mut self, event: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner = Box::into_raw(Box::new(event)) as *mut T as *mut ();
        true
    }

    /// 移除事件，如果当前有事件，则返回被移除的事件
    pub fn remove<T: 'static>(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        let result = self.get();
        self.inner = ptr::null_mut();
        result
    }
}

///
/// Quic事件
///
#[derive(Debug)]
pub enum QuicEvent {
    Accepted(QuicSocket),                                                       //Quic已接受连接
    ConnectionReceived(ConnectionHandle, ConnectionEvent),                      //Quic连接接收数据
    ConnectionSend(ConnectionHandle, Transmit),                                 //Quic连接发送数据
    StreamOpen(ConnectionHandle, Dir, AsyncValueNonBlocking<Result<StreamId>>), //Quic流打开
    StreamReady(ConnectionHandle, StreamId, QuicSocketReady),                   //Quic流就绪
    StreamWrite(ConnectionHandle, StreamId, Vec<u8>),                           //Quic流写数据
    RebindUdp(UdpSocketHandle),                                                 //Quic重绑定Udp连接句柄
    Timeout(ConnectionHandle, Option<(usize, SocketEvent)>),                    //Quic连接超时
    StreamClose(ConnectionHandle, Option<StreamId>, u32),                       //Quic流关闭
    ConnectionClose(ConnectionHandle),                                          //Quic连接关闭
}




