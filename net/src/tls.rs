use std::fs;
use std::thread;
use std::path::Path;
use std::ops::Range;
use std::boxed::FnBox;
use std::cell::RefCell;
use std::time::Duration;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::collections::VecDeque;
use std::sync::mpsc::{Sender, Receiver};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::io::{Write, Read, BufReader, Result, ErrorKind, Error};

use slab::Slab;
use vecio::Rawv;
use fnv::FnvHashMap;
use mio::tcp::{TcpListener, TcpStream, Shutdown};
use gray::GrayVersion;
use rustls::{RootCertStore, Session, NoClientAuth, AllowAnyAuthenticatedClient, AllowAnyAnonymousOrAuthenticatedClient};

use atom::Atom;
use timer::{NetTimer, NetTimers, TimerCallback};

use data::{CloseFn, Protocol, RecvFn, State, Websocket};

/*
* tls连接最大接收缓冲区
*/
pub const MAX_TLS_RECV_SIZE: usize = 16 * 1024;

/*
* tls外部请求
*/
pub type TlsExtRequest = Box<FnBox(&mut TlsHandler) + Send>;

/*
* tcp监听处理器
*/
pub type ListenerHandler = Box<Fn(Result<(TlsSocket, Arc<RwLock<TlsStream>>)>, Result<SocketAddr>) + Send>;

/*
* TLS服务器端配置
*/
pub struct TlsConfig {
    protocol: Protocol, //协议
    addr: SocketAddr,   //地址
    cert_path: Atom,    //TLS证书路径
    key_path: Atom,     //TLS密钥路径
}

impl TlsConfig {
    //构建TLS服务器配置
    pub fn new(protocol: Protocol, addr: SocketAddr, cert_file: &str, key_file: &str) -> Self {
        let mut path =  Path::new(cert_file);
        if !path.exists() {
            panic!("Tls cert file not exist, file: {:?}", path);
        }
        path = Path::new(key_file);
        if !path.exists() {
            panic!("Tls key file not exist, file: {:?}", path);
        }

        TlsConfig {
            protocol,
            addr,
            cert_path: Atom::from(cert_file),
            key_path: Atom::from(key_file),
        }
    }
}

/*
* TLS socket，主要作为外部使用的tls会话
*/
#[derive(Clone)]
pub struct TlsSocket {
    pub state: Arc<AtomicUsize>,        //状态
    pub socket: usize,                  //对应的tcp流的令牌id
    pub sender: Sender<TlsExtRequest>,  //tls事件循环请求发送者
    pub gray: Option<usize>,            //灰度
}

unsafe impl Sync for TlsSocket {}
unsafe impl Send for TlsSocket {}

impl TlsSocket {
    //构建tls socket
    pub fn new(id: usize, sender: Sender<TlsExtRequest>) -> Self {
        Self {
            state: Arc::new(AtomicUsize::new(State::Run as usize)),
            socket: id,
            sender,
            gray: None,
        }
    }
}

impl GrayVersion for TlsSocket {
    fn get_gray(&self) -> &Option<usize> {
        &self.gray
    }

    fn set_gray(&mut self, gray: Option<usize>) {
        self.gray = gray;
    }

    fn get_id(&self) -> usize {
        self.socket
    }
}

/*
* TLS流，主要作为外部使用的tls流
*/
pub struct TlsStream {
    pub handshake: bool,                                    //tls是否已握手
    pub token: mio::Token,                                  //当前流的令牌
//    pub interest: mio::Ready,                               //当前流的关注

    pub max_send_size: usize,                               //最大发送大小
    pub send_buf_offset: usize,                             //当前发送缓冲区的当前偏移
    pub send_remain_size: usize,                            //待发送数据大小
    pub send_bufs: VecDeque<Arc<Vec<u8>>>,                  //发送缓冲区列表

    pub recv_timeout: Option<Duration>,                     //接收超时时长
    pub recv_timer: Option<NetTimer<mio::Token>>,           //接收定时器

    pub recv_buf: Vec<u8>,                                  //接收缓冲区
    pub recv_size: usize,                                   //待接收数据大小，由外部指定
    pub recv_buf_offset: usize,                             //当前接收缓冲区的当前偏移，recv_buf_offset >= recv_callback_offset
    pub recv_callback_offset: usize,                        //最近一次接收回调的偏移

    pub temp_recv_buf: Option<Vec<u8>>,                     //扩展接收缓冲区，用于临时接收大于最大接收缓冲区大小的数据
    pub temp_recv_buf_offset: usize,                        //当前扩展缓冲区的当前偏移

    pub wait_wakeup_readable: Arc<RwLock<Vec<mio::Token>>>, //待唤醒可读事件的tcp流的令牌列表，与内部使用的TlsHandler共享同一个令牌列表

    pub recv_callback: Option<RecvFn>,                      //接收回调
    pub close_callback: Option<CloseFn>,                    //关闭回调

    pub timers: Arc<RwLock<NetTimers<TimerCallback>>>,      //定时器，与内部使用的TlsHandler共享同一个定时器

    //websocket
    pub websocket: Websocket,
    pub websocket_buf: Vec<u8>,
    //socket
    pub socket: Option<TlsSocket>,
}

impl TlsStream {
    //构建tls流
    pub fn new(id: usize, recv_buff_size: usize, wait_wakeup_readable: Arc<RwLock<Vec<mio::Token>>>, timers: Arc<RwLock<NetTimers<TimerCallback>>>) -> Self {
        if recv_buff_size == 0 || recv_buff_size > MAX_TLS_RECV_SIZE {
            panic!("Invalid recv buff size, size: {}", recv_buff_size);
        }

        let mut recv_buf = Vec::with_capacity(recv_buff_size);
        unsafe {
            recv_buf.set_len(recv_buff_size);
        }

        Self {
            handshake: false,
            token: mio::Token(id),
//            interest: mio::Ready::empty(),

            max_send_size: 0,
            send_buf_offset: 0,
            send_remain_size: 0,
            send_bufs: VecDeque::new(),

            recv_timeout: None,
            recv_buf_offset: 0,
            recv_callback_offset: 0,

            recv_buf: recv_buf,
            recv_size: 0,
            temp_recv_buf_offset: 0,
            wait_wakeup_readable,
            recv_timer: None,
            temp_recv_buf: None,

            recv_callback: None,
            close_callback: None,

            timers,
            websocket: Websocket::None,
            websocket_buf: vec![],
            socket: None,
        }
    }

    //获取tls socket的定时器
    pub fn get_timers(&self) -> Arc<RwLock<NetTimers<TimerCallback>>> {
        self.timers.clone()
    }

    //设置tls socket
    pub fn set_socket(&mut self, socket: TlsSocket) {
        self.socket = Some(socket);
    }

    //设置流关闭回调
    pub fn set_close_callback(&mut self, func: CloseFn) {
        self.close_callback = Some(func);
    }

    //设置发送缓冲区大小，单位字节
    pub fn set_send_buf_size(&mut self, size: usize) {
        self.max_send_size = size;
    }

