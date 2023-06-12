use std::rc::Rc;
use std::fs::File;
use std::path::Path;
use std::task::Waker;
use std::future::Future;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::result::Result as GenResult;
use std::cell::{Ref, RefCell, UnsafeCell};
use std::io::{Error, Result, ErrorKind, Read, Write, BufReader};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};

use mio::{Token, Interest, Poll,
          net::TcpStream};
use futures::{sink::SinkExt,
              future::{FutureExt, LocalBoxFuture}};
use crossbeam_channel::Sender;
use bytes::{Buf, BufMut, BytesMut};
use rustls::{ClientConnection, ServerConnection};
use log::warn;

use pi_async::{lock::spin_lock::SpinLock,
               rt::{serial::AsyncValue,
                    serial_local_thread::LocalTaskRuntime,
                    async_pipeline::{AsyncReceiverExt, AsyncPipeLineExt, PipeSender, channel}}};
use pi_async_buffer::ByteBuffer;

use crate::{Stream, Socket, SocketEvent, SocketHandle, SocketImage, SocketContext,
            utils::{TlsConfig, SharedStream, Hibernate, Ready}};

/// 默认的读取块大小，单位字节
const DEFAULT_READ_BLOCK_LEN: usize = 4096;

/// 默认的写入块大小，单位字节
const DEFAULT_WRITE_BLOCK_LEN: usize = 4096;

/// 默认的读取块每次扩容大小，单位字节
const DEFAULT_READ_BLOCK_ADJUST_LEN: usize = 1024;

/// 默认的Tcp接收帧缓冲数量，单位帧
const DEAFULT_RECV_FRAME_BUF_SIZE: usize = 16;

/// 最小的Tcp已读读缓冲大小限制，单位字节
const MIN_READED_READ_BUF_SIZE_LIMIT: usize = DEFAULT_READ_BLOCK_LEN;

/// 最小的Tcp已读写缓冲大小限制，单位字节
const MIN_READED_WRITE_BUF_SIZE_LIMIT: usize = DEFAULT_WRITE_BLOCK_LEN;

/// 默认的Tcp已读读缓冲大小限制，单位字节
const DEAFULT_READED_READ_BUF_SIZE_LIMIT: usize = 256 * 1024;

/// 默认的Tcp已读写缓冲大小限制，单位字节
const DEAFULT_READED_WRITE_BUF_SIZE_LIMIT: usize = 256 * 1024;

/// 默认的Tls写缓冲大小倍数
const DEFAULT_TLS_WRITE_BUF_SIZE_MULTIPLE: f64 = 2.0;

///
/// Tls连接
///
pub struct TlsSocket {
    rt:                 Option<LocalTaskRuntime<()>>,                           //连接所在运行时
    uid:                Option<usize>,                                          //连接唯一id
    local:              SocketAddr,                                             //连接本地地址
    remote:             SocketAddr,                                             //连接远端地址
    token:              Option<Token>,                                          //连接令牌
    stream:             TcpStream,                                              //连接流
    tls_connect:        TlsConnect,                                             //Tls内部连接
    interest:           Arc<SpinLock<Interest>>,                                //连接当前感兴趣的事件类型
    wait_recv_len:      usize,                                                  //连接需要接收的字节数
    recv_len:           usize,                                                  //连接已接收的字节数
    read_len:           Arc<AtomicUsize>,                                       //连接读取块大小
    readed_read_limit:  Arc<AtomicUsize>,                                       //已读读缓冲大小限制
    readed:             usize,                                                  //已读读缓冲当前大小
    read_buf:           Rc<UnsafeCell<Option<BytesMut>>>,                       //连接读缓冲
    wait_ready_len:     usize,                                                  //连接异步准备读取的字节数
    ready_len:          usize,                                                  //连接异步准备读取已就绪的字节数
    ready_reader:       SpinLock<Option<AsyncValue<usize>>>,                    //异步准备读取器
    wait_sent_len:      AtomicUsize,                                            //连接需要发送的字节数
    sent_len:           usize,                                                  //连接已发送的字节数
    write_len:          Arc<AtomicUsize>,                                       //连接写入块大小
    readed_write_limit: Arc<AtomicUsize>,                                       //已读写缓冲大小限制
    readed_write_len:   usize,                                                  //已读写缓冲大小
    write_buf:          Option<BytesMut>,                                       //连接写缓冲
    poll:               Option<Rc<UnsafeCell<Poll>>>,                          //连接所在轮询器
    hibernate:          SpinLock<Option<Hibernate<Self>>>,                      //连接异步休眠对象
    hibernate_wakers:   SpinLock<VecDeque<Waker>>,                              //连接正在休眠时，其它休眠对象的唤醒器队列
    hibernated_queue:   Arc<SpinLock<VecDeque<LocalBoxFuture<'static, ()>>>>,   //连接休眠时任务队列
    handle:             Option<SocketHandle<Self>>,                             //连接句柄
    context:            Rc<UnsafeCell<SocketContext>>,                          //连接上下文
    write_listener:     Option<Sender<(Token, Vec<u8>)>>,                       //连接写事件监听器
    closed:             Arc<AtomicBool>,                                        //连接关闭状态
    close_listener:     Option<Sender<(Token, Result<()>)>>,                    //连接关闭事件监听器
    timer_handle:       Option<usize>,                                          //定时器句柄
    timer_listener:     Option<Sender<(Token, Option<(usize, SocketEvent)>)>>,  //定时事件监听器
}

