use std::rc::Rc;
use std::task::Waker;
use std::future::Future;
use std::cell::UnsafeCell;
use std::fmt::{Debug, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use std::result::Result as GenResult;
use std::collections::{VecDeque, BTreeMap};
use std::io::{Cursor, Result, Error, ErrorKind};
use std::sync::{Arc, atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering}};

use quinn_proto::{ConnectionHandle, Connection, Transmit, StreamId, SendStream, RecvStream, ReadError, WriteError, VarInt, Dir};
use crossbeam_channel::Sender;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::future::{BoxFuture, FutureExt, LocalBoxFuture};

use pi_async::{lock::spin_lock::SpinLock,
               rt::{AsyncValueNonBlocking,
                    serial_local_thread::LocalTaskRuntime}};

use udp::SocketHandle;

use crate::{SocketHandle as QuicSocketHandle, SocketEvent,
            utils::{QuicSocketStatus, QuicSocketReady, QuicCloseEvent, Hibernate, SocketContext}, QuicEvent};

/// 默认的读取块大小，单位字节
const DEFAULT_READ_BLOCK_LEN: usize = 4096;

/// 默认的写入块大小，单位字节
const DEFAULT_WRITE_BLOCK_LEN: usize = 4096;

/// 最小的Quic已读读缓冲大小限制，单位字节
const MIN_READED_READ_BUF_SIZE_LIMIT: usize = DEFAULT_READ_BLOCK_LEN;

/// 最小的Quic已读写缓冲大小限制，单位字节
const MIN_READED_WRITE_BUF_SIZE_LIMIT: usize = DEFAULT_WRITE_BLOCK_LEN;

/// 默认的Quic已读读缓冲大小限制，单位字节
const DEAFULT_READED_READ_BUF_SIZE_LIMIT: usize = 64 * 1024;

/// 默认的Quic已读写缓冲大小限制，单位字节
const DEAFULT_READED_WRITE_BUF_SIZE_LIMIT: usize = 64 * 1024;

/// 最小的Quic扩展流数量
const MIN_EXPAND_STREAMS_LEN: usize = 0;

/// 最大的Quic扩展流数量
const MAX_EXPAND_STREAMS_LEN: usize = 0x10000;

///
/// Quic连接
///
pub struct QuicSocket {
    rt:                 Option<LocalTaskRuntime<()>>,                                       //连接所在运行时
    uid:                usize,                                                              //连接唯一id
    status:             Arc<AtomicU8>,                                                      //连接状态
    is_client:          AtomicBool,                                                         //是否是客户端
    udp_handle:         SocketHandle,                                                       //Udp连接句柄
    handle:             ConnectionHandle,                                                   //Quic内部连接句柄
    connect:            Connection,                                                         //Quic内部连接
    readed_read_limit:  usize,                                                              //已读读缓冲大小限制
    readed_write_limit: usize,                                                              //已读写缓冲大小限制
    main_stream_id:     Option<StreamId>,                                                   //连接的主流唯一id
    main_stream:        QuicStream,                                                         //连接的主流
    expanding_streams:  BTreeMap<StreamId, QuicStream>,                                     //扩展流
    clock:              Instant,                                                            //内部时钟
    expired:            Option<Instant>,                                                    //下次到期时间
    timer_ref:          Option<u64>,                                                        //内部定时器
    timer_handle:       Option<u64>,                                                        //外部定时器
    hibernate:          SpinLock<Option<Hibernate>>,                                        //连接异步休眠对象
    hibernate_wakers:   SpinLock<VecDeque<Waker>>,                                          //连接正在休眠时，其它休眠对象的唤醒器队列
    hibernated_queue:   Arc<SpinLock<VecDeque<LocalBoxFuture<'static, ()>>>>,               //连接休眠时任务队列
    socket_handle:      Option<QuicSocketHandle>,                                           //Quic连接句柄
    context:            Rc<UnsafeCell<SocketContext>>,                                      //Quic连接上下文
    event_send:         Option<Sender<QuicEvent>>,                                          //Quic事件发送器
    close_reason:       Option<(u32, Result<()>)>,                                          //连接关闭的原因
}

impl Debug for QuicSocket {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f,
               "QuicSocket[uid = {:?}, local = {:?}, remote = {:?}]",
               self.get_uid(),
               self.get_local(),
               self.get_remote())
    }
}

impl QuicSocket {
    /// 构建一个Quic连接
    pub fn new(udp_handle: SocketHandle,
               handle: ConnectionHandle,
               connect: Connection,
               readed_read_size_limit: usize,
               readed_write_size_limit: usize,
               clock: Instant) -> Self {
        let readed_read_size_limit = if readed_read_size_limit < MIN_READED_READ_BUF_SIZE_LIMIT {
            //TCP已读读缓冲大小限制过小
            DEAFULT_READED_READ_BUF_SIZE_LIMIT
        } else {
            readed_read_size_limit
        };

        let readed_write_size_limit = if readed_write_size_limit < MIN_READED_WRITE_BUF_SIZE_LIMIT {
            //TCP已读写缓冲大小限制过小
            DEAFULT_READED_WRITE_BUF_SIZE_LIMIT
        } else {
            readed_write_size_limit
        };

        let expanding_streams = BTreeMap::new();
        let main_stream = QuicStream::new(Dir::Bi, readed_read_size_limit, readed_write_size_limit);
        let status = Arc::new(AtomicU8::new(QuicSocketStatus::UdpAccepted.into()));
        let is_client = AtomicBool::new(false); //默认是非客户端
        let hibernate = SpinLock::new(None);
        let hibernate_wakers = SpinLock::new(VecDeque::new());
        let hibernated_queue = Arc::new(SpinLock::new(VecDeque::new()));
        let context = Rc::new(UnsafeCell::new(SocketContext::empty()));

        QuicSocket {
            rt: None,
            uid: handle.0,
            status,
            is_client,
            udp_handle,
            handle,
            connect,
            readed_read_limit: readed_read_size_limit,
            readed_write_limit: readed_write_size_limit,
            main_stream_id: None,
            main_stream,
            expanding_streams,
            clock,
            expired: None,
            timer_ref: None,
            timer_handle: None,
            hibernate,
            hibernate_wakers,
            hibernated_queue,
            socket_handle: None,
            context,
            event_send: None,
            close_reason: None,
        }
    }