    //设置接收超时时长，单位毫秒
    pub fn set_recv_timeout(&mut self, time: usize) {
        self.recv_timeout = Some(Duration::from_millis(time as u64));
    }

    //唤醒当前tcp流的可读事件，线程安全的关注指定tcp流的读事件
    pub fn wakeup_readable(&mut self, size: usize, func: RecvFn) -> Option<(RecvFn, Result<Arc<Vec<u8>>>)> {
        if !self.recv_callback.is_none() {
            //不允许重复设置接收回调
            return Some((func, Err(Error::new(
                ErrorKind::Other,
                "Recive callback can't set twice",
            ))));
        }

        self.temp_recv_buf = None;
        self.temp_recv_buf_offset = 0;

        let cb_offset = self.recv_callback_offset;
        let buf_offset = self.recv_buf_offset;
        if size == 0 && buf_offset > cb_offset {
            //从已接收缓冲区中读所有未读数据，并立即同步返回
            let param = self.recv_buf[cb_offset..buf_offset]
                .iter()
                .cloned()
                .collect();
            let size2 = buf_offset - cb_offset;
            self.recv_callback_offset += size2;
            return Some((func, Ok(Arc::new(param))));
        } else if size <= buf_offset - cb_offset {
            //从已接收缓冲区中读指定大小的未读数据，并立即同步返回
            let param = self.recv_buf[cb_offset..cb_offset + size]
                .iter()
                .cloned()
                .collect();
            self.recv_callback_offset += size;
            return Some((func, Ok(Arc::new(param))));
        } else if size > self.recv_buf.capacity() {
            //当前需要读取的大小大于接收缓冲区的容量，则初始化扩展缓冲区，准备异步接收数据
            let mut buf = Vec::with_capacity(size);
            unsafe {
                buf.set_len(size);
            }

            let len = buf_offset - cb_offset;
            buf[0..len].copy_from_slice(&self.recv_buf[cb_offset..buf_offset]);
            self.recv_buf_offset = 0;
            self.recv_callback_offset = 0;
            self.temp_recv_buf = Some(buf);
        } else if self.recv_callback_offset + size > self.recv_buf.capacity() {
            //已读大小与当前需要读取的大小之和大于接收缓冲区的容量，则设置当前当前接收缓冲偏移，准备异步接收剩余的数据
            let len = self.recv_buf_offset - self.recv_callback_offset;
            move_vec(&mut self.recv_buf, self.recv_callback_offset, 0, len);
            self.recv_callback_offset = 0;
            self.recv_buf_offset = len;
        }
        let timeout = self.recv_timeout.unwrap();
        let timer = NetTimer::new();
        timer.set_timeout(timeout, self.token);
        self.recv_size = size;
        self.recv_callback = Some(func);
        self.recv_timer = Some(timer);
        self.wait_wakeup_readable.write().unwrap().push(self.token);

        return None;
    }
}

/*
* tls事件源
*/
pub enum TlsOrigin {
    TcpServer(ListenerHandler, TcpListener),                    //tls监听连接事件源
    TcpStream(Arc<RwLock<TlsStream>>, TcpStream, mio::Token),   //tls监听流读写事件源
}

/*
* TLS连接，用于绑定tcp流和tls会话
*/
struct TlsConnection {
    token: mio::Token,                  //tls连接对应的tcp流的令牌
    closing: bool,                      //是否正在关闭tls连接
    closed: bool,                       //是否已关闭tls连接
    tls_session: rustls::ServerSession, //tls会话
}

impl TlsConnection {
    //构建tls连接
    fn new(token: mio::Token, tls_session: rustls::ServerSession) -> Self {
        TlsConnection {
            token,
            closing: false,
            closed: false,
            tls_session,
        }
    }

    //判断tls会话是否握手完成，只用于tls连接外部的握手判断
    fn is_handshake(&self) -> bool {
        !self.closing && !self.closed && !self.tls_session.is_handshaking()
    }

    //判断tls会话是否关闭
    fn is_closed(&self) -> bool {
        self.closed
    }

    //根据tls会话获取关注的事件类型
    fn event_set(&self) -> mio::Ready {
        let rd = self.tls_session.wants_read();
        let wr = self.tls_session.wants_write();

        if rd && wr {
            mio::Ready::readable() | mio::Ready::writable()
        } else if wr {
            mio::Ready::writable()
        } else {
            mio::Ready::readable()
        }
    }

    //根据tls连接需要，注册tcp流
    fn register(&self, socket: &TcpStream, poll: &mio::Poll) {
        poll.register(socket,
                      self.token,
                      self.event_set(),
                      mio::PollOpt::level() | mio::PollOpt::oneshot())
            .unwrap();
    }

    //根据tls会话需要，重新注册tcp流
    fn reregister(&self, socket: &TcpStream, poll: &mio::Poll) {
        poll.reregister(socket,
                        self.token,
                        self.event_set(),
                        mio::PollOpt::level() | mio::PollOpt::oneshot())
            .unwrap();
    }

    //注销读关注
    fn unregister_readable(&self, socket: &TcpStream, poll: &mio::Poll) {
        let mut interest = self.event_set();
        interest.remove(mio::Ready::readable()); //强制注销读关注
        poll.reregister(socket,
                        self.token,
                        interest,
                        mio::PollOpt::level() | mio::PollOpt::oneshot())
            .unwrap();
    }

    //注销写关注，根据tls会话缓冲区是否为空来确定是否注销写关注
    fn unregister_writable(&self, socket: &TcpStream, poll: &mio::Poll) {
        self.reregister(socket, poll);
    }

    //根据外部需要，重新注册tcp流的写关注
    fn reregister_writable(&self, socket: &TcpStream, poll: &mio::Poll) {
        let mut interest = self.event_set();
        if interest.is_writable() {
            //已经注册写关注，则忽略
            return;
        }

        interest.insert(mio::Ready::writable()); //强制注册写关注
        poll.reregister(socket,
                        self.token,
                        interest,
                        mio::PollOpt::level() | mio::PollOpt::oneshot())
            .unwrap();
    }

    //tls握手
    fn handshake(&mut self,
                 poll: &mut mio::Poll,
                 socket: &mut TcpStream) {
        if let Err(e) = self.tls_session.complete_io::<TcpStream>(socket) {
            if let ErrorKind::WouldBlock = e.kind() {
                //当前阻塞，则忽略本次握手，则重新注册当前tcp流，等待下次握手
                return;
            }

            println!("!!!> Handshake failed, e: {:?}", e);
            let _ = socket.shutdown(Shutdown::Both);
            self.closed = true; //设置tls连接状态为已关闭
        }

        if self.closing && !self.tls_session.wants_write() {
            //如果tls连接正在关闭或当前tls会话不可写，则立即关闭当前tcp流
            let _ = socket.shutdown(Shutdown::Both);
            self.closed = true; //设置tls连接状态为已关闭
        } else {
            //如果tls连接当前未关闭或当前tls会话可写
            self.reregister(socket, poll);
        }
    }