unsafe impl Send for TlsSocket {}
unsafe impl Sync for TlsSocket {}

impl Stream for TlsSocket {
    fn new(local: &SocketAddr,
           remote: &SocketAddr,
           token: Option<Token>,
           stream: TcpStream,
           recv_frame_buf_size: usize,
           readed_read_size_limit: usize,
           readed_write_size_limit: usize,
           tls_cfg: TlsConfig) -> Self {
        let recv_frame_buf_size = if recv_frame_buf_size == 0 {
            //Tcp接收缓冲帧数量过小
            DEAFULT_RECV_FRAME_BUF_SIZE
        } else {
            recv_frame_buf_size
        };

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

        let tls_connect = match tls_cfg {
            TlsConfig::Client(cfg) => {
                //使用指定Tls配置，构建客户端Tls连接
                unimplemented!();
            },
            TlsConfig::Server(cfg) => {
                //使用指定Tls配置，构建服务端Tls连接
                let connection = ServerConnection::new(cfg)
                    .expect(format!("Create Tls socket failed, token: {:?}, remote: {:?}, local: {:?}",
                                    token,
                                    remote,
                                    local).as_str());
                TlsConnect::Server(connection)
            },
            TlsConfig::Empty => {
                //无效配置
                panic!("Create Tls socket failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid tls config",
                       token,
                       remote,
                       local);
            },
        };

        let interest = Arc::new(SpinLock::new(Interest::READABLE));
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
        let context = Rc::new(UnsafeCell::new(SocketContext::empty()));
        let closed = Arc::new(AtomicBool::new(false));

        let mut result = TlsSocket {
            rt: None,
            uid: None,
            local: local.clone(),
            remote: remote.clone(),
            token,
            stream,
            tls_connect,
            interest,
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
            write_buf,
            poll: None,
            hibernate,
            hibernate_wakers,
            hibernated_queue,
            handle: None,
            context,
            write_listener: None,
            closed,
            close_listener: None,
            timer_handle: None,
            timer_listener: None,
        };
        result.set_interest(result.event_set()); //设置Tls连接初始感兴趣的事件

        result
    }

    fn set_runtime(&mut self, rt: LocalTaskRuntime<()>) {
        self.rt = Some(rt);
    }

    fn set_handle(&mut self, shared: &Arc<UnsafeCell<Self>>) {
        if let Some(token) = self.token {
            if let Some(close_listener) = &self.close_listener {
                if let Some(timer_listener) = &self.timer_listener {
                    let image = SocketImage::new(shared,
                                                 self.local,
                                                 self.remote,
                                                 token,
                                                 self.uid.unwrap(),
                                                 self.is_security(),
                                                 self.closed.clone(),
                                                 close_listener.clone(),
                                                 timer_listener.clone()
                    );
                    self.handle = Some(SocketHandle::new(image));
                }
            }
        }
    }

    #[inline]
    fn get_stream_ref(&self) -> &TcpStream {
        &self.stream
    }

    #[inline]
    fn get_stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    fn set_token(&mut self, token: Option<Token>) -> Option<Token> {
        let last = self.token.take();
        self.token = token;
        last
    }

