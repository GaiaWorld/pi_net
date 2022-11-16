use std::rc::Rc;
use std::task::Waker;
use std::time::Instant;
use std::future::Future;
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::result::Result as GenResult;
use std::io::{Cursor, Result, Error, ErrorKind};
use std::sync::{Arc, atomic::{AtomicU8, AtomicUsize, Ordering}};

use quinn_proto::{ConnectionHandle, Connection, Transmit, StreamId, SendStream, RecvStream, ReadError, WriteError, VarInt, Dir};
use crossbeam_channel::Sender;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::future::{FutureExt, LocalBoxFuture};

use pi_async::{lock::spin_lock::SpinLock,
               rt::{serial::AsyncValue,
                    serial_local_thread::LocalTaskRuntime}};

use udp::{Socket, SocketHandle};

use crate::{SocketHandle as QuicSocketHandle, SocketEvent, utils::{QuicSocketStatus, QuicSocketReady, QuicCloseEvent, Hibernate}};

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

///
/// Quic连接
///
pub struct QuicSocket<S: Socket> {
    rt:                 Option<LocalTaskRuntime<()>>,                                       //连接所在运行时
    uid:                usize,                                                              //连接唯一id
    status:             Arc<AtomicU8>,                                                      //连接状态
    udp_handle:         SocketHandle<S>,                                                    //Udp连接句柄
    handle:             ConnectionHandle,                                                   //Quic内部连接句柄
    connect:            Connection,                                                         //Quic内部连接
    main_stream_id:     Option<StreamId>,                                                   //连接的主流唯一id
    write_sent:         Option<Sender<(ConnectionHandle, Transmit)>>,                       //Socket发送事件发送器
    ready_sent:         Option<Sender<(ConnectionHandle, QuicSocketReady)>>,                //连接流就绪请求发送器
    clock:              Instant,                                                            //内部时钟
    expired:            Option<Instant>,                                                    //下次到期时间
    timer_ref:          Option<u64>,                                                        //当前定时器
    timer_listener:     Option<Sender<(ConnectionHandle, Option<(u64, SocketEvent)>)>>,     //定时事件监听器
    wait_recv_len:      usize,                                                              //连接需要接收的字节数
    recv_len:           usize,                                                              //连接已接收的字节数
    read_len:           Arc<AtomicUsize>,                                                   //连接读取块大小
    readed_read_limit:  Arc<AtomicUsize>,                                                   //已读读缓冲大小限制
    readed:             usize,                                                              //已读读缓冲当前大小
    read_buf:           Rc<UnsafeCell<Option<BytesMut>>>,                                   //连接流读缓冲
    wait_ready_len:     usize,                                                              //连接异步准备读取的字节数
    ready_len:          usize,                                                              //连接异步准备读取已就绪的字节数
    ready_reader:       SpinLock<Option<AsyncValue<usize>>>,                                //异步准备读取器
    wait_sent_len:      AtomicUsize,                                                        //连接需要发送的字节数
    sent_len:           usize,                                                              //连接已发送的字节数
    write_len:          Arc<AtomicUsize>,                                                   //连接写入块大小
    readed_write_limit: Arc<AtomicUsize>,                                                   //已读写缓冲大小限制
    readed_write_len:   usize,                                                              //已读写缓冲大小
    write_listener:     Option<Sender<(ConnectionHandle, Option<StreamId>, Vec<u8>)>>,      //连接写事件监听器
    write_buf:          Option<BytesMut>,                                                   //连接写缓冲
    hibernate:          SpinLock<Option<Hibernate<S>>>,                                     //连接异步休眠对象
    hibernate_wakers:   SpinLock<VecDeque<Waker>>,                                          //连接正在休眠时，其它休眠对象的唤醒器队列
    hibernated_queue:   Arc<SpinLock<VecDeque<LocalBoxFuture<'static, ()>>>>,               //连接休眠时任务队列
    socket_handle:      Option<QuicSocketHandle<S>>,                                        //Quic连接句柄
    close_reason:       Option<(u32, Result<()>)>,                                          //连接关闭的原因
    close_listener:     Option<Sender<QuicCloseEvent>>,                                     //连接关闭事件监听器
}