    //处理读事件
    fn read(&mut self,
            poll: &mut mio::Poll,
            socket: &mut TcpStream,
            buf: &mut [u8]) -> Result<usize> {
        self.decode_tls(socket);
        let r = self.try_read(buf);


        if self.closing && !self.tls_session.wants_write() {
            //如果tls连接正在关闭或当前tls会话不可写，则立即关闭当前tcp流
            let _ = socket.shutdown(Shutdown::Both);
            self.closed = true; //设置tls连接状态为已关闭
        } else {
            //如果tls连接当前未关闭或当前tls会话可写
            self.reregister(socket, poll);
        }

        r
    }

    //处理写事件
    fn write(&mut self,
            poll: &mut mio::Poll,
            socket: &mut TcpStream) {
        self.encode_tls(socket);

        if self.closing && !self.tls_session.wants_write() {
            //如果tls连接正在关闭并且当前tls会话不可写，则立即关闭当前tcp流
            let _ = socket.shutdown(Shutdown::Both);
            self.closed = true; //设置tls连接状态为已关闭
        } else {
            //如果tls连接当前未关闭或当前tls会话可写，则重新注册当前tcp流
            self.reregister(socket, poll);
        }
    }

    //从tcp流中读取密文数据并解码，然后写入tls会话读缓冲区
    fn decode_tls(&mut self, socket: &mut TcpStream) {
        let rc = self.tls_session.read_tls(socket);
        if rc.is_err() {
            let err = rc.unwrap_err();

            if let ErrorKind::WouldBlock = err.kind() {
                return;
            }

            println!("!!!> Read tls stream error {:?}", err);
            self.closing = true;
            return;
        }

        if rc.unwrap() == 0 {
            self.closing = true;
            return;
        }

        //处理tls数据
        let processed = self.tls_session.process_new_packets();
        if processed.is_err() {
            println!("!!!> Cannot process packet: {:?}", processed);
            self.closing = true;
            return;
        }
    }

    //尝试从tls会话读缓冲区中读取明文数据
    fn  try_read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let r = self.tls_session.read(buf);
        if let Err(ref e) = r {
            if e.kind() == ErrorKind::Interrupted {
                //非错误异常，则尝试继续读
                return self.try_read(buf);
            } else if e.kind() == ErrorKind::WouldBlock {
                //尝试读时阻塞，则忽略
                ()
            } else {
                //读取错误，则准备关闭tls会话
                println!("!!!> Try tls read failed: {:?}", r);
                self.closing = true;
            }
        }
        r
    }

    //尝试向tls会话写缓冲区写入明文数据
    fn try_write(&mut self, buf: &[u8]) -> Result<()> {
        let r = self.tls_session.write_all(buf);
        if let Err(ref e) = r {
            if e.kind() == ErrorKind::Interrupted {
                //非错误异常，则尝试继续写
                return self.try_write(buf);

            } else if e.kind() == ErrorKind::WouldBlock {
                //尝试写时阻塞，则忽略
                ()
            } else {
                //写错误，则准备关闭tls会话
                println!("!!!> Try tls write failed, e: {:?}", e);
                self.closing = true;
            }
        }
        r
    }

    //编码tls会话写缓冲区中的明文数据，并写入tcp流
    fn encode_tls(&mut self, socket: &mut TcpStream) {
        #[cfg(any(unix))]
        let rc = self.tls_session.writev_tls(&mut WriteVAdapter::new(socket));

        let rc = self.tls_session.write_tls(socket);
        if rc.is_err() {
            println!("!!!> Write tls stream error {:?}", rc);
            self.closing = true;
        }
    }

    //尝试刷新tls会话写缓冲区，检查tls会话当前缓冲区是否为空，则刷新
    fn try_flush(&mut self) -> Result<()> {
        let r = self.tls_session.flush();
        if r.is_err() {
            //刷新错误，则准备关闭tls会话
            println!("!!!> Try tls flush failed: {:?}", r);
            self.closing = true;
        }
        r
    }

    //关闭tls会话
    fn close(&mut self) {
        self.closing = true;
        self.tls_session.send_close_notify(); //将会话关闭消息写入tls会话写缓冲区
    }
}

/*
* TLS服务器，用于绑定tcp连接监听器
*/
struct TlsServer {
    cfg: Arc<rustls::ServerConfig>,                     //TLS配置
    connections: FnvHashMap<mio::Token, TlsConnection>, //安全连接表
}

impl TlsServer {
    //构建一个TLS服务器
    fn new(cfg: Arc<rustls::ServerConfig>) -> Self {
        TlsServer {
            cfg,
            connections: FnvHashMap::default(),
        }
    }

    //判断指定tls连接是否握手
    fn is_handshake(&self, token: mio::Token) -> bool {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            return self.connections
                .get(&token)
                .unwrap()
                .is_handshake();
        }
        false
    }

    //注销指定tls流的读事件
    fn unregister_readable(&self,
                           poll: &mio::Poll,
                           socket: &TcpStream,
                           token: mio::Token) {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            self.connections
                .get(&token)
                .unwrap()
                .unregister_readable(socket, poll);
        }
    }

    //重新注册指定tls流的读事件
    fn reregister_readable(&self,
                           poll: &mio::Poll,
                           socket: &TcpStream,
                           token: mio::Token) {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            self.connections
                .get(&token)
                .unwrap()
                .reregister(socket, poll);
        }
    }

    //注销指定tls流的写事件
    fn unregister_writable(&self,
                           poll: &mio::Poll,
                           socket: &TcpStream,
                           token: mio::Token) {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            self.connections
                .get(&token)
                .unwrap()
                .unregister_writable(socket, poll);
        }
    }

    //重新注册指定tls流的写事件
    fn reregister_writable(&self,
                           poll: &mio::Poll,
                           socket: &TcpStream,
                           token: mio::Token) {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            self.connections
                .get(&token)
                .unwrap()
                .reregister_writable(socket, poll);
        }
    }

    //接受tls连接请求
    fn accept(&mut self, socket: &TcpStream, poll: &mio::Poll, token: mio::Token) {
        //构建tls连接，用于通过令牌绑定tcp流与tls会话
        let tls_session = rustls::ServerSession::new(&self.cfg); //构建tls会话
        self.connections.insert(token, TlsConnection::new(token, tls_session)); //绑定tls会话
        self.connections[&token].register(socket, poll); //注册tcp流
    }

    //处理tls握手
    fn handle_handshake(&mut self, poll: &mut mio::Poll, socket: &mut TcpStream, token: mio::Token) {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            self.connections
                .get_mut(&token)
                .unwrap()
                .handshake(poll, socket);

            if self.connections[&token].is_closed() {
                self.connections.remove(&token);
            }
        }
    }

    //处理tls流读事件
    fn handle_read_stream(&mut self,
                          poll: &mut mio::Poll,
                          socket: &mut TcpStream,
                          token: mio::Token,
                          buf: &mut [u8]) -> Result<usize> {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            let r = self.connections
                            .get_mut(&token)
                            .unwrap()
                            .read(poll, socket, buf);

            if self.connections[&token].is_closed() {
                self.connections.remove(&token);
            }

            return r;
        }
        Err(Error::new(ErrorKind::NotConnected, "handle read stream failed, invalid tls stream"))
    }

    //写tls流
    fn write_stream(&mut self,
                    token: mio::Token,
                    buf: &[u8]) -> Result<()> {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            let r = self.connections
                .get_mut(&token)
                .unwrap()
                .try_write(buf);

            if self.connections[&token].is_closed() {
                self.connections.remove(&token);
            }

            return r;
        }
        Err(Error::new(ErrorKind::NotConnected, "write stream failed, invalid tls stream"))
    }

    //处理tls流写事件
    fn handle_write_stream(&mut self,
                           poll: &mut mio::Poll,
                           socket: &mut TcpStream,
                           token: mio::Token) {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            self.connections
                .get_mut(&token)
                .unwrap()
                .write(poll, socket);

            if self.connections[&token].is_closed() {
                self.connections.remove(&token);
            }
        }
    }

    //刷新tls流
    fn flush(&mut self, token: mio::Token) -> Result<()> {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            let r = self.connections
                .get_mut(&token)
                .unwrap()
                .try_flush();

            if self.connections[&token].is_closed() {
                self.connections.remove(&token);
            }

            return r;
        }
        Err(Error::new(ErrorKind::NotConnected, "flush stream failed, invalid tls stream"))
    }

    //关闭tls流
    fn close(&mut self, token: mio::Token) {
        if self.connections.contains_key(&token) {
            //令牌对应的tls流存在
            self.connections
                .get_mut(&token)
                .unwrap()
                .close();

            if self.connections[&token].is_closed() {
                self.connections.remove(&token);
            }
        }
    }
}