    fn set_uid(&mut self, uid: usize) -> Option<usize> {
        let last = self.uid.take();
        self.uid = Some(uid);
        last
    }

    fn get_interest(&self) -> Option<Interest> {
        Some({ *self.interest.lock() })
    }

    fn set_interest(&self, interest: Interest) {
        *self.interest.lock() = interest;
    }

    #[inline]
    fn get_read_block_len(&self) -> usize {
        self.read_len.load(Ordering::Acquire)
    }

    fn set_read_block_len(&self, len: usize) {
        self.read_len.store(len, Ordering::Release);
    }

    fn get_write_block_len(&self) -> usize {
        self.write_len.load(Ordering::Acquire)
    }

    fn set_write_block_len(&self, len: usize) {
        self.write_len.store(len, Ordering::Release);
    }

    fn set_poll(&mut self, poll: Rc<UnsafeCell<Poll>>) {
        self.poll = Some(poll);
    }

    fn set_write_listener(&mut self,
                          listener: Option<Sender<(Token, Vec<u8>)>>) {
        self.write_listener = listener;
    }

    fn set_close_listener(&mut self,
                          listener: Option<Sender<(Token, Result<()>)>>) {
        self.close_listener = listener;
    }

    fn set_timer_listener(&mut self, listener: Option<Sender<(Token, Option<(usize, SocketEvent)>)>>) {
        self.timer_listener = listener;
    }

    fn set_timer_handle(&mut self, timer_handle: usize) -> Option<usize> {
        let last_timer_handle = self.unset_timer_handle();
        self.timer_handle = Some(timer_handle);
        last_timer_handle
    }

    fn unset_timer_handle(&mut self) -> Option<usize> {
        self.timer_handle.take()
    }

    #[inline]
    fn is_require_recv(&self) -> bool {
        self.wait_recv_len > self.recv_len
    }

    fn recv(&mut self) -> Result<usize> {
        self.do_tls_read()?;
        let result = self.try_plain_read();
        self.set_interest(self.event_set()); //设置当前连接感兴趣的事件

        match &self.tls_connect {
            TlsConnect::Client(con) => {
                if !con.is_handshaking() {
                    //当前客户端Tls连接已握手
                    if !self.is_require_recv() {
                        //本次接收已完成，且当前连接已接收了足够的数据，则设置连接暂时不需要处理接收事件
                        self.set_interest(Interest::WRITABLE);
                    }
                }
            },
            TlsConnect::Server(con) => {
                if !con.is_handshaking() {
                    //当前服务端Tls连接已握手
                    if !self.is_require_recv() {
                        //本次接收已完成，且当前连接已接收了足够的数据，则设置连接暂时不需要处理接收事件
                        self.set_interest(Interest::WRITABLE); //设置连接暂时不需要处理接收事件
                    }
                }
            },
        }

        result
    }

    fn send(&mut self) -> Result<usize> {
        if let Err(e) = self.try_plain_write() {
            //尝试写入明文到Tls连接写缓冲错误，则立即返回错误
            Err(e)
        } else {
            let result = self.do_tls_write();
            self.set_interest(self.event_set()); //设置当前连接感兴趣的事件
            match &self.tls_connect {
                TlsConnect::Client(con) => {
                    if !con.is_handshaking() {
                        //当前客户端Tls连接已握手，且本次发送全部成功或完全没有发送
                        if let Some(write_buf) = &self.write_buf {
                            if write_buf.remaining() > 0
                                || self.wait_sent_len.load(Ordering::Relaxed) > self.sent_len {
                                //连接的当前写缓冲区中还有待发送的数据，或者还有未填充到写缓冲区的的待发送数据，则增加连接当前只对写事件感兴趣
                                self.set_interest(Interest::WRITABLE);
                            } else {
                                //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                                self.set_interest(Interest::READABLE);
                            }
                        } else {
                            //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                            self.set_interest(Interest::READABLE);
                        }
                    }
                },
                TlsConnect::Server(con) => {
                    if !con.is_handshaking() {
                        //当前服务端Tls连接已握手，且本次发送全部成功或完全没有发送
                        if let Some(write_buf) = &self.write_buf {
                            if write_buf.remaining() > 0
                                || self.wait_sent_len.load(Ordering::Relaxed) > self.sent_len {
                                //连接的当前写缓冲区中还有待发送的数据，或者还有未填充到写缓冲区的的待发送数据，则增加连接当前只对写事件感兴趣
                                self.set_interest(Interest::WRITABLE);
                            } else {
                                //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                                self.set_interest(Interest::READABLE);
                            }
                        } else {
                            //连接的当前写缓冲区中没有待发送的数据，则增加连接当前只对读事件感兴趣
                            self.set_interest(Interest::READABLE);
                        }
                    }
                },
            }

            result
        }
    }
}