    /// 判断当前连接是否已关闭
    pub fn is_closeing(&self) -> bool {
        if let QuicSocketStatus::Closeing = self.status.load(Ordering::Acquire).into() {
            true
        } else {
            false
        }
    }

    /// 判断当前连接是否已关闭
    pub fn is_closed(&self) -> bool {
        if self.status.load(Ordering::Acquire) > 3 {
            true
        } else {
            false
        }
    }

    /// 获取连接所在运行时
    pub fn get_runtime(&self) -> Option<LocalTaskRuntime<()>> {
        self.rt.clone()
    }

    /// 设置连接所在运行时
    pub fn set_runtime(&mut self, rt: LocalTaskRuntime<()>) {
        self.rt = Some(rt);
    }

    /// 获取连接唯一id
    pub fn get_uid(&self) -> usize {
        self.uid
    }

    /// 获取内部连接句柄
    pub fn get_connection_handle(&self) -> &ConnectionHandle {
        &self.handle
    }

    /// 获取内部连接的只读引用
    pub(crate) fn get_connection(&self) -> &Connection {
        &self.connect
    }

    /// 获取内部连接的可写引用
    pub(crate) fn get_connection_mut(&mut self) -> &mut Connection {
        &mut self.connect
    }

    /// 获取连接状态
    pub fn get_status(&self) -> QuicSocketStatus {
        self.status.load(Ordering::Acquire).into()
    }

    /// 设置连接状态
    pub fn set_status(&mut self, status: QuicSocketStatus) {
        self.status.store(status.into(), Ordering::Release);
    }

    /// 获取本地连接地址
    pub fn get_local(&self) -> SocketAddr {
        self
            .udp_handle
            .get_local()
            .clone()
    }

    /// 获取远端连接地址
    pub fn get_remote(&self) -> SocketAddr {
        self.connect.remote_address()
    }

    /// 判断当前连接是否通过0rtt建立连接
    pub fn is_0rtt(&self) -> bool {
        self.connect.is_handshaking()
    }

    /// 获取当前连接的延迟估计
    pub fn get_latency(&self) -> Duration {
        self.connect.rtt()
    }

    /// 获取连接的主流唯一id
    pub fn get_main_stream_id(&self) -> Option<&StreamId> {
        self.main_stream_id.as_ref()
    }

    /// 接受连接的主流唯一id
    pub fn accept_main_streams(&mut self, stream_id: StreamId) {
        self.main_stream_id = Some(stream_id);
    }

    /// 打开连接的主流，主流一定是双向流
    pub(crate) fn open_main_streams(&mut self) -> Result<()> {
        if let Some(stream_id) = self.connect.streams().open(Dir::Bi) {
            //创建主流成功，则为当前连接设置主流的唯一id
            self.main_stream_id = Some(stream_id);
            Ok(())
        } else {
            //创建主流失败，则立即返回错误原因
            Err(Error::new(ErrorKind::ConnectionAborted,
                           format!("Open main stream failed, uid: {:?}, remote: {:?}, local: {:?}, reason: open streams error",
                                   self.get_uid(),
                                   self.get_remote(),
                                   self.get_local())))
        }
    }

    /// 接受连接的扩展流
    pub fn accept_expanding_streams(&mut self,
                                    stream_id: StreamId,
                                    stream_type: Dir) {
        self
            .expanding_streams
            .insert(stream_id,
                    QuicStream::new(stream_type,
                                    self.readed_read_limit,
                                    self.readed_write_limit));
    }

    /// 加入指定的流
    pub(crate) fn add_expanding_stream(&mut self,
                                       stream_id: StreamId,
                                       stream_type: Dir) {
        self
            .expanding_streams
            .insert(stream_id,
                    QuicStream::new(stream_type,
                                    self.readed_read_limit,
                                    self.readed_write_limit));
    }

    /// 打开连接的扩展流，可以指定流的类型
    pub async fn open_expanding_stream(&self,
                                       stream_type: Dir) -> Result<StreamId> {
        if let Some(sender) = &self.event_send {
            let result = AsyncValueNonBlocking::new();
            let result_copy = result.clone();
            if let Err(e) = sender.send(QuicEvent::StreamOpen(self.handle,
                                                              stream_type,
                                                              result_copy)) {
                //发送事件失败，则立即返回错误原因
                Err(Error::new(ErrorKind::ConnectionAborted,
                               format!("Open expanding stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_type: {:?}, reason: {:?}",
                                       self.get_uid(),
                                       self.get_remote(),
                                       self.get_local(),
                                       stream_type,
                                       e)))
            } else {
                //发送事件成功，则等待流打开成功
                result.await
            }
        } else {
            Err(Error::new(ErrorKind::ConnectionAborted,
                           format!("Open expanding stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_type: {:?}, reason: event sender not exist",
                                   self.get_uid(),
                                   self.get_remote(),
                                   self.get_local(),
                                   stream_type)))
        }
    }