/*
* TLS处理器
*/
pub struct TlsHandler {
    poll: mio::Poll,                                    //轮询器
    slab: Arc<RefCell<Slab<TlsOrigin>>>,                //tls源相关数据的内存空间管理器
    sender: Sender<TlsExtRequest>,                      //tls事件循环请求发送者
    timers: Arc<RwLock<NetTimers<TimerCallback>>>,      //定时器，与外部使用的TlsStream共享同一个定时器
    wait_wakeup_readable: Arc<RwLock<Vec<mio::Token>>>, //待唤醒可读事件的tcp流的令牌列表，与外部使用的TlsStream共享同一个令牌列表
    wait_wakeup_writable: Arc<RwLock<Vec<mio::Token>>>, //待唤醒可写事件的tcp流的令牌列表
    tls_server: Option<Arc<RefCell<TlsServer>>>,        //tls连接服务器，管理所有tls连接
}

/*
* 处理外部请求绑定tls端口
*/
pub fn handle_bind(handler: &mut TlsHandler, config: TlsConfig, func: ListenerHandler) {
    match config.protocol {
        Protocol::TCP => bind_tcp(handler, config, func),
    }
}

//绑定tls端口
fn bind_tcp(handler: &mut TlsHandler, config: TlsConfig, func: ListenerHandler) {
    match TcpListener::bind(&config.addr) {
        Err(e) => panic!("bind tcp {:?} failed, e: {:?}", &config.addr, e),
        Ok(listener) => {
            //绑定指定地址和端口成功
            let mut slab = handler.slab.borrow_mut();
            let entry = slab.vacant_entry(); //创建tcp连接监听器和tcp连接流所需的空间管理器
            let key = entry.key(); //为tcp连接监听器分配空间，并得到空间偏移

            println!("===> Bind tcp listener addr: {:?}, counte: {:?}: interest: {:?}", config.addr, key, mio::Ready::readable());

            //注册需要轮询的tcp连接监听器，分配唯一令牌，并设置关注的事件类型和轮询方式
            handler
                .poll
                .register(&listener, mio::Token(key), mio::Ready::readable(), mio::PollOpt::level())
                .unwrap();

            //根据证书和私钥，初始化tls服务器
            let tls_cfg = make_config(&config.cert_path, &config.key_path);
            handler.tls_server = Some(Arc::new(RefCell::new(TlsServer::new(tls_cfg))));

            //将tcp连接监听器与监听回调函数打包，并写入分配的空间中
            entry.insert(TlsOrigin::TcpServer(func, listener));
        },
    }
}

//构建TLS配置，需要证书文件和私钥文件
fn make_config(cert_file: &str, key_file: &str) -> Arc<rustls::ServerConfig> {
    let client_auth = NoClientAuth::new();
    let mut config = rustls::ServerConfig::new(client_auth);
    config.key_log = Arc::new(rustls::KeyLogFile::new());

    let certs = load_certs(cert_file);
    let privkey = load_private_key(key_file);
    config.set_single_cert_with_ocsp_and_sct(certs, privkey, vec![], vec![])
        .expect("bad certificates/private key");
    config.set_protocols(&vec![]);

    Arc::new(config)
}

/*
* 启动TLS线程的事件处理循环
*/
pub fn startup(sender: Sender<TlsExtRequest>, receiver: Receiver<TlsExtRequest>, recv_buff_size: usize) {
    //构建处理器
    let mut handler = TlsHandler {
        sender: sender,
        slab: Arc::new(RefCell::new(Slab::<TlsOrigin>::new())),
        poll: mio::Poll::new().unwrap(),
        timers: Arc::new(RwLock::new(NetTimers::new())),
        wait_wakeup_readable: Arc::new(RwLock::new(Vec::<mio::Token>::new())),
        wait_wakeup_writable: Arc::new(RwLock::new(Vec::<mio::Token>::new())),
        tls_server: None, //等待绑定时初始化tls服务器
    };

    let mut events = mio::Events::with_capacity(1024); //初始化tls事件池

    //事件处理循环
    loop {
        //事件处理间隔
        thread::sleep(Duration::from_millis(10));

        //唤醒tcp流的可读事件
        handle_wakeup_readable(&mut handler);

        //唤醒tcp流的可写事件
        handle_wakeup_writable(&mut handler);

        //处理所有已注册的令牌产生的tcp连接和tcp流事件
        handle_event(&mut handler, &mut events, recv_buff_size);

        //处理外部发送到线程的所有请求
        while let Ok(req) = receiver.try_recv() {
            req.call_box((&mut handler,));
        }

        //处理所有已注册的令牌产生的tcp流超时事件
        handle_timeout(&handler);
    }
}

//处理所有待唤醒tcp流的可读事件
fn handle_wakeup_readable(handler: &mut TlsHandler) {
    //唤醒所有待唤醒的可读事件
    for &mio::Token(id) in handler.wait_wakeup_readable.read().unwrap().iter() {
        let mut slab = handler.slab.borrow_mut();
        let origin = slab.get_mut(id).unwrap();
        match origin {
            &mut TlsOrigin::TcpStream(ref mut stream, ref mut tcp_stream, _) => {
                //指定令牌的tcp流存在，则同步更新外部流的关注，并重新注册tls流的可读事件
                let token = mio::Token(id);
                let mut s = &mut stream.write().unwrap();
                handler.tls_server.as_ref().unwrap().borrow().reregister_readable(&handler.poll, tcp_stream, token);
            }
            _ => panic!("handle wakeup readable failed, id: {}", id),
        }
    }
    handler.wait_wakeup_readable.write().unwrap().clear(); //同步清理所有待唤醒可读事件的令牌
}