impl Socket for TlsSocket {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn is_flush(&self) -> bool {
        true
    }

    fn set_flush(&self, flush: bool) {

    }

    fn get_handle(&self) -> SocketHandle<Self> {
        self
            .handle
            .as_ref()
            .unwrap()
            .clone()
    }

    fn remove_handle(&mut self) -> Option<SocketHandle<Self>> {
        self.handle.take()
    }

    fn get_local(&self) -> &SocketAddr {
        &self.local
    }

    fn get_remote(&self) -> &SocketAddr {
        &self.remote
    }

    fn get_token(&self) -> Option<&Token> {
        self.token.as_ref()
    }

    fn get_uid(&self) -> Option<&usize> {
        self.uid.as_ref()
    }

    fn get_context(&self) -> Rc<UnsafeCell<SocketContext>> {
        self.context.clone()
    }

    fn set_timeout(&self, timeout: usize, event: SocketEvent) {
        if let Some(listener) = &self.timer_listener {
            if let Some(token) = self.token {
                listener.send((token, Some((timeout, event))));
            }
        }
    }

    fn unset_timeout(&self) {
        if let Some(listener) = &self.timer_listener {
            if let Some(token) = self.token {
                listener.send((token, None));
            }
        }
    }

    fn is_security(&self) -> bool {
        true
    }

    fn read_ready(&mut self, adjust: usize) -> GenResult<AsyncValue<usize>, usize> {
        self.wait_recv_len += adjust; //增加连接需要接收的字节数
        let interest = *self.interest.lock(); //保证在调用set_interest时已释放RefCell的只读引用
        self.set_interest(interest.add(Interest::READABLE)); //设置连接当前对读事件感兴趣

        let remaining = unsafe {
            (&*self.read_buf.get())
                .as_ref()
                .unwrap()
                .remaining()
        };
        if remaining >= adjust && remaining > 0 {
            //连接当前读缓冲区有足够的数据，则立即返回当前读取缓冲区中剩余可读字节的数量
            return Err(remaining);
        }

        //连接当前读缓冲区没有足够的数据，则只读需要的字节数
        let value = AsyncValue::new();
        let value_copy = value.clone();
        *self.ready_reader.lock() = Some(value);  //设置当前连接的异步准备读取器
        self.wait_ready_len = adjust - remaining; //设置本次异步准备读取实际需要的字节数

        Ok(value_copy)
    }

    #[inline]
    fn is_wait_wakeup_read_ready(&self) -> bool {
        self.ready_reader.lock().is_some()
    }

    fn wakeup_read_ready(&mut self) {
        if (self.wait_ready_len == 0) || (self.wait_ready_len <= self.ready_len) {
            //已完成异步准备读取指定的明文字节数
            if let Some(ready_reader) = self.ready_reader.lock().take() {
                //当前连接已设置了异步准备读取器，则立即移除并唤醒当前异步准备读取器
                ready_reader.set(self.ready_len); //设置实际读取的明文字节数
                self.wait_ready_len = 0; //重置异步准备读取的明文字节数
                self.ready_len = 0; //重置异步准备读取已就绪的明文字节数
            }
        }
    }

    fn get_read_buffer(&self) -> Rc<UnsafeCell<Option<BytesMut>>> {
        self.read_buf.clone()
    }

    #[inline]
    fn get_write_buffer(&mut self) -> Option<&mut BytesMut> {
        self.write_buf.as_mut()
    }

    fn write_ready<B>(&mut self, buf: B) -> Result<()>
        where B: AsRef<[u8]> + 'static {
        if self.is_closed() {
            //连接已关闭，则忽略，并立即返回
            return Err(Error::new(ErrorKind::ConnectionAborted,
                                  format!("Write ready failed, token: {:?}, peer: {:?}, local: {:?}, reason: connection already closed",
                                          self.get_token(),
                                          self.get_remote(),
                                          self.get_local())));
        }