impl<S: Socket> QuicSocket<S> {
    /// 构建一个Quic连接
    pub fn new(udp_handle: SocketHandle<S>,
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

        let status = Arc::new(AtomicU8::new(QuicSocketStatus::UdpAccepted.into()));
        let read_len = Arc::new(AtomicUsize::new(DEFAULT_READ_BLOCK_LEN));
        let readed_read_limit = Arc::new(AtomicUsize::new(readed_read_size_limit));
        let read_buf = Rc::new(UnsafeCell::new(Some(BytesMut::new())));
        let ready_reader = SpinLock::new(None);
        let wait_sent_len = AtomicUsize::new(0);
        let write_len = Arc::new(AtomicUsize::new(DEFAULT_WRITE_BLOCK_LEN));
        let readed_write_limit = Arc::new(AtomicUsize::new(readed_write_size_limit));
        let write_buf = Some(BytesMut::new());
        let hibernate = SpinLock::new(None);
        let hibernate_wakers = SpinLock::new(VecDeque::new());
        let hibernated_queue = Arc::new(SpinLock::new(VecDeque::new()));

        QuicSocket {
            rt: None,
            uid: handle.0,
            status,
            udp_handle,
            handle,
            connect,
            main_stream_id: None,
            write_sent: None,
            ready_sent: None,
            clock,
            expired: None,
            timer_ref: None,
            timer_listener: None,
            wait_recv_len: 0,
            recv_len: 0,
            read_len,
            readed_read_limit,
            readed: 0,
            read_buf,
            wait_ready_len: 0,
            ready_len: 0,
            ready_reader,
            wait_sent_len,
            sent_len: 0,
            write_len,
            readed_write_limit,
            readed_write_len: 0,
            write_listener: None,
            write_buf,
            hibernate,
            hibernate_wakers,
            hibernated_queue,
            socket_handle: None,
            close_reason: None,
            close_listener: None,
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

    /// 获取连接状态
    pub fn get_status(&self) -> QuicSocketStatus {
        self.status.load(Ordering::Acquire).into()
    }

    /// 设置连接状态
    pub fn set_status(&mut self, status: QuicSocketStatus) {
        self.status.store(status.into(), Ordering::Release);
    }

    /// 获取本地连接地址
    pub fn get_local(&self) -> Option<IpAddr> {
        self.connect.local_ip()
    }

    /// 获取远端连接地址
    pub fn get_remote(&self) -> SocketAddr {
        self.connect.remote_address()
    }

    /// 判断当前连接是否通过0rtt建立连接
    pub fn is_0rtt(&self) -> bool {
        self.connect.is_handshaking()
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
    pub fn open_main_streams(&mut self) -> Result<()> {
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

    /// 设置连接的写入Quic数据报文发送器
    pub fn set_write_sent(&mut self, write_sent: Option<Sender<(ConnectionHandle, Transmit)>>) {
        self.write_sent = write_sent;
    }

    /// 设置连接的连接流就绪请求发送器
    pub fn set_ready_sent(&mut self, ready_sent: Option<Sender<(ConnectionHandle, QuicSocketReady)>>) {
        self.ready_sent = ready_sent;
    }

    /// 设置写事件监听器
    pub fn set_write_listener(&mut self,
                              listener: Option<Sender<(ConnectionHandle, Option<StreamId>, Vec<u8>)>>) {
        self.write_listener = listener;
    }

    /// 获取Udp连接句柄的只读引用
    pub fn get_udp_handle(&self) -> &SocketHandle<S> {
        &self.udp_handle
    }

    /// 判断当前连接是否是客户端连接
    pub fn is_client(&self) -> bool {
        self.udp_handle.get_remote().is_some()
    }

    /// 获取Quic连接句柄
    pub fn get_socket_handle(&self) -> QuicSocketHandle<S> {
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
                                                  self.timer_listener.as_ref().unwrap().clone(),
                                                  self.close_listener.as_ref().unwrap().clone());

        self.socket_handle = Some(socket_handle);
    }

    /// 移除Quic连接句柄
    pub fn remove_socket_handle(&mut self) -> Option<QuicSocketHandle<S>> {
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

    /// 获取当前定时器
    pub fn get_timer_ref(&self) -> Option<u64> {
        self.timer_ref
    }

    /// 设置定时器
    pub fn set_timer_ref(&mut self, timer_ref: Option<u64>) {
        self.timer_ref = timer_ref;
    }

    /// 移除定时器
    pub fn unset_timer_ref(&mut self) -> Option<u64> {
        self.timer_ref.take()
    }

    /// 设置定时器监听器
    pub fn set_timer_listener(&mut self,
                              listener: Option<Sender<(ConnectionHandle, Option<(u64, SocketEvent)>)>>) {
        self.timer_listener = listener;
    }

    /// 设置关闭事件监听器
    pub fn set_close_listener(&mut self,
                              listener: Option<Sender<QuicCloseEvent>>) {
        self.close_listener = listener;
    }

    /// 写入指定的Quic数据报文
    pub fn write_transmit(&self, data: Transmit) -> Result<()> {
        if let Some(write_sent) = &self.write_sent {
            if let Err(e) = write_sent.send((self.handle, data)) {
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
                if let Err(e) = self.udp_handle.write_ready(Cursor::new((&data.contents[offset..offset + size]).to_vec()), Some(data.destination)) {
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
            if let Err(e) = self.udp_handle.write_ready(Cursor::new((&data.contents[offset..offset + len]).to_vec()), Some(data.destination)) {
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
            if let Err(e) = self.udp_handle.write_ready(Cursor::new(data.contents), Some(data.destination)) {
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
    pub fn set_ready(&self, ready: QuicSocketReady) {
        if let Some(sender) = &self.ready_sent {
            sender.send((self.handle, ready));
        }
    }

    /// 获取读取的块大小
    pub fn get_read_block_len(&self) -> usize {
        self.read_len.load(Ordering::Acquire)
    }

    /// 设置读取的块大小
    pub fn set_read_block_len(&self, len: usize) {
        self.read_len.store(len, Ordering::Release);
    }

    /// 获取连接指定流的接收流
    #[inline]
    pub fn get_recv_stream(&mut self, stream_id: StreamId) -> RecvStream<'_> {
        self.connect.recv_stream(stream_id)
    }

    /// 是否需要继续从连接流中接收数据
    #[inline]
    pub fn is_require_recv(&self) -> bool {
        self.wait_recv_len > self.recv_len
    }

    /// 接收流中的数据，返回成功，则表示本次接收了需要的字节数，并返回本次接收的字节数，否则返回接收错误
    pub fn recv(&mut self) -> Result<usize> {
        if self.is_closed() {
            //连接正在关闭或已关闭，则忽略，并立即返回
            return Err(Error::new(ErrorKind::ConnectionAborted,
                                  format!("Receive stream failed, uid: {:?}, peer: {:?}, local: {:?}, reason: connection already closed",
                                          self.get_uid(),
                                          self.get_remote(),
                                          self.get_local())));
        }

        let uid = self.get_uid();
        let remote = self.get_remote();
        let local = self.get_local();
        let mut result = Ok(0); //初始化本次接收的结果值
        match self.connect.recv_stream(self.get_main_stream_id().unwrap().clone()).read(false) {
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
                let mut blocks_len = 0; //初始化本次接收的块大小
                let mut blocks: Vec<(u64, Bytes)> = Vec::new(); //初始化本次接收的块

                loop {
                    match chunks.next(usize::MAX) {
                        Ok(Some(chunk)) => {
                            //部分接收成功，则继续接收，直到接收完流的当前的所有数据
                            let block_len = chunk.bytes.len(); //部分接收成功的块大小
                            self.recv_len += block_len; //增加连接已接收的字节数
                            if self.ready_reader.lock().is_some() {
                                //当前连接设置了异步准备接收器，则记录本次成功接收的字节数
                                self.ready_len += block_len;
                            }
                            blocks_len += block_len; //增加已接收块的大小

                            blocks.push((chunk.offset, chunk.bytes));
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
                                if let Some(buf) = unsafe { &mut *self.read_buf.get() } {
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
                chunks.finalize(); //完成本次流的所有数据的接收

                if self.wait_recv_len > self.recv_len {
                    //本次接收已完成，但当前连接未接收足够的数据，则设置连接需要继续处理接收事件
                    if let Some(sender) = &self.ready_sent {
                        sender.send((self.handle, QuicSocketReady::Readable));
                    }
                }
            }
        }

        if result.is_ok() {
            //本次接收成功
            if self.readed > self.readed_read_limit.load(Ordering::Relaxed) {
                //本次接收成功，且已达已读读缓冲大小限制，则清理已读取的读缓冲区，并释放对应的内存
                unsafe {
                    let old_buf = (&mut *self.read_buf.get()).take().unwrap();
                    let mut new_buf = BytesMut::with_capacity(old_buf.remaining());
                    new_buf.put(old_buf);
                    *self.read_buf.get() = Some(new_buf);
                    self.readed = 0;
                }
            }
        }

        //返回本次流接收的结果
        result
    }

    /// 通知连接读就绪，可以开始接收指定字节数的数据，如果当前需要等待接收则返回AsyncValue, 否则返回接收缓冲区中已有数据的字节数
    /// 设置准备读取的字节大小为0，则表示准备接收任意数量的字节，直到当前连接的流没有可接收的数据
    /// 设置准备读取的字节大小大于0，则表示至少需要接收指定数量的字节，如果还未接收到指定数量的字节，则继续从流中接收
    /// 异步阻塞读取读缓冲前应该保证调用此函数对读缓冲进行填充，避免异步读取被异步阻塞
    /// 返回0长度，表示当前连接已关闭，继续操作将是未定义行为
    /// 注意调用此方法，在保持连接的前提下，必须保证后续一定还可以接收到数据，否则会导致无法唤醒当前异步准备读取器
    pub fn read_ready(&mut self, adjust: usize) -> GenResult<AsyncValue<usize>, usize> {
        if self.is_closed() {
            //连接已关闭，则忽略，并立即返回
            return Err(0);
        }

        self.wait_recv_len += adjust; //增加连接需要接收的字节数
        self.set_ready(QuicSocketReady::Readable); //设置连接当前对读事件感兴趣

        let remaining = unsafe {
            (&*self.read_buf.get())
                .as_ref()
                .unwrap()
                .remaining()
        };
        if remaining >= adjust && remaining > 0 {
            //连接当前读取缓冲区有足够的数据，则立即返回当前读取缓冲区中剩余可读字节的数量
            return Err(remaining);
        }

        //连接当前读缓冲区没有足够的数据，则只读需要的字节数
        let value = AsyncValue::new();
        let value_copy = value.clone();
        *self.ready_reader.lock() = Some(value); //设置当前连接的异步准备读取器
        self.wait_ready_len = adjust - remaining; //设置本次异步准备读取实际需要的字节数

        Ok(value_copy)
    }

    /// 判断当前连接是否有异步准备读取器
    pub fn is_wait_wakeup_read_ready(&self) -> bool {
        self.ready_reader.lock().is_some()
    }

    /// 唤醒当前连接异步阻塞的读就绪，如果当前连接没有异步准备读取器则忽略
    pub fn wakeup_read_ready(&mut self) {
        if (self.wait_ready_len == 0) || (self.wait_ready_len <= self.ready_len) {
            //已完成异步准备读取指定的字节数
            if let Some(ready_reader) = self.ready_reader.lock().take() {
                //当前连接已设置了异步准备读取器，则立即移除并唤醒当前异步准备读取器
                ready_reader.set(self.ready_len); //设置实际读取的字节数
                self.wait_ready_len = 0; //重置异步准备读取的字节数
                self.ready_len = 0; //重置异步准备读取已就绪的字节数
            }
        }
    }

    /// 获取连接的输入缓冲区的只读引用
    pub fn get_read_buffer(&self) -> Rc<UnsafeCell<Option<BytesMut>>> {
        self.read_buf.clone()
    }

    /// 获取写入的块大小
    pub fn get_write_block_len(&self) -> usize {
        self.write_len.load(Ordering::Acquire)
    }

    /// 设置写入的块大小
    pub fn set_write_block_len(&self, len: usize) {
        self.write_len.store(len, Ordering::Release);
    }

    /// 获取连接指定流的发送流
    #[inline]
    pub fn get_send_stream(&mut self, stream_id: StreamId) -> SendStream<'_> {
        self.connect.send_stream(stream_id)
    }

    /// 发送数据到流，返回成功，则返回本次发送了多少字节数，否则返回发送错误原因
    pub fn send(&mut self) -> Result<usize> {
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
        let mut block_len = self.get_write_block_len(); //本次发送的限制

        if let Some(write_buf) = self.get_write_buffer() {
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
                match self.get_send_stream(self.main_stream_id.unwrap().clone()).write(block) {
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
                        self.sent_len += len; //增加连接已发送的字节数
                        self.readed_write_len += len; //增加已读写缓冲大小的字节数
                        block_pos += len; //增加接收块的已填充位置

                        block = &block[block_pos..]; //设置需要继续发送的块
                        result = Ok(block_pos);
                    },
                    Err(e) if e == WriteError::Blocked => {
                        //本次发送已阻塞，则退出本次发送
                        if !block.is_empty() {
                            //当前发送块中还有未发送的数据，则重新写入写缓冲区
                            //注意这是因为每次发送都会消耗当前写缓冲区中的所有数据，所以可以将未发送完的数据重新写入写缓冲区
                            if let Some(write_buf) = &mut self.write_buf {
                                write_buf.put_slice(&block[block_pos..]);
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
        if (block_pos > 0)
            && (self.readed_write_len > self.readed_write_limit.load(Ordering::Relaxed)) {
            //本次发送成功，已达已读写缓冲大小限制，则清理已发送的写缓冲区，并释放对应的内存
            let old_write_buf = self
                .write_buf
                .take()
                .unwrap();
            let mut new_write_buf = BytesMut::new();
            new_write_buf.put(old_write_buf);
            self.readed_write_len = 0; //重置已读写缓冲大小
            self.write_buf = Some(new_write_buf); //重置写缓冲区
        }

        if block_pos < block_len {
            //本次发送部分成功，则增加连接当前对写事件感兴趣
            self.set_ready(QuicSocketReady::Writable);
        } else {
            //本次发送全部成功或完全没有发送
            if let Some(write_buf) = &mut self.write_buf {
                if write_buf.remaining() > 0
                    || self.wait_sent_len.load(Ordering::Relaxed) > self.sent_len {
                    //连接的当前写缓冲区中还有待发送的数据，或者还有未填充到写缓冲区的的待发送数据，则增加连接当前只对写事件感兴趣
                    self.set_ready(QuicSocketReady::Writable);
                } else {
                    //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                    self.set_ready(QuicSocketReady::Readable);
                }
            } else {
                //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                self.set_ready(QuicSocketReady::Readable);
            }
        }

        result
    }

    /// 获取连接的输出缓冲区的可写引用
    #[inline]
    pub fn get_write_buffer(&mut self) -> Option<&mut BytesMut> {
        self.write_buf.as_mut()
    }

    /// 通知连接写就绪，可以开始发送指定的数据
    pub fn write_ready<B>(&mut self, buf: B) -> Result<()>
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
        self.wait_sent_len
            .fetch_add(buf.as_ref().len(), Ordering::Relaxed); //首先要同步增加需要发送的字节数
        if let Some(listener) = &self.write_listener {
            listener.send((self.handle, None, buf.as_ref().to_vec()));
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
                     handle: QuicSocketHandle<S>,
                     ready: QuicSocketReady) -> Option<Hibernate<S>> {
        if self.is_closed() {
            //连接已关闭，则立即返回空
            return None;
        }

        let hibernate = Hibernate::new(handle, ready);
        let hibernate_copy = hibernate.clone();

        Some(hibernate_copy)
    }

    /// 设置当前连接的休眠对象，设置成功返回真
    pub fn set_hibernate(&self, hibernate: Hibernate<S>) -> bool {
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

        if let Some(value) = self.ready_reader.lock().take() {
            //当前连接设置了异步准备读取器，则立即移除并唤醒当前异步准备读取器，并设置读取的长度为0
            value.set(0);
            self.wait_ready_len = 0; //重置异步准备读取的字节数
            self.ready_len = 0; //重置异步准备读取已就绪的字节数
        }
        self.close_reason = Some((code, reason)); //设置连接关闭的原因

        //通知连接关闭
        if let Some(listener) = &self.close_listener {
            if let Err(e) = listener.send(QuicCloseEvent::CloseStream(self.get_connection_handle().clone(), None, code)) {
                return Err(Error::new(ErrorKind::BrokenPipe, e));
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

    /// 关闭当前连接的主流
    pub fn shutdown(&mut self, code: u32) -> Result<()> {
        let main_stream_id = self.main_stream_id.unwrap();

        //关闭接收流
        if let Err(e) = self.connect.recv_stream(main_stream_id).stop(VarInt::from_u32(code)) {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Shutdown quic recv stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
                                      self.get_uid(),
                                      self.get_remote(),
                                      self.get_local(),
                                      main_stream_id,
                                      e)));
        }

        //关闭发送流
        if let Err(e) = self.connect.send_stream(main_stream_id).finish() {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Shutdown quic send stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
                                          self.get_uid(),
                                          self.get_remote(),
                                          self.get_local(),
                                          main_stream_id,
                                          e)));
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