//处理所有待唤醒tcp流的可写事件
fn handle_wakeup_writable(handler: &mut TlsHandler) {
    //唤醒所有待唤醒的可写事件
    for &mio::Token(id) in handler.wait_wakeup_writable.read().unwrap().iter() {
        let mut slab = handler.slab.borrow();
        let origin = slab.get(id).unwrap();
        match origin {
            &TlsOrigin::TcpStream(ref stream, ref tcp_stream, _) => {
                //指定令牌的tcp流存在，则同步更新外部流的关注，并重新注册tls流的可写事件
                let token = mio::Token(id);
                handler.tls_server.as_ref().unwrap().borrow().reregister_writable(&handler.poll, tcp_stream, token);

                //将重新注册tls流可写事件的tcp流的待发送缓冲区中的首个数据，写入已握手的tls流，已保证唤醒tls流，剩余数据由唤醒的tls流完成写入
                let buf = stream.write().unwrap().send_bufs.pop_front();
                if let Some(bin) = buf {
                    if fill_write_buffer(stream.clone(), handler.tls_server.as_ref().unwrap().clone(), token, bin) {
                        //写入tls流成功，则继续
                        continue;
                    } else {
                        //写入tls流错误，则立即关闭当前tcp流
                        handle_close(handler, id, true);
                    }
                }
            }
            _ => panic!("handle wakeup writable failed, id: {}", id),
        }
    }
    handler.wait_wakeup_writable.write().unwrap().clear(); //同步清理所有待唤醒可写事件的令牌
}

//处理tcp连接和tcp流写事件
fn handle_event(handler: &mut TlsHandler, events: &mut mio::Events, recv_buff_size: usize) {
    if handler.tls_server.is_none() {
        //tls服务器还未初始化，则忽略
        return;
    }

    handler.poll.poll(events, Some(Duration::from_millis(0))).unwrap();
    for event in events.iter() {
        let token = event.token();  //当前事件令牌
        let readiness = event.readiness();
        let mut accepted = None; //已接受连接
        let mut handshaked = false; //连接已握手
        let mut close_id: Option<usize> = None; //准备关闭的tcp流标记

        if let Some(origin) = handler.slab.borrow_mut().get_mut(token.0) {
            match origin {
                &mut TlsOrigin::TcpServer(_, ref listener) => {
                    //处理请求连接事件
                    if readiness.is_readable() {
                        match listener.accept() {
                            Ok((tcp_stream, addr)) => {
                                //接受连接请求，返回tcp流
                                println!("===> Accepting new connection from {:?}", addr);
                                accepted = Some(tcp_stream);
                            },
                            Err(e) => {
                                println!("!!!> Encountered error while accepting connection; e: {:?}", e);
                            },
                        }
                    } else if readiness.is_writable() {
                        //TODO error callback...
                        panic!("invalid connect, readiness: writable");
                    }
                },
                &mut TlsOrigin::TcpStream(ref stream, ref mut tcp_stream, _) => {
                    //处理读写事件
                    if !stream.read().unwrap().handshake {
                        //检查最新的tls握手状态
                        if handler.tls_server.as_ref().unwrap().borrow().is_handshake(token.clone()) {
                            //连接已握手
                            handshaked = true;
                        }
                    }

                    if readiness.is_readable() {
                        //处理可读事件
                        let result = handle_read_stream_event(&mut handler.poll, tcp_stream, stream.clone(), handler.tls_server.as_ref().unwrap().clone(), &event);
                        match result {
                            None => (), //忽略握手过程中的非空读
                            Some(ReadResult::Empty) => {
                                //空读，则无论是否握手中，都暂时让出当前tcp流的可读事件
                                yield_readable(handler, stream.clone(), tcp_stream, token.clone());
                            },
                            Some(ReadResult::Finish(r)) => {
                                match r {
                                    Err(_) => {
                                        //握手过程中或握手后，读错误，则准备关闭当前tcp流
                                        let mio::Token(id) = event.token();
                                        close_id = Some(id);
                                    }
                                    Ok((recv_callback, range)) => {
                                        //握手后，已读取指定长度的数据，则暂时让出当前tcp流的可读事件，执行接收回调，等待外部再次唤醒当前tcp流的可读事件
                                        yield_readable(handler, stream.clone(), tcp_stream, token.clone());
                                        callback_recv(stream.clone(), recv_callback, range);
                                    },
                                }
                            },
                        }
                    } else if readiness.is_writable() {
                        //处理可写事件
                        let result = handle_write_stream_event(&mut handler.poll, tcp_stream, stream.clone(), handler.tls_server.as_ref().unwrap().clone(), &event);
                        if let Some(b) = result {
                            if b {
                                //握手后，已写入待发送缓冲区的数据，则暂时让出当前tcp流的可写事件，等待外部请求发送数据时再次唤醒当前tcp流的可写事件
                                yield_writable(&handler, stream.clone(), tcp_stream, token.clone());
                            } else {
                                //握手过程中或握手后，写错误，则准备关闭当前tcp流
                                let mio::Token(id) = event.token();
                                close_id = Some(id);
                            }
                        }
                    }
                },
            }
        }

        if let Some(tcp_stream) = accepted {
            //接受连接请求，且建立tcp流成功，则绑定tcp流和tls会话，并开始tls握手
            handle_connect_event(handler, tcp_stream, token.clone(), recv_buff_size);
        }

        if handshaked {
            //已握手，则tls连接完成
            handle_tls_handshake(handler, token.clone());
        }

        if let Some(id) = close_id {
            //读写事件错误，则关闭tls连接
            handle_close(handler, id, true);
        }
    }
}

//处理tls连接事件
fn handle_connect_event(handler: &TlsHandler, tcp_stream: TcpStream, acceptor_token: mio::Token, recv_buff_size: usize) -> Option<usize> {
    //为tcp流分配空间
    let mut slab = handler.slab.borrow_mut();
    let entry = slab.vacant_entry();
    let key = entry.key();
    let token = mio::Token(key);

    //为tcp流设置接收缓冲区
    tcp_stream.set_recv_buffer_size(recv_buff_size).unwrap();

    //注册需要轮询的tcp流，分配唯一令牌，并设置关注的事件类型和轮询方式
    let mut stream = TlsStream::new(0, recv_buff_size, handler.wait_wakeup_readable.clone(), handler.timers.clone()); //创建外部使用的tls流，设置tls流的默认接收缓冲区，与tls处理器共享相同的待唤醒令牌列表和定时器
    stream.token = token.clone();
    handler.tls_server.as_ref().unwrap().borrow_mut().accept(&tcp_stream, &handler.poll, stream.token.clone()); //绑定tcp流和tls会话

    //将tcp流、外部tls流和对应的连接接受器令牌打包，并写入分配的空间中
    entry.insert(TlsOrigin::TcpStream(Arc::new(RwLock::new(stream)), tcp_stream, acceptor_token));
    Some(key)
}