    /// 关闭指定的流
    pub fn close_expanding_stream(&self, stream_id: StreamId) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }

        if self.main_stream_id.unwrap() == stream_id {
            //不允许关闭主流
            return Err(Error::new(ErrorKind::BrokenPipe,
                                  format!("Close quic connection stream failed, reason: disable close main stream")));
        }

        if let Some(stream) = self.get_stream(&stream_id) {
            //指定流存在
            if let Some(value) = stream.ready_reader.lock().take() {
                //当前连接设置了异步准备读取器，则立即移除并唤醒当前异步准备读取器，并设置读取的长度为0
                unsafe {
                    value.set(0);
                    (*stream.wait_ready_len.get()) = 0; //重置异步准备读取的字节数
                    (*stream.ready_len.get()) = 0; //重置异步准备读取已就绪的字节数
                }
            }
        }

        //通知关闭当前连接的指定流
        if let Some(sender) = &self.event_send {
            if let Err(e) = sender.send(QuicEvent::StreamClose(self.get_connection_handle().clone(), Some(stream_id), 0)) {
                return Err(Error::new(ErrorKind::BrokenPipe,
                                      format!("Close quic connection stream failed, reason: {:?}",
                                              e)));
            }
        }

        Ok(())
    }

    /// 设置连接的Quic事件发送器
    pub fn set_event_sent(&mut self, event_send: Option<Sender<QuicEvent>>) {
        self.event_send = event_send;
    }

    /// 获取Udp连接句柄的只读引用
    pub fn get_udp_handle(&self) -> &SocketHandle {
        &self.udp_handle
    }

    /// 设置Udp连接句柄
    pub fn set_udp_handle(&mut self,
                          udp_handle: SocketHandle) {
        self.udp_handle = udp_handle;
    }

    /// 判断当前连接是否是客户端连接
    pub fn is_client(&self) -> bool {
        self.is_client.load(Ordering::Relaxed)
    }

    /// 启用客户端
    pub(crate) fn enable_client(&self) {
        self.is_client.store(true, Ordering::Relaxed);
    }

    /// 获取Quic连接句柄
    pub fn get_socket_handle(&self) -> QuicSocketHandle {
        self
            .socket_handle
            .as_ref()
            .unwrap()
            .clone()
    }

    /// 设置Quic连接句柄
    pub fn set_socket_handle(&mut self,
                             socket: Arc<UnsafeCell<Self>>) {
        let socket_handle = QuicSocketHandle::new(self.uid,
                                                  self.status.clone(),
                                                  socket,
                                                  self.udp_handle.get_local().clone(),
                                                  self.get_remote(),
                                                  self.event_send.as_ref().unwrap().clone());

        self.socket_handle = Some(socket_handle);
    }

    /// 移除Quic连接句柄
    pub fn remove_socket_handle(&mut self) -> Option<QuicSocketHandle> {
        self.socket_handle.take()
    }

    /// 获取Quic内部连接的只读引用
    pub fn get_connect(&self) -> &Connection {
        &self.connect
    }

    /// 获取Quic内部连接的可写引用
    pub fn get_connect_mut(&mut self) -> &mut Connection {
        &mut self.connect
    }

    /// 获取Quic连接上下文只读引用
    pub fn get_context(&self) -> Rc<UnsafeCell<SocketContext>> {
        self.context.clone()
    }

    /// 获取内部时钟
    pub fn get_clock(&self) -> &Instant {
        &self.clock
    }

    /// 获取下次到期时间
    pub fn get_expired(&self) -> Option<&Instant> {
        self.expired.as_ref()
    }

    /// 设置下次到期时间
    pub fn set_expired(&mut self, expired: Option<Instant>) {
        self.expired = expired;
    }

    /// 获取内部定时器
    pub fn get_timer_ref(&self) -> Option<u64> {
        self.timer_ref
    }

    /// 设置内部定时器
    pub fn set_timer_ref(&mut self, timer_ref: Option<u64>) {
        self.timer_ref = timer_ref;
    }

    /// 移除内部定时器
    pub fn unset_timer_ref(&mut self) -> Option<u64> {
        self.timer_ref.take()
    }

    /// 设置连接的超时定时器，同时只允许设置一个定时器，新的定时器会覆盖未超时的旧定时器
    pub fn set_timeout(&self, timeout: usize, event: SocketEvent) {
        if let Some(sender) = &self.event_send {
            sender
                .send(QuicEvent::Timeout(self.handle, Some((timeout, event))));
        }
    }

    /// 取消连接的未超时超时定时器
    pub fn unset_timeout(&self) {
        if let Some(sender) = &self.event_send {
            sender
                .send(QuicEvent::Timeout(self.handle, None));
        }
    }

    /// 设置外部定时器句柄，返回上个定时器句柄
    pub fn set_timer_handle(&mut self, timer_handle: u64) -> Option<u64> {
        let last_timer_handle = self.unset_timer_handle();
        self.timer_handle = Some(timer_handle);
        last_timer_handle
    }

    /// 取消外部定时器句柄，返回定时器句柄
    pub fn unset_timer_handle(&mut self) -> Option<u64> {
        self.timer_handle.take()
    }

    /// 写入指定的Quic数据报文
    pub fn write_transmit(&self, data: Transmit) -> Result<()> {
        if let Some(sender) = &self.event_send {
            if let Err(e) = sender.send(QuicEvent::ConnectionSend(self.handle, data)) {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Send quic data failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                              self.get_uid(),
                                              self.get_remote(),
                                              self.get_local(),
                                              e)));
            }
        }

        Ok(())
    }

    /// 发送指定的Quic数据报文
    pub fn send_transmit(&self, data: Transmit) -> Result<()> {
        if let Some(size) = data.segment_size {
            //多帧Quic数据报文
            let mut offset = 0; //数据报文的帧偏移
            let mut len = data.contents.len(); //数据报文的剩余大小
            while len > size {
                if let Err(e) = self.udp_handle.write((&data.contents[offset..offset + size]).to_vec(), Some(data.destination)) {
                    return Err(Error::new(ErrorKind::Other,
                                          format!("Send transmit next frame failed, uid: {:?}, remote: {:?}, local: {:?}, size: {:?}, offset: {:?}, len: {:?}, reason: {:?}",
                                                  self.get_uid(),
                                                  self.get_remote(),
                                                  self.get_local(),
                                                  size,
                                                  offset,
                                                  len,
                                                  e)));
                }
                offset += size; //更新数据报文的帧偏移
                len -= size; //更新数据报文的剩余大小
            }
            if let Err(e) = self.udp_handle.write((&data.contents[offset..offset + len]).to_vec(), Some(data.destination)) {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Send transmit tail frame failed, uid: {:?}, remote: {:?}, local: {:?}, size: {:?}, offset: {:?}, len: {:?}, reason: {:?}",
                                              self.get_uid(),
                                              self.get_remote(),
                                              self.get_local(),
                                              size,
                                              offset,
                                              len,
                                              e)));
            }
        } else {
            //单帧Quic数据报文
            let mut len = data.contents.len(); //数据报文的大小
            if let Err(e) = self.udp_handle.write(data.contents, Some(data.destination)) {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Send transmit single frame failed, uid: {:?}, remote: {:?}, local: {:?}, size: None, offset: {:?}, len: {:?}, reason: {:?}",
                                              self.get_uid(),
                                              self.get_remote(),
                                              self.get_local(),
                                              0,
                                              len,
                                              e)));
            }
        }


        Ok(())
    }

    /// 设置当前连接感兴趣的事件
    #[inline]
    pub fn set_ready(&self,
                     stream_id: StreamId,
                     ready: QuicSocketReady) {
        if let Some(sender) = &self.event_send {
            sender
                .send(QuicEvent::StreamReady(self.handle, stream_id, ready));
        }
    }

    /// 获取读取的块大小
    pub fn get_read_block_len(&self, stream_id: &StreamId) -> Option<usize> {
        if let Some(id) = &self.main_stream_id {
            if stream_id == id {
                //获取主流的读取块大小
                return Some(self
                    .main_stream
                    .read_len
                    .load(Ordering::Acquire));
            }
        }

        if let Some(stream) = self.expanding_streams.get(stream_id) {
            //获取扩展流的读取块大小
            Some(stream.read_len.load(Ordering::Acquire))
        } else {
            //指定的流不存在
            None
        }
    }

    /// 设置读取的块大小
    pub fn set_read_block_len(&self,
                              stream_id: &StreamId,
                              len: usize) {
        if let Some(id) = &self.main_stream_id {
            if stream_id == id {
                //设置主流的读取块大小
                self
                    .main_stream
                    .read_len
                    .store(len, Ordering::Release);
                return;
            }
        }

        if let Some(stream) = self.expanding_streams.get(stream_id) {
            //获取扩展流的读取块大小
            stream
                .read_len
                .store(len, Ordering::Release);
        }
    }

    /// 获取连接的所有流唯一id，主流是第一个流
    pub fn get_stream_ids(&self) -> Vec<StreamId> {
        vec![self.main_stream_id.unwrap().clone()]
            .iter()
            .chain(self.expanding_streams.keys())
            .map(|x| x.clone())
            .collect()
    }

    /// 获取指定唯一id的流的只读引用
    #[inline]
    pub fn get_stream(&self, stream_id: &StreamId) -> Option<&QuicStream> {
        if let Some(id) = &self.main_stream_id {
            if stream_id == id {
                return Some(&self.main_stream);
            }
        }

        self.expanding_streams.get(stream_id)
    }

    /// 获取连接指定流的接收流
    #[inline]
    pub fn get_recv_stream(&mut self, stream_id: StreamId) -> RecvStream<'_> {
        self.connect.recv_stream(stream_id)
    }

    /// 获取连接指定流的发送流
    #[inline]
    pub fn get_send_stream(&mut self, stream_id: StreamId) -> SendStream<'_> {
        self.connect.send_stream(stream_id)
    }

    /// 是否需要继续从连接流中接收数据
    #[inline]
    pub fn is_require_recv(&self, stream_id: &StreamId) -> bool {
        if let Some(stream) = self.get_stream(stream_id) {
            //指定流存在
            unsafe {
                return (*stream.wait_recv_len.get()) > (*stream.recv_len.get());
            }
        } else {
            //指定流不存在
            false
        }
    }

    /// 接收指定流中的数据，返回成功，则表示本次接收了需要的字节数，并返回本次接收的字节数，否则返回接收错误
    pub fn recv(&mut self, stream_id: StreamId) -> Result<usize> {
        if self.is_closed() {
            //连接正在关闭或已关闭，则忽略，并立即返回
            return Err(Error::new(ErrorKind::ConnectionAborted,
                                  format!("Receive stream failed, uid: {:?}, peer: {:?}, local: {:?}, reason: connection already closed",
                                          self.get_uid(),
                                          self.get_remote(),
                                          self.get_local())));
        }

        if !self.expanding_streams.contains_key(&stream_id) {
            //指定的流不存在，一般是因为流是由服务端创建，客户端并不知道流已创建，则创建新的流
            self
                .add_expanding_stream(stream_id.clone(),
                                      Dir::Bi);
        }

        let uid = self.get_uid();
        let remote = self.get_remote();
        let local = self.get_local();
        let mut result = Ok(0); //初始化本次接收的结果值
        match self.connect.recv_stream(stream_id).read(false) {
            Err(e) => {
                //获取当前连接接收流失败，则立即返回错误原因
                result = Err(Error::new(ErrorKind::ConnectionAborted,
                                        format!("Receive stream failed, uid: {:?}, peer: {:?}, local: {:?}, reason: {:?}",
                                                uid,
                                                remote,
                                                local,
                                                e)));
            },
            Ok(mut chunks) => {
                //成功获取当前连接的主流的接收流
                let mut stream = if let Some(id) = &self.main_stream_id {
                    if &stream_id == id {
                        Some(&self.main_stream)
                    } else {
                        self.expanding_streams.get(&stream_id)
                    }
                } else {
                    //主流不存在
                    None
                };

                if let Some(stream) = stream {
                    //指定流存在
                    let mut blocks_len = 0; //初始化本次接收的块大小
                    let mut blocks: Vec<(u64, Bytes)> = Vec::new(); //初始化本次接收的块

                    loop {
                        match chunks.next(usize::MAX) {
                            Ok(Some(chunk)) => {
                                //部分接收成功，则继续接收，直到接收完流的当前的所有数据
                                unsafe {
                                    let block_len = chunk.bytes.len(); //部分接收成功的块大小
                                    (*stream.recv_len.get()) += block_len; //增加连接已接收的字节数
                                    if stream.ready_reader.lock().is_some() {
                                        //当前连接设置了异步准备接收器，则记录本次成功接收的字节数
                                        (*stream.ready_len.get()) += block_len;
                                    }
                                    blocks_len += block_len; //增加已接收块的大小

                                    blocks.push((chunk.offset, chunk.bytes));
                                }
                            },
                            Ok(None) => {
                                //对端已关闭当前连接流，则立即返回错误原因
                                result = Err(Error::new(ErrorKind::ConnectionAborted,
                                                        format!("Receive stream failed, uid: {:?}, remote: {:?}, local: {:?}, reason: peer already closed",
                                                                uid,
                                                                remote,
                                                                local)));

                                break;
                            },
                            Err(e) if e == ReadError::Blocked => {
                                //已接收完流的当前的所有数据，则退出本次接收
                                blocks.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap()); //重新排序接收到的所有数据块
                                let (_, bytes_vec): (Vec<u64>, Vec<Bytes>) = blocks.into_iter().unzip();
                                for bytes in bytes_vec {
                                    if let Some(buf) = stream.read_buf.lock().as_mut() {
                                        //填充到连接的读缓冲区
                                        buf.put_slice(bytes.as_ref());
                                    }
                                }

                                result = Ok(blocks_len);
                                break;
                            },
                            Err(e) => {
                                //接收错误，则立即返回错误原因
                                result = Err(Error::new(ErrorKind::ConnectionAborted,
                                                        format!("Receive stream failed, uid: {:?}, peer: {:?}, local: {:?}, reason: {:?}",
                                                                uid,
                                                                remote,
                                                                local,
                                                                e)));
                                break;
                            },
                        }
                    }

                    if unsafe { (*stream.wait_recv_len.get()) > (*stream.recv_len.get()) } {
                        //本次接收已完成，但当前连接未接收足够的数据，则设置连接需要继续处理接收事件
                        if let Some(sender) = &self.event_send {
                            sender
                                .send(QuicEvent::StreamReady(self.handle, stream_id, QuicSocketReady::Readable));
                        }
                    }
                }

                //完成本次流的所有数据的接收
                chunks.finalize();
            }
        }

        if let Some(stream) = self.get_stream(&stream_id) {
            //指定流存在
            if result.is_ok() {
                //本次接收成功
                unsafe {
                    if (*stream.readed.get()) > stream.readed_read_limit.load(Ordering::Relaxed) {
                        //本次接收成功，且已达已读读缓冲大小限制，则清理已读取的读缓冲区，并释放对应的内存

                            let old_buf = stream.read_buf.lock().take().unwrap();
                            let mut new_buf = BytesMut::with_capacity(old_buf.remaining());
                            new_buf.put(old_buf);
                            *stream.read_buf.lock() = Some(new_buf);
                            (*stream.readed.get()) = 0;
                    }
                }
            }
        }

        //返回本次流接收的结果
        result
    }

    /// 通知连接的指定流读就绪，可以开始接收指定字节数的数据，如果当前需要等待接收则返回AsyncValueNonBlocking, 否则返回接收缓冲区中已有数据的字节数
    /// 设置准备读取的字节大小为0，则表示准备接收任意数量的字节，直到当前连接的流没有可接收的数据
    /// 设置准备读取的字节大小大于0，则表示至少需要接收指定数量的字节，如果还未接收到指定数量的字节，则继续从流中接收
    /// 异步阻塞读取读缓冲前应该保证调用此函数对读缓冲进行填充，避免异步读取被异步阻塞
    /// 返回0长度，表示当前连接已关闭，继续操作将是未定义行为
    /// 注意调用此方法，在保持连接的前提下，必须保证后续一定还可以接收到数据，否则会导致无法唤醒当前异步准备读取器
    pub fn read_ready(&mut self,
                      stream_id: &StreamId,
                      adjust: usize) -> GenResult<AsyncValueNonBlocking<usize>, usize> {
        if self.is_closed() {
            //连接已关闭，则忽略，并立即返回
            return Err(0);
        }

        if let Some(stream) = self.get_stream(stream_id) {
            //指定流存在
            unsafe { (*stream.wait_recv_len.get()) += adjust; } //增加流需要接收的字节数
            self
                .set_ready(stream_id.clone(),
                           QuicSocketReady::Readable); //设置连接当前对读事件感兴趣

            let remaining = unsafe {
                stream.read_buf
                    .lock()
                    .as_ref()
                    .unwrap()
                    .remaining()
            };
            if remaining >= adjust && remaining > 0 {
                //流当前读取缓冲区有足够的数据，则立即返回当前读取缓冲区中剩余可读字节的数量
                return Err(remaining);
            }

            //流当前读缓冲区没有足够的数据，则只读需要的字节数
            let value = AsyncValueNonBlocking::new();
            let value_copy = value.clone();
            *stream.ready_reader.lock() = Some(value); //设置当前流的异步准备读取器
            unsafe { (*stream.wait_ready_len.get()) = adjust - remaining; } //设置本次异步准备读取实际需要的字节数

            Ok(value_copy)
        } else {
            //指定流不存在
            Err(0)
        }
    }

    /// 判断当前连接的指定流是否有异步准备读取器
    pub fn is_wait_wakeup_read_ready(&self, stream_id: &StreamId) -> bool {
        if let Some(id) = &self.main_stream_id {
            if stream_id == id {
                //判断主流是否有异步准备读取器
                return self
                    .main_stream
                    .ready_reader
                    .lock()
                    .is_some()
            }
        }

        if let Some(stream) = self.expanding_streams.get(stream_id) {
            //判断扩展流是否有异步准备读取器
            stream
                .ready_reader
                .lock()
                .is_some()
        } else {
            //指定的流不存在
            false
        }
    }

    /// 唤醒指定流异步阻塞的读就绪，如果指定流没有异步准备读取器则忽略
    pub fn wakeup_read_ready(&mut self, stream_id: &StreamId) {
        if let Some(stream) = self.get_stream(stream_id) {
            //指定流存在
            unsafe {
                if ((*stream.wait_ready_len.get()) == 0) || ((*stream.wait_ready_len.get()) <= (*stream.ready_len.get())) {
                    //已完成异步准备读取指定的字节数
                    if let Some(ready_reader) = stream.ready_reader.lock().take() {
                        //当前连接已设置了异步准备读取器，则立即移除并唤醒当前异步准备读取器
                        ready_reader.set((*stream.ready_len.get())); //设置实际读取的字节数
                        (*stream.wait_ready_len.get()) = 0; //重置异步准备读取的字节数
                        (*stream.ready_len.get()) = 0; //重置异步准备读取已就绪的字节数
                    }
                }
            }
        }
    }

    /// 获取连接的输入缓冲区的剩余未读字节数
    pub fn read_buffer_remaining(&self, stream_id: &StreamId) -> Option<usize> {
        if let Some(stream) = self.get_stream(stream_id) {
            //指定流存在
            if let Some(buf) = stream.read_buf.lock().as_ref() {
                Some(buf.remaining())
            } else {
                None
            }
        } else {
            //指定流不存在
            None
        }
    }

    /// 获取指定流的输入缓冲区的只读引用
    pub fn get_read_buffer(&self,
                           stream_id: &StreamId) -> Option<Arc<SpinLock<Option<BytesMut>>>> {
        if let Some(id) = &self.main_stream_id {
            if stream_id == id {
                //获取主流的输入缓冲区的只读引用
                return Some(self
                    .main_stream
                    .read_buf
                    .clone())
            }
        }

        if let Some(stream) = self.expanding_streams.get(stream_id) {
            //获取扩展流的输入缓冲区的只读引用
            Some(stream
                .read_buf
                .clone())
        } else {
            //指定的流不存在
            None
        }
    }

    /// 获取写入的块大小
    pub fn get_write_block_len(&self, stream_id: &StreamId) -> Option<usize> {
        if let Some(id) = &self.main_stream_id {
            if stream_id == id {
                //获取主流的写入的块大小
                return Some(self
                    .main_stream
                    .write_len
                    .load(Ordering::Acquire))
            }
        }

        if let Some(stream) = self.expanding_streams.get(stream_id) {
            //获取扩展流的写入的块大小
            Some(stream
                .write_len
                .load(Ordering::Acquire))
        } else {
            //指定的流不存在
            None
        }
    }

    /// 设置写入的块大小
    pub fn set_write_block_len(&self,
                               stream_id: &StreamId,
                               len: usize) {
        if let Some(stream) = self.get_stream(stream_id) {
            //指定流存在
            stream.write_len.store(len, Ordering::Release);
        }
    }

    /// 发送数据到流，返回成功，则返回本次发送了多少字节数，否则返回发送错误原因
    pub fn send(&mut self, stream_id: StreamId) -> Result<usize> {
        if let QuicSocketStatus::Disconnect = self.get_status() {
            //连接已关闭，则忽略，并立即返回
            return Err(Error::new(ErrorKind::ConnectionAborted,
                                  format!("Send stream failed, uid: {:?}, remote: {:?}, local: {:?}, reason: connection already closed",
                                          self.get_uid(),
                                          self.get_remote(),
                                          self.get_local())));
        }

        let mut result = Ok(0); //初始化本次发送的结果值
        let mut block_pos = 0; //初始化发送块的已填充位置
        if let Some(mut block_len) = self.get_write_block_len(&stream_id) {
            //获取本次发送的限制
            if let Some(write_buf) = self.get_write_buffer(&stream_id) {
                let remaining = write_buf.remaining(); //当前写缓冲区剩余未发送的数据大小
                block_len = if block_len > remaining {
                    //当前写缓冲区剩余未发送的数据小于本次发送限制，则以剩余未发送的数据大小作为本次发送的发送块大小
                    remaining
                } else {
                    //当前写缓冲区剩余未发送的数据大于等于本次发送限制，则以本次发送限制的大小作为本次发送的发送块大小
                    block_len
                };
                let bytes = write_buf.copy_to_bytes(block_len);
                let mut block = bytes.as_ref(); //初始化本次发送块
                drop(write_buf);

                while !block.is_empty() {
                    //发送块中还有未发送的数据
                    match self.get_send_stream(stream_id).write(block) {
                        Ok(0) => {
                            //未能向流写入整个发送块，则立即返回错误原因
                            result = Err(Error::new(ErrorKind::WriteZero,
                                                    format!("Send stream failed, uid: {:?}, remote: {:?}, local: {:?}, reason: failed to write whole buffer",
                                                            self.get_uid(),
                                                            self.get_remote(),
                                                            self.get_local())));
                            break;
                        },
                        Ok(len) => {
                            //部分发送成功，则继续发送，直到发送完发送块中的所有数据
                            if let Some(stream) = self.get_stream(&stream_id) {
                                //指定流存在
                                unsafe {
                                    (*stream.sent_len.get()) += len; //增加连接已发送的字节数
                                    (*stream.readed_write_len.get()) += len; //增加已读写缓冲大小的字节数
                                    block_pos += len; //增加接收块的已填充位置

                                    block = &block[block_pos..]; //设置需要继续发送的块
                                    result = Ok(block_pos);
                                }
                            }
                        },
                        Err(e) if e == WriteError::Blocked => {
                            //本次发送已阻塞，则退出本次发送
                            if !block.is_empty() {
                                //当前发送块中还有未发送的数据，则重新写入写缓冲区
                                //注意这是因为每次发送都会消耗当前写缓冲区中的所有数据，所以可以将未发送完的数据重新写入写缓冲区
                                if let Some(stream) = self.get_stream(&stream_id) {
                                    //指定流存在
                                    unsafe {
                                        if let Some(write_buf) = (&mut *stream.write_buf.get()) {
                                            write_buf.put_slice(&block[block_pos..]);
                                        }
                                    }
                                }
                            }

                            result = Ok(block_pos);
                            break;
                        },
                        Err(e) => {
                            //发送错误，则立即返回错误原因
                            result = Err(Error::new(ErrorKind::ConnectionAborted,
                                                    format!("Send stream failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                            self.get_uid(),
                                                            self.get_remote(),
                                                            self.get_local(),
                                                            e)));
                            break;
                        },
                    }
                }
            }

            //本次发送已完成
            if let Some(stream) = self.get_stream(&stream_id) {
                //指定流存在
                unsafe {
                    if (block_pos > 0)
                        && ((*stream.readed_write_len.get()) > stream.readed_write_limit.load(Ordering::Relaxed)) {
                        //本次发送成功，已达已读写缓冲大小限制，则清理已发送的写缓冲区，并释放对应的内存
                        let old_write_buf = (&mut *stream.write_buf.get())
                            .take()
                            .unwrap();
                        let mut new_write_buf = BytesMut::new();
                        new_write_buf.put(old_write_buf);
                        (*stream.readed_write_len.get()) = 0; //重置已读写缓冲大小
                        (*stream.write_buf.get()) = Some(new_write_buf); //重置写缓冲区
                    }

                    if block_pos < block_len {
                        //本次发送部分成功，则增加连接当前对写事件感兴趣
                        self.set_ready(stream_id.clone(), QuicSocketReady::Writable);
                    } else {
                        //本次发送全部成功或完全没有发送
                        if let Some(write_buf) = (&mut *stream.write_buf.get()) {
                            if write_buf.remaining() > 0
                                || stream.wait_sent_len.load(Ordering::Relaxed) > (*stream.sent_len.get()) {
                                //连接的当前写缓冲区中还有待发送的数据，或者还有未填充到写缓冲区的的待发送数据，则增加连接当前只对写事件感兴趣
                                self.set_ready(stream_id.clone(), QuicSocketReady::Writable);
                            } else {
                                //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                                self.set_ready(stream_id.clone(), QuicSocketReady::Readable);
                            }
                        } else {
                            //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                            self.set_ready(stream_id.clone(), QuicSocketReady::Readable);
                        }
                    }
                }
            }
        }

        result
    }

    /// 获取连接的输出缓冲区的可写引用
    #[inline]
    pub fn get_write_buffer(&mut self,
                            stream_id: &StreamId) -> Option<&mut BytesMut> {
        if let Some(stream) = self.get_stream(stream_id) {
            //指定流存在
            unsafe {
                (&mut *stream.write_buf.get()).as_mut()
            }
        } else {
            //指定的流不存在
            None
        }
    }

    /// 通知连接写就绪，可以开始发送指定的数据
    pub fn write_ready<B>(&self,
                          stream_id: StreamId,
                          buf: B) -> Result<()>
        where B: AsRef<[u8]> + 'static {
        if self.is_closed() {
            //连接已关闭，则忽略，并立即返回
            return Err(Error::new(ErrorKind::ConnectionAborted,
                                  format!("Write ready failed, uid: {:?}, peer: {:?}, local: {:?}, reason: connection already closed",
                                          self.get_uid(),
                                          self.get_remote(),
                                          self.get_local())));
        }

        //发送指定数据
        if let Some(stream) = self.get_stream(&stream_id) {
            //流存在
            stream.wait_sent_len
                .fetch_add(buf.as_ref().len(), Ordering::Relaxed); //首先要同步增加需要发送的字节数
            if let Some(sender) = &self.event_send {
                sender.send(QuicEvent::StreamWrite(self.handle, stream_id, buf.as_ref().to_vec()));
            }
        }

        Ok(())
    }

    /// 判断当前连接是否已休眠
    pub fn is_hibernated(&self) -> bool {
        self.hibernate.lock().is_some() ||
            self.hibernated_queue.lock().len() > 0
    }

    /// 当前连接已休眠，则新的任务必须加入等待任务队列中
    pub fn push_hibernated_task<F>(&self,
                                   task: F)
        where F: Future<Output = ()> + 'static {
        let boxed = async move {
            task.await;
        }.boxed_local();

        self.hibernated_queue.lock().push_back(boxed);
    }

    /// 开始执行连接休眠时加入的任务，当前任务执行完成后自动执行下一个任务，直到任务队列为空
    pub fn run_hibernated_tasks(&self) {
        if let Some(rt) = &self.rt {
            let hibernated_queue = self.hibernated_queue.clone();

            rt.spawn(async move {
                loop {
                    let task = {
                        //立即释放锁，防止锁重入
                        hibernated_queue
                            .lock()
                            .pop_front()
                    };

                    if let Some(task) = task {
                        task.await;
                    } else {
                        //休眠任务队列已清空，则退出当前执行
                        return;
                    }
                }
            });
        }
    }

    /// 获取当前连接的休眠对象，返回空表示连接已关闭
    pub fn hibernate(&self,
                     handle: QuicSocketHandle,
                     stream_id: StreamId,
                     ready: QuicSocketReady) -> Option<Hibernate> {
        if self.is_closed() {
            //连接已关闭，则立即返回空
            return None;
        }

        let hibernate = Hibernate::new(handle, stream_id, ready);
        let hibernate_copy = hibernate.clone();

        Some(hibernate_copy)
    }

    /// 设置当前连接的休眠对象，设置成功返回真
    pub fn set_hibernate(&self, hibernate: Hibernate) -> bool {
        let mut locked = self.hibernate.lock();
        if locked.is_some() {
            //当前连接已设置了休眠对象，则返回失败
            return false;
        }

        *locked = Some(hibernate);
        true
    }

    /// 设置当前连接在休眠时挂起的其它休眠对象的唤醒器
    pub fn set_hibernate_wakers(&self, waker: Waker) {
        self
            .hibernate_wakers
            .lock()
            .push_back(waker);
    }

    /// 非阻塞的唤醒被休眠的当前连接，如果当前连接未被休眠，则忽略
    /// 还会唤醒当前连接正在休眠时，当前连接的所有其它休眠对象的唤醒器
    /// 唤醒过程可能会被阻塞，这不会导致线程阻塞而是返回假，调用者可以继续尝试唤醒，直到返回真
    pub fn wakeup(&mut self, result: Result<()>) -> bool {
        if self.is_closed() {
            //连接已关闭，则忽略唤醒
            return true;
        }

        let mut r = false;
        if let Some(hibernate) = self.hibernate.lock().take() {
            r = hibernate.wakeup(result); //唤醒休眠的当前连接
        }

        if r {
            //当前连接成功唤醒后，再唤醒当前连接在休眠时生成的所有其它休眠对象
            let mut locked = self.hibernate_wakers.lock();
            while let Some(waker) = locked.pop_front() {
                waker.wake();
            }
        }

        r
    }

    /// 派发一个异步任务到连接所在运行时
    pub fn spawn(&self, task: LocalBoxFuture<'static, ()>) {
        if let Some(rt) = &self.rt {
            rt.send(task);
        }
    }

    /// 获取连接关闭的原因
    pub fn get_close_reason(&mut self) -> Option<&(u32, Result<()>)> {
        self.close_reason.as_ref()
    }

    /// 获取连接关闭的原因
    pub fn close_reason(&mut self) -> Option<(u32, Result<()>)> {
        self.close_reason.take()
    }

    /// 线程安全的关闭Quic连接
    pub fn close(&mut self,
                 code: u32,
                 reason: Result<()>) -> Result<()> {
        //更新连接状态为已关闭
        let mut current = 3;
        loop {
            match self.status.compare_exchange(current,
                                               4,
                                               Ordering::AcqRel,
                                               Ordering::Relaxed) {
                Err(new_current) if new_current > 3 => {
                    //当前连接正在关闭或已关闭，则忽略
                    return Ok(());
                },
                Err(new_current) => {
                    //当前连接状态已更改，则重新尝试关闭连接
                    current = new_current;
                },
                Ok(_) => {
                    //当前连接未关闭，则继续
                    break;
                }
            }
        }

        for stream_id in self.get_stream_ids() {
            if let Some(stream) = self.get_stream(&stream_id) {
                //指定流存在
                if let Some(value) = stream.ready_reader.lock().take() {
                    //当前连接设置了异步准备读取器，则立即移除并唤醒当前异步准备读取器，并设置读取的长度为0
                    unsafe {
                        value.set(0);
                        (*stream.wait_ready_len.get()) = 0; //重置异步准备读取的字节数
                        (*stream.ready_len.get()) = 0; //重置异步准备读取已就绪的字节数
                    }
                }
            }
        }
        self.close_reason = Some((code, reason)); //设置连接关闭的原因

        //通知连接关闭，并关闭当前连接的所有流
        if let Some(sender) = &self.event_send {
            if let Err(e) = sender.send(QuicEvent::StreamClose(self.get_connection_handle().clone(), None, code)) {
                return Err(Error::new(ErrorKind::BrokenPipe,
                                      format!("Close quic connection failed, reason: {:?}",
                                              e)));
            }
        }

        Ok(())
    }

    /// 判断当前连接的主流是否已发送完成
    pub fn is_send_finished(&mut self) -> bool {
        self
            .connect
            .streams()
            .send_streams() == 0
    }

    /// 关闭当前连接的指定流，空则表示关闭当前连接的所有流
    pub fn shutdown(&mut self,
                    stream_id: Option<StreamId>,
                    code: u32) -> Result<()> {
        let stream_ids = if let Some(stream_id) = stream_id {
            //关闭指定的流
            vec![stream_id]
        } else {
            //关闭所有的流
            let mut ids = self.get_stream_ids();
            ids.reverse(); //倒序关闭所有流
            ids
        };

        for stream_id in stream_ids {
            //关闭接收流
            if let Err(e) = self.connect.recv_stream(stream_id).stop(VarInt::from_u32(code)) {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Shutdown quic recv stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
                                              self.get_uid(),
                                              self.get_remote(),
                                              self.get_local(),
                                              stream_id,
                                              e)));
            }

            //关闭发送流
            if let Err(e) = self.connect.send_stream(stream_id).finish() {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Shutdown quic send stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
                                              self.get_uid(),
                                              self.get_remote(),
                                              self.get_local(),
                                              stream_id,
                                              e)));
            }
        }

        Ok(())
    }

    /// 强制关闭当前连接
    pub fn force_close(&mut self, code: u32, reason: String) {
        self
            .connect
            .close(self.clock,
                   VarInt::from_u32(code),
                   Bytes::from(reason));
    }
}