        //发送指定数据
        self.wait_sent_len
            .fetch_add(buf.as_ref().len(), Ordering::Relaxed); //首先要同步增加需要发送的字节数
        if let Some(listener) = &self.write_listener {
            if let Some(token) = self.token.clone() {
                listener.send((token, buf.as_ref().to_vec()));
            }
        }

        Ok(())
    }

    fn reregister_interest(&mut self, ready: Ready) -> Result<()> {
        let interest = { *self.interest.lock() }; //保证在调用set_interest时已释放RefCell的只读引用
        match ready {
            Ready::Empty => self.set_interest(Interest::WRITABLE), //暂时中止接收消息
            Ready::Readable => self.set_interest(interest.add(Interest::READABLE)), //继续接收消息
            Ready::Writable => self.set_interest(interest.add(Interest::WRITABLE)), //继续发送消息
            Ready::OnlyRead => self.set_interest(Interest::READABLE), //继续只接收消息
            Ready::OnlyWrite => self.set_interest(Interest::WRITABLE), //继续只发送消息
            Ready::ReadWrite => self.set_interest(Interest::READABLE.add(Interest::WRITABLE)) //继续接收消息和发送消息
        }

        if let Some(interest) = self.get_interest() {
            //需要修改当前连接感兴趣的事件类型
            let token = self.get_token().unwrap().clone();
            unsafe {
                (&mut *self
                    .poll
                    .as_ref()
                    .unwrap()
                    .get())
                    .registry()
                    .reregister(self.get_stream_mut(),
                                token,
                                interest)
            }
        } else {
            Ok(())
        }
    }

    fn is_hibernated(&self) -> bool {
        self.hibernate.lock().is_some() ||
            self.hibernated_queue.lock().len() > 0
    }

    fn push_hibernated_task<F>(&self,
                               task: F)
        where F: Future<Output = ()> + 'static {
        let boxed = async move {
            task.await;
        }.boxed_local();

        self.hibernated_queue.lock().push_back(boxed);
    }

    fn run_hibernated_tasks(&self) {
        if let Some(rt) = &self.rt {
            let hibernated_queue = self.hibernated_queue.clone();

            rt.send(async move {
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

    fn hibernate(&self,
                 handle: SocketHandle<Self>,
                 ready: Ready) -> Option<Hibernate<Self>> {
        if self.is_closed() {
            //连接已关闭，则立即返回空
            return None;
        }

        let hibernate = Hibernate::new(handle, ready);
        let hibernate_copy = hibernate.clone();

        Some(hibernate_copy)
    }

    fn set_hibernate(&self, hibernate: Hibernate<Self>) -> bool {
        let mut locked = self.hibernate.lock();
        if locked.is_some() {
            //当前连接已设置了休眠对象，则返回失败
            return false;
        }

        *locked = Some(hibernate);
        true
    }

    fn set_hibernate_wakers(&self, waker: Waker) {
        self
            .hibernate_wakers
            .lock()
            .push_back(waker);
    }

    fn wakeup(&mut self, result: Result<()>) -> bool {
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

    fn close(&mut self, reason: Result<()>) -> Result<()> {
        //更新连接状态为已关闭
        if let Ok(true) = self.closed.compare_exchange(false,
                                                       true,
                                                       Ordering::AcqRel,
                                                       Ordering::Relaxed) {
            //当前已关闭，则忽略
            return Ok(());
        }

        if let Some(value) = self.ready_reader.lock().take() {
            //当前连接设置了异步准备读取器，则立即移除并唤醒当前异步准备读取器，并设置读取的长度为0
            value.set(0);
            self.wait_ready_len = 0; //重置异步准备读取的字节数
            self.ready_len = 0; //重置异步准备读取已就绪的字节数
        }

        //通知Tls连接的对端连接已关闭
        match &mut self.tls_connect {
            TlsConnect::Client(con) => {
                //通知客户端Tls连接的对端连接已关闭
                unimplemented!();
            },
            TlsConnect::Server(con) => {
                //通知服务端Tls连接的对端连接已关闭
                con.send_close_notify();
                if let Err(e) = self.do_tls_write() {
                    return Err(Error::new(ErrorKind::BrokenPipe, format!("Close tls socket failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                                         self.get_token(),
                                                                         self.get_remote(),
                                                                         self.get_local(),
                                                                         e)))
                }
            },
        }

        //通知Tls连接的监听器连接已关闭
        if let Some(listener) = &self.close_listener {
            if let Some(token) = self.get_token() {
                if let Err(e) = listener.send((token.clone(), reason)) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(())
    }
}

impl TlsSocket {
    // 根据Tls返回当前连接感兴趣的事件
    fn event_set(&self) -> Interest {
        match &self.tls_connect {
            TlsConnect::Client(con) => {
                //客户端Tls连接当前感兴趣的事件
                unimplemented!();
            },
            TlsConnect::Server(con) => {
                //服务端Tls连接当前感兴趣的事件
                let rd = con.wants_read();
                let wr = con.wants_write();

                if rd && wr {
                    Interest::READABLE | Interest::WRITABLE
                } else if wr {
                    Interest::WRITABLE
                } else {
                    Interest::READABLE
                }
            },
        }
    }

    // 接收Tcp流中的数据到Tls连接读缓冲，并解析Tls连接读缓冲的Tls数据包
    fn do_tls_read(&mut self) -> Result<usize> {
        let token = if let Some(token) = self.get_token() {
            Some(token.clone())
        } else {
            None
        };
        let remote = self.get_remote().clone();
        let local = self.get_local().clone();

        match &mut self.tls_connect {
            TlsConnect::Client(con) => {
                //客户端Tls连接接收数据
                unimplemented!();
            },
            TlsConnect::Server(con) => {
                //服务端Tls连接接收数据
                let len = match con.read_tls(&mut self.stream) {
                    Ok(0) => {
                        //Tls连接流已结束
                        return Err(Error::new(ErrorKind::ConnectionAborted,
                                              format!("Receive tls stream failed, token: {:?}, peer: {:?}, local: {:?}, reason: peer already closed",
                                                      token,
                                                      remote,
                                                      local)));
                    }
                    Ok(len) => {
                        //Tls已接收了当前Tcp连接流中所有的数据
                        len
                    },
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        //Tls本次接收未完成，因为TCP连接流已阻塞，则立即退出本次Tls接收
                        return Ok(0);
                    },
                    Err(e) => {
                        //Tls接收错误，则立即返回错误原因
                        return Err(Error::new(e.kind(),
                                              format!("Receive tls stream failed, token: {:?}, peer: {:?}, local: {:?}, reason: {:?}",
                                                      token,
                                                      remote,
                                                      local,
                                                      e)));
                    }
                };

                if let Err(e) = con.process_new_packets() {
                    //处理接收到的Tls数据包错误，则立即向对端发送错误原因，并立即返回错误原因
                    if let Err(e) = self.do_tls_write() {
                        //向对端发送错误原因失败
                        return Err(Error::new(e.kind(),
                                              format!("Receive tls stream failed, token: {:?}, peer: {:?}, local: {:?}, reason: {:?}",
                                                      token,
                                                      remote,
                                                      local,
                                                      e)));
                    }
                }

                Ok(len)
            },
        }
    }

    // 尝试处理Tls连接读缓冲的所有Tls包，并将Tls包解析为明文
    fn try_plain_read(&mut self) -> Result<usize> {
        match &mut self.tls_connect {
            TlsConnect::Client(con) => {
                //解析客户端Tls连接读缓冲的Tls包
                unimplemented!();
            },
            TlsConnect::Server(con) => {
                //解析服务端Tls连接读缓冲的Tls包
                match con.process_new_packets() {
                    Err(e) => {
                        let token = self.get_token().unwrap().clone();
                        let remote = self.get_remote().clone();
                        let local = self.get_local().clone();

                        Err(Error::new(ErrorKind::ConnectionAborted,
                                       format!("Process tls stream failed, token: {:?}, peer: {:?}, local: {:?}, reason: {:?}",
                                               token,
                                               remote,
                                               local,
                                               e)))
                    },
                    Ok(io_state) => {
                        //在do_tls_read中已经调用过process_new_packets，所以这里只处理成功的情况
                        let plain_bytes_len = io_state.plaintext_bytes_to_read(); //获取可读的明文字节大小
                        if plain_bytes_len > 0 {
                            //当前有还未读取的可读明文数据
                            let mut block = Vec::with_capacity(plain_bytes_len); //初始化本次接收块
                            block.resize(plain_bytes_len, 0u8);

                            //读取当前所有还未读取的可读明文数据
                            con
                                .reader()
                                .read_exact(&mut block)
                                .unwrap();
                            self.recv_len += plain_bytes_len; //增加连接已接收的明文字节数
                            if self.ready_reader.lock().is_some() {
                                //当前连接设置了异步准备接收器，则记录本次成功接收的明文字节数
                                self.ready_len += plain_bytes_len;
                            }

                            if let Some(buf) = unsafe { (&mut *self.read_buf.get()) } {
                                //填充到连接的读缓冲区
                                buf.put_slice(&block[..]);
                            }

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

                            //本次有可读取的未读明文数据，返回本次成功读取的未读明文字节大小
                            Ok(plain_bytes_len)
                        } else {
                            //本次没有可读取的未读明文数据
                            Ok(0)
                        }
                    },
                }
            },
        }
    }

    // 尝试将明文数据写入Tls连接的写缓冲，并在Tls握手成功后对Tls写缓冲的数据加密
    fn try_plain_write(&mut self) -> Result<usize> {
        let mut block_len = self.get_write_block_len(); //本次发送的限制

        match &mut self.tls_connect {
            TlsConnect::Client(con) => {
                //客户端Tls连接发送明文数据
                unimplemented!();
            },
            TlsConnect::Server(con) => {
                //服务端Tls连接发送明文数据
                let mut result = Ok(0); //初始化本次发送明文结果
                if let Some(write_buf) = &mut self.write_buf {
                    let remaining = write_buf.remaining(); //当前写缓冲区剩余未发送的数据大小
                    block_len = if block_len > remaining {
                        //当前写缓冲区剩余未发送的数据小于本次发送限制，则以剩余未发送的数据大小作为本次发送的发送块大小
                        remaining
                    } else {
                        //当前写缓冲区剩余未发送的数据大于等于本次发送限制，则以本次发送限制的大小作为本次发送的发送块大小
                        block_len
                    };
                    let mut bytes = write_buf.copy_to_bytes(block_len);
                    let mut block = bytes.as_ref(); //初始化本次发送块

                    //设置本次发送明文的Tls连接写缓冲的大小
                    let tls_write_buffer_limit = (block_len as f64 * DEFAULT_TLS_WRITE_BUF_SIZE_MULTIPLE).trunc() as usize;
                    con.set_buffer_limit(Some(tls_write_buffer_limit));

                    match con.writer().write_all(block) {
                        Ok(_) => {
                            //本次发送明文成功
                            self.readed_write_len += block_len; //增加已读写缓冲大小的字节数
                            self.sent_len += block_len; //增加连接已发送的字节数
                            result = Ok(block_len);
                        },
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            //本次发送已阻塞，则退出本次发送
                            if !block.is_empty() {
                                //当前发送块中还有未发送的数据，则重新写入写缓冲区
                                //注意这是因为每次发送都会消耗当前写缓冲区中的所有数据，所以可以将未发送完的数据重新写入写缓冲区
                                write_buf.put_slice(block);
                            }
                        },
                        Err(e) => {
                            //发送错误，则立即返回错误原因
                            result = Err(Error::new(e.kind(),
                                                    format!("Write to tls stream failed, token: {:?}, peer: {:?}, local: {:?}, reason: {:?}",
                                                            self.get_token(),
                                                            self.get_remote(),
                                                            self.get_local(),
                                                            e)));
                        },
                    }
                }

                //本次发送已完成
                if let Ok(len) = result {
                    if (len > 0)
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
                }

                result
            },
        }
    }

    //将Tls连接写缓冲的加密数据发送到Tcp流中
    fn do_tls_write(&mut self) -> Result<usize> {
        match &mut self.tls_connect {
            TlsConnect::Client(con) => {
                //客户端Tls连接发送数据
                unimplemented!();
            },
            TlsConnect::Server(con) => {
                //服务端Tls连接发送数据
                con.write_tls(&mut self.stream)
            },
        }
    }
}

// Tls内部连接
enum TlsConnect {
    Client(ClientConnection),   //客户端Tls连接
    Server(ServerConnection),   //服务端Tls连接
}