//处理tls连接握手
fn handle_tls_handshake(handler: &TlsHandler, token: mio::Token) {
    let slab = handler.slab.borrow();
    if let Some(origin) = slab.get(token.0) {
        match origin {
            &TlsOrigin::TcpStream(ref stream, ref tcp_stream, ref acceptor_token) => {
                //更新tcp流的握手状态
                stream.write().unwrap().handshake = true;

                //暂时让出当前tcp流的可读事件，等待外部再次唤醒当前tcp流的可读事件
                yield_readable(handler, stream.clone(), tcp_stream, token.clone());

                //构建外部使用的socket，与内部的tls处理共享同一个tls事件循环请求发送者
                let socket = TlsSocket::new(token.0, handler.sender.clone());

                if let Some(origin) = slab.get(acceptor_token.0) {
                    //获取接受器对应的连接回调
                    match origin {
                        &TlsOrigin::TcpServer(ref connect_callback, _) => {
                            //调用连接回调
                            connect_callback(Ok((socket, stream.clone())), tcp_stream.peer_addr());
                        },
                        _ => (), //建立tls连接异常，tls源不相同
                    }
                }
            },
            _ => (), //建立tls连接异常，tls源不相同
        }
    }
}

//读事件返回
enum ReadResult {
    Empty,                                  //空读
    Finish(Result<(RecvFn, Range<usize>)>), //读完成
}

//处理tls流的可读事件
fn handle_read_stream_event(poll: &mut mio::Poll,
                            tcp_stream: &mut TcpStream,
                            stream: Arc<RwLock<TlsStream>>,
                            tls_server: Arc<RefCell<TlsServer>>,
                            event: &mio::Event) -> Option<ReadResult> {
    if stream.read().unwrap().handshake && stream.read().unwrap().recv_callback.is_none() {
        //tls会话已握手，且接收回调已空，则默认为空读
        return Some(ReadResult::Empty);
    }

    let mut r = None;
    let token = event.token(); //获得事件源对应的令牌
    if stream.read().unwrap().temp_recv_buf.is_none() {
        //没有临时缓冲区，表示当前tcp流中的数据大小在缓冲区大小范围内
        let result = fill_read_buffer(poll,
                                           tcp_stream,
                                           &mut stream.write().unwrap(),
                                           &mut tls_server.borrow_mut(),
                                           token);

        match result {
            Err(e) => {
                //从tcp流中读取数据失败
                r = Some(ReadResult::Finish(Err(e)));
            },
            Ok(size) if stream.read().unwrap().handshake => {
                //从tcp流中读取数据成功，且tls会话已握手
                let s = &mut stream.write().unwrap();
                if s.recv_size == 0 && s.recv_buf_offset > s.recv_callback_offset {
                    //读取任意大小的数据，且已填充缓冲区，则返回接收回调函数和本次填充的缓冲区范围
                    let start = s.recv_callback_offset;
                    let end = s.recv_buf_offset;
                    let callback = s.recv_callback.take().unwrap();
                    if size == 0 {
                        r = Some(ReadResult::Empty);
                    } else {
                        r = Some(ReadResult::Finish(Ok((callback, start..end))));
                    }

                    let size2 = end - start;
                    s.recv_callback_offset += size2;
                } else {
                    //读取指定大小的数据，允许读取大于指定大小的数据
                    if size == s.recv_size && s.recv_size != 0 {
                        //已填充指定大小的缓冲区，则返回接收回调函数和本次填充的缓冲区范围
                        let start = s.recv_callback_offset;
                        let end = start + s.recv_size;
                        let callback = s.recv_callback.take().unwrap();
                        if size == 0 {
                            r = Some(ReadResult::Empty);
                        } else {
                            r = Some(ReadResult::Finish(Ok((callback, start..end))));
                        }

                        s.recv_callback_offset += s.recv_size;
                    }
                }
            },
            Ok(0) => {
                //从tcp流中空读，且tls会话未握手
                r = Some(ReadResult::Empty);
            }
            _ => (), //忽略握手过程中的正常读取
        }
    } else {
        //有临时缓冲区，表示当前tcp流中的数据大小大于缓冲区大小范围
        let result = fill_temp_read_buffer(poll,
                                                tcp_stream,
                                                &mut stream.write().unwrap(),
                                                &mut tls_server.borrow_mut(),
                                                token);

        match result {
            Err(e) => {
                //从tcp流中读取数据失败
                r = Some(ReadResult::Finish(Err(e)));
            },
            Ok(lasted_size) if stream.read().unwrap().handshake => {
                //从tcp流中读取数据成功, 且tls会话已握手
                let buf_len = stream.read().unwrap().temp_recv_buf.as_ref().unwrap().len(); //扩展缓冲区大小
                let s = &mut stream.write().unwrap();
                if s.temp_recv_buf_offset == buf_len {
                    //已填充扩展缓冲区，则返回接收回调函数和本次填充的缓冲区范围
                    let callback = s.recv_callback.take().unwrap();
                    if lasted_size == 0 {
                        r = Some(ReadResult::Empty);
                    } else {
                        r = Some(ReadResult::Finish(Ok((callback, 0..buf_len))));
                    }
                }
            },
            Ok(0) => {
                //从tcp流中空读，且tls会话未握手
                r = Some(ReadResult::Empty);
            }
            _ => (), //忽略握手过程中的正常读取
        }
    }

    r
}

//填充读缓冲区，用于读取在缓冲区大小范围内的数据
fn fill_read_buffer(poll: &mut mio::Poll,
                    tcp_stream: &mut TcpStream,
                    stream: &mut TlsStream,
                    tls_server: &mut TlsServer,
                    token: mio::Token) -> Result<usize> {
    loop {
        if stream.recv_size == 0 {
            //读取tcp流中不超过当前缓冲区剩余大小的数据
            let begin = stream.recv_buf_offset; //缓冲区已填充位置
            match tls_server.handle_read_stream(poll, tcp_stream, token, &mut stream.recv_buf[begin..]) {
                Ok(0) => {
                    //tcp流上没有数据，则结束本次读取
                    break Ok(0);
                },
                Ok(size) => {
                    //tcp流上有数据，则移动已读取偏移，并结束本次读取
                    stream.recv_buf_offset += size;
                    break Ok(size);
                }
                Err(err) => {
                    if let ErrorKind::WouldBlock = err.kind() {
                        //tcp读取可能阻塞，则结束本次读取
                        println!("!!!> Recvive wouldblock, token: {:?}, offset: {}", stream.token, stream.recv_buf_offset);
                        break Ok(0);
                    }
                    break Err(err);
                }
            }
        } else {
            //读取tcp流中不超过当前缓冲区剩余大小，且外部指定大小的数据
            let begin = stream.recv_buf_offset; //缓冲区已填充位置
            let end = stream.recv_callback_offset + stream.recv_size; //需要填充的缓冲区长度
            match tls_server.handle_read_stream(poll, tcp_stream, token,&mut stream.recv_buf[begin..]) {
                Ok(0) => {
                    //tcp流上没有数据，则结束本次读取
                    break Ok(0);
                },
                Ok(size) => {
                    //tcp流上有数据，则移动已读取偏移
                    stream.recv_buf_offset += size;
                    if stream.recv_buf_offset >= end {
                        //已填充外部指定大小的缓冲区，则结束本次读取
                        break Ok(stream.recv_size);
                    }
                }
                Err(err) => {
                    if let ErrorKind::WouldBlock = err.kind() {
                        //tcp读取可能阻塞，则结束本次读取
                        println!("!!!> Recvive wouldblock, token: {:?}, offset: {}", stream.token, stream.recv_buf_offset);
                        break Ok(0);
                    }
                    break Err(err);
                }
            }
        }

    }
}