///
/// Quic连接流
///
pub struct QuicStream {
    stream_type:        Dir,                                                //流类型
    wait_recv_len:      UnsafeCell<usize>,                                  //流需要接收的字节数
    recv_len:           UnsafeCell<usize>,                                  //流已接收的字节数
    read_len:           Arc<AtomicUsize>,                                   //流读取块大小
    readed_read_limit:  Arc<AtomicUsize>,                                   //已读读缓冲大小限制
    readed:             UnsafeCell<usize>,                                  //已读读缓冲当前大小
    read_buf:           Arc<SpinLock<Option<BytesMut>>>,                    //流读缓冲
    wait_ready_len:     UnsafeCell<usize>,                                  //流异步准备读取的字节数
    ready_len:          UnsafeCell<usize>,                                  //流异步准备读取已就绪的字节数
    ready_reader:       SpinLock<Option<AsyncValueNonBlocking<usize>>>,     //异步准备读取器
    wait_sent_len:      AtomicUsize,                                        //流需要发送的字节数
    sent_len:           UnsafeCell<usize>,                                  //流已发送的字节数
    write_len:          Arc<AtomicUsize>,                                   //流写入块大小
    readed_write_limit: Arc<AtomicUsize>,                                   //已读写缓冲大小限制
    readed_write_len:   UnsafeCell<usize>,                                  //已读写缓冲大小
    write_buf:          UnsafeCell<Option<BytesMut>>,                       //流写缓冲
}