//填充扩展缓冲区，用于读取大于缓冲区范围的数据
fn fill_temp_read_buffer(poll: &mut mio::Poll,
                         tcp_stream: &mut TcpStream,
                         stream: &mut TlsStream,
                         tls_server: &mut TlsServer,
                         token: mio::Token) -> Result<usize> {
    loop {
        let mut offset = &mut stream.temp_recv_buf_offset; //扩展缓冲区已填充位置
        let mut buf = stream.temp_recv_buf.as_mut().unwrap();

        if *offset == buf.len() {
            //扩展缓冲区溢出
            panic!("temp recv buffer overflow");
        }

        match tls_server.handle_read_stream(poll, tcp_stream, token, &mut stream.recv_buf[*offset..]) {
            Ok(0) => {
                //tcp流上没有数据，则结束本次读取
                break Ok(0);
            },
            Ok(size) => {
                //tcp流上有数据，则移动已读取偏移
                *offset += size;
                if *offset == buf.len() {
                    //已填充外部指定大小的扩展缓冲区，则结束本次读取
                    break Ok(size);
                }
            }
            Err(err) => {
                if let ErrorKind::WouldBlock = err.kind() {
                    //tcp读取可能阻塞，则结束本次读取
                    println!("!!!> Recvive wouldblock by temp, token: {:?}, offset: {}", stream.token, *offset);
                    break Ok(0);
                }
                break Err(err);
            }
        }
    }
}

//让出指定tcp流的可读事件，线程安全的注销指定tcp流关注的读事件
fn yield_readable(handler: &TlsHandler, stream: Arc<RwLock<TlsStream>>, tcp_stream: &TcpStream, token: mio::Token) {
    let lock = &mut stream.read().unwrap();
    handler.tls_server.as_ref().unwrap().borrow().unregister_readable(&handler.poll, tcp_stream, token);
}

//调用外部注册的接收回调
fn callback_recv(stream: Arc<RwLock<TlsStream>>, recv_callback: RecvFn, range: Range<usize>) {
    let vec: Vec<u8>;
    let is_temp_recv = !stream.read().unwrap().temp_recv_buf.is_none();

    if is_temp_recv {
        vec = stream.write().unwrap().temp_recv_buf.take().unwrap()
            [..]
            .iter()
            .cloned()
            .collect();
    } else {
        vec = stream.read().unwrap().recv_buf[range]
            .iter()
            .cloned()
            .collect();
    }

    recv_callback(Ok(Arc::new(vec)));
}

/*
* 线程安全的处理外部通过tls连接发送数据的请求
*/
pub fn handle_send(handler: &mut TlsHandler, token_id: usize, bin: Arc<Vec<u8>>) {
    let mut is_close = false;
    {
        let mut slab = handler.slab.borrow_mut();
        if let Some(origin) = slab.get_mut(token_id) {
            match origin {
                &mut TlsOrigin::TcpStream(ref stream, ref mut tcp_stream, _) => {
                    let mut s = stream.write().unwrap();
                    is_close = send_tcp(&handler, tcp_stream, &mut s, mio::Token(token_id), bin);
                }
                _ => panic!("handle send failed, tls stream not exist"),
            }
        }
    }

    if is_close {
        handle_close(handler, token_id, true);
    }
}

//tls连接发送数据
fn send_tcp(handler: &TlsHandler, tcp_stream: &mut TcpStream, stream: &mut TlsStream, token: mio::Token, bin: Arc<Vec<u8>>) -> bool {
    match State::from_usize(stream.socket.as_ref().expect("tls socket not exist").state.load(Ordering::SeqCst)) {
        State::Closed => {
            //tls socket状态为已关闭
            panic!("send tcp failed, invalid tls socket state");
        },
        State::WouldClose => {
            //tls socket状态为关闭中，则忽略，并准备关闭tls socket
            true
        }
        State::Run => {
            //tls socket状态为运行中，则发送
            let bin_len = bin.len();
            if stream.send_remain_size + bin_len > stream.max_send_size {
                //当前待发送数据已超过最大发送大小，则忽略，并准备关闭tls socket
                true
            } else {
                stream.send_bufs.push_back(bin); //将待发送数据加入发送缓冲区尾部，等待唤醒当前tcp流的可写事件后处理

                if stream.send_remain_size == 0 {
                    //当前无待发送数据，则表示当前tcp流已让出可写事件，则将当前tcp流加入待唤醒列表中，准备发送数据
                    handler.wait_wakeup_writable.write().unwrap().push(token);
                }

                stream.send_remain_size += bin_len; //统计待发送数据大小
                false
            }
        }
    }
}

//处理tls流的可写事件
fn handle_write_stream_event(poll: &mut mio::Poll,
                             tcp_stream: &mut TcpStream,
                             stream: Arc<RwLock<TlsStream>>,
                             tls_server: Arc<RefCell<TlsServer>>,
                             event: &mio::Event) -> Option<bool> {
    //根据顺序处理发送缓冲区中的待发送数据
    let mut r = None; //忽略握手过程中的正常写入
    let token = event.token(); //获得事件源对应的令牌
    if stream.read().unwrap().handshake {
        //tls流已握手，则向tls流写入待发送缓冲区中剩余的待发送数据
        loop {
            let buf = stream.write().unwrap().send_bufs.pop_front();
            if let Some(bin) = buf {
                if fill_write_buffer(stream.clone(), tls_server.clone(), token, bin) {
                    //写入tls流成功，则继续发送
                    continue;
                } else {
                    //写入tls流错误，则准备关闭当前tcp流
                    r = Some(true);
                    break;
                }
            } else {
                //当前发送缓冲区为空，则结束发送
                r = Some(true);
                break;
            }
        }
    };

    //处理tls流的写缓冲区，并强制刷新tls流
    tls_server.borrow_mut().handle_write_stream(poll, tcp_stream, token.clone());
    if let Err(e) = tls_server.borrow_mut().flush(token.clone()) {
        //刷新错误
        println!("!!!> Handle write stream error, e:{:?}", e);
        r = Some(false);
    }

    r
}

//从待发送缓冲区中弹出一个待发送数据，并写入tls流的写缓冲区，返回是否成功
fn fill_write_buffer(stream: Arc<RwLock<TlsStream>>,
                     tls_server: Arc<RefCell<TlsServer>>,
                     token: mio::Token,
                     buf: Arc<Vec<u8>>) -> bool {
    let offset = stream.read().unwrap().send_buf_offset;
    match tls_server.borrow_mut().write_stream(token, &buf[offset..]) {
        Err(e) => {
            if let ErrorKind::WouldBlock = e.kind() {
                //写入tls流阻塞，则还原待发送缓冲区，返回失败，并等待下次继续写
                println!("!!!> Handle write stream would block: already write size: {}", offset);
                stream.write().unwrap().send_bufs.push_front(buf);
                false
            } else {
                //写入tls流错误，则返回失败
                println!("!!!> Handle write stream error, e:{:?}", e);
                false
            }
        },
        Ok(()) => {
            //写入tls流成功，则返回成功
            let size = buf.len(); //写入成功的数据大小
            let mut s = stream.write().unwrap();
            s.send_remain_size -= size; //减少待发送数据大小
            s.send_buf_offset = 0; //重置发送缓冲偏移
            true
        },
    }
}

//让出指定tcp流的可写事件，线程安全的注销指定tcp流关注的写事件
fn yield_writable(handler: &TlsHandler, stream: Arc<RwLock<TlsStream>>, tcp_stream: &TcpStream, token: mio::Token) {
    let lock = &mut stream.read().unwrap();
    handler.tls_server.as_ref().unwrap().borrow().unregister_writable(&handler.poll, tcp_stream, token);
}

//处理tls流超时事件
fn handle_timeout(handler: &TlsHandler) {
    if handler.tls_server.is_none() {
        //tls服务器还未初始化，则忽略
        return;
    }

    //轮询定时器，并处理所有超时事件
    handler.timers.write().unwrap().poll();
    let mut tokens: Vec<usize> = vec![];
    for (_, origin) in handler.slab.borrow().iter() {
        match origin {
            &TlsOrigin::TcpStream(ref stream, _, _) => {
                //处理tcp连接流的超时事件
                let s = &mut stream.read().unwrap();
                if s.recv_callback.is_none() || s.recv_timer.is_none() {
                    //接收回调或接收定时器不存在，则忽略
                    continue;
                }
                let timer = s.recv_timer.as_ref().unwrap();
                match timer.poll() {
                    Some(mio::Token(id)) => {
                        //记录已超时的tcp连接流对应的令牌
                        tokens.push(id);
                    },
                    None => (),
                }
            }
            _ => (), //忽略tcp连接监听器的超时事件
        }
    }

    //根据已超时的令牌，关闭对应的流
    if tokens.len() > 0 {
        println!("===> Close tokens, len = {}", tokens.len());
    }
    for id in tokens {
        handle_close(handler, id, true);
    }
}

/*
* 处理关闭tcp连接，读写事件错误、超时、外部发送请求错误和外部主动关闭都会导致关闭tcp连接
*/
pub fn handle_close(handler: &TlsHandler, token_id: usize, force: bool) {
    if let Some(origin) = handler.slab.borrow().get(token_id) {
        match origin {
            &TlsOrigin::TcpServer(_, _) => {
                //忽略还未接受的连接请求源
                panic!("close error, unable to close tcp listener");
            },
            &TlsOrigin::TcpStream(ref stream, ref tcp_stream, _) => {
                //关闭指定令牌对应的tcp连接，并回调关闭成功
                handler.tls_server.as_ref().unwrap().borrow_mut().close(mio::Token(token_id)); //关闭tls流
                close_tcp(&handler.poll, &mut stream.write().unwrap(), tcp_stream, force); //关闭tcp流
                let close_callback = &mut stream.write().unwrap().close_callback;
                if close_callback.is_some() {
                    //关闭回调存在，则取出并调用
                    let closed = close_callback.take().unwrap();
                    closed.call_box((token_id, Ok(())));
                }
            },
        }
    }

    if force {
        if handler.slab.borrow().contains(token_id) {
            //从tls的空间管理器内移除指定的tcp流
            handler.slab.borrow_mut().remove(token_id);
        }

    }
}

//关闭tcp流
fn close_tcp(poll: &mio::Poll, stream: &mut TlsStream, tcp_stream: &TcpStream, force: bool) {
    if force || stream.send_remain_size == 0 {
        if stream.socket.is_some() {
            //tcp socket存在，则改变状态
            stream.socket.as_ref().unwrap().state.swap(State::Closed as usize, Ordering::SeqCst);
        }

        poll.deregister(tcp_stream).unwrap();
        println!("===> Close tcp stream {:?}, shutdown", stream.token);
        if let Err(e) = tcp_stream.shutdown(Shutdown::Both) {
            println!("!!!> Close tcp stream err, e: {:?}", e);
        }
    } else {
        if stream.socket.is_some() {
            //tcp socket存在，则改变状态
            stream.socket.as_ref()
                .unwrap()
                .state.compare_exchange(State::Run as usize, State::WouldClose as usize, Ordering::SeqCst, Ordering::Acquire);
        }
    }
}

//写向量适配器
#[cfg(any(unix))]
struct WriteVAdapter<'a> {
    rawv: &'a mut Rawv
}

#[cfg(any(unix))]
impl<'a> WriteVAdapter<'a> {
    pub fn new(rawv: &'a mut Rawv) -> WriteVAdapter<'a> {
        WriteVAdapter { rawv }
    }
}

#[cfg(any(unix))]
impl<'a> rustls::WriteV for WriteVAdapter<'a> {
    fn writev(&mut self, bytes: &[&[u8]]) -> Result<usize> {
        self.rawv.writev(bytes)
    }
}

//加载TLS证书
fn load_certs(filename: &str) -> Vec<rustls::Certificate> {
    let certfile = fs::File::open(filename).expect("cannot open certificate file");
    let mut reader = BufReader::new(certfile);
    rustls::internal::pemfile::certs(&mut reader).unwrap()
}

//加载TLS私钥，支持pkcs8和rsa
fn load_private_key(filename: &str) -> rustls::PrivateKey {
    let rsa_keys = {
        let keyfile = fs::File::open(filename)
            .expect("cannot open private key file");
        let mut reader = BufReader::new(keyfile);
        rustls::internal::pemfile::rsa_private_keys(&mut reader)
            .expect("file contains invalid rsa private key")
    };

    let pkcs8_keys = {
        let keyfile = fs::File::open(filename)
            .expect("cannot open private key file");
        let mut reader = BufReader::new(keyfile);
        rustls::internal::pemfile::pkcs8_private_keys(&mut reader)
            .expect("file contains invalid pkcs8 private key (encrypted keys not supported)")
    };

    if !pkcs8_keys.is_empty() {
        //优先加载pkcs8密钥
        pkcs8_keys[0].clone()
    } else {
        //否则加载rsa密钥
        assert!(!rsa_keys.is_empty());
        rsa_keys[0].clone()
    }
}

fn move_vec(v: &mut Vec<u8>, src_offset: usize, dst_offset: usize, size: usize) {
    let len = v.len();
    if src_offset + size > len || dst_offset + size > len {
        panic!("move_vec failed!");
    }

    unsafe {
        let v = v.as_mut_ptr();
        v.wrapping_offset(src_offset as isize)
            .copy_to(v.wrapping_offset(dst_offset as isize), size);
    }
}