unsafe impl Send for QuicStream {}
unsafe impl Sync for QuicStream {}

impl QuicStream {
    /// 构建一个Quic连接流
    pub(crate) fn new(stream_type: Dir,
                      readed_read_size_limit: usize,
                      readed_write_size_limit: usize) -> Self {
        let read_len = Arc::new(AtomicUsize::new(DEFAULT_READ_BLOCK_LEN));
        let readed_read_limit = Arc::new(AtomicUsize::new(readed_read_size_limit));
        let read_buf = Arc::new(SpinLock::new(Some(BytesMut::new())));
        let ready_reader = SpinLock::new(None);
        let wait_sent_len = AtomicUsize::new(0);
        let write_len = Arc::new(AtomicUsize::new(DEFAULT_WRITE_BLOCK_LEN));
        let readed_write_limit = Arc::new(AtomicUsize::new(readed_write_size_limit));
        let write_buf = UnsafeCell::new(Some(BytesMut::new()));

        QuicStream {
            stream_type,
            wait_recv_len: UnsafeCell::new(0),
            recv_len: UnsafeCell::new(0),
            read_len,
            readed_read_limit,
            readed: UnsafeCell::new(0),
            read_buf,
            wait_ready_len: UnsafeCell::new(0),
            ready_len: UnsafeCell::new(0),
            ready_reader,
            wait_sent_len,
            sent_len: UnsafeCell::new(0),
            write_len,
            readed_write_limit,
            readed_write_len: UnsafeCell::new(0),
            write_buf,
        }
    }
}

