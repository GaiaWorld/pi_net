use std::io::Result;
use std::sync::{Arc, RwLock};
use std::sync::mpsc::Sender;
use std::sync::atomic::AtomicUsize;
use std::net::SocketAddr;
use std::time::{Duration};
use std::collections::VecDeque;

use slab::Slab;

use atom::Atom;
use apm::common::unregister_server_port;
use apm::counter::{GLOBAL_PREF_COLLECT, PrefCounter};
use mio::{Poll, Ready, Token};
use mio::net::{TcpListener, TcpStream};
use timer::{NetTimer, NetTimers, TimerCallback};

//TCP服务器前缀
const TCP_SERVER_PREFIX: &'static str = "tcp_server_";
//TCP服务器请求连接数量后缀
const TCP_SERVER_CONNECT_COUNT_SUFFIX: &'static str = "_connect_count";
//TCP服务器接受连接数量后缀
const TCP_SERVER_ACCEPTED_COUNT_SUFFIX: &'static str = "_accepted_count";
//TCP服务器关闭连接数量后缀
const TCP_SERVER_CLOSED_COUNT_SUFFIX: &'static str = "_closed_count";
//TCP服务器连接输入字节数量后缀
const TCP_SERVER_INPUT_BYTE_COUNT_SUFFIX: &'static str = "_input_byte_count";
//TCP服务器连接输出字节数量后缀
const TCP_SERVER_OUTPUT_BYTE_COUNT_SUFFIX: &'static str = "_output_byte_count";

pub type SendClosureFn = Box<FnOnce(&mut NetHandler) + Send>;

// close_callback(stream_id: usize, reason: Result<()>);
pub type CloseFn = Box<FnOnce(usize, Result<()>)>;

// recv_callback(vec: Result<Arc<Vec<u8>>>);
pub type RecvFn = Box<FnOnce(Result<Arc<Vec<u8>>>)>;

pub type ListenerFn = Box<Fn(Result<(RawSocket, Arc<RwLock<RawStream>>)>, Result<SocketAddr>) + Send>;

#[derive(Clone)]
pub struct RawSocket {
    pub state: Arc<AtomicUsize>,
    pub socket: usize,
    pub sender: Sender<SendClosureFn>,
    pub gray: Option<usize>,
    pub peer: Option<SocketAddr>,
    pub local: SocketAddr,
}

unsafe impl Sync for RawSocket {}
unsafe impl Send for RawSocket {}

pub enum Protocol {
    TCP,
}

pub struct Config {
    pub protocol: Protocol,
    pub addr: SocketAddr,
}

#[derive(Clone)]
pub enum Websocket {
    None,
    Bin(usize, usize), //offset, size
}

// all size's unit is byte
pub struct RawStream {
    pub token: Token,
    pub interest: Ready,

    pub max_send_size: usize,
    pub send_buf_offset: usize,  // the current offset of current buffer
    pub send_remain_size: usize, // all bufs's remain size
    pub send_bufs: VecDeque<Arc<Vec<u8>>>,

    pub recv_timeout: Option<Duration>,
    pub recv_timer: Arc<AtomicUsize>,
    pub timeout_token: Arc<AtomicUsize>,

    pub recv_buf: Vec<u8>,
    pub recv_size: usize,
    pub recv_buf_offset: usize, // offset from net, recv_buf_offset >= recv_callback_offset
    pub recv_callback_offset: usize, // offset from lastest callback

    pub temp_recv_buf: Option<Vec<u8>>,
    pub temp_recv_buf_offset: usize,

    pub recv_comings: Arc<RwLock<Vec<Token>>>,

    pub recv_callback: Option<RecvFn>,
    pub close_callback: Option<CloseFn>,

    pub net_timers: Arc<RwLock<NetTimers<TimerCallback>>>,

    //websocket
    pub websocket: Websocket,
    pub websocket_buf: Vec<u8>,
    //socket
    pub socket: Option<RawSocket>,
}

pub enum State {
    Run = 0,
    WouldClose = 1,
    Closed = 2,
}

impl State {
    //将数字转换为状态
    pub fn from_usize(n: usize) -> Self {
        match n {
            0 => State::Run,
            1 => State::WouldClose,
            _ => State::Closed,
        }
    }
}

pub enum NetData {
    TcpServer(ListenerFn, TcpListener),
    TcpStream(Arc<RwLock<RawStream>>, TcpStream),
}

pub struct NetCounter {
    pub connect_count:  PrefCounter,    //请求连接计数
    pub accepted_count: PrefCounter,    //接受连接计数
    pub closed_count:   PrefCounter,    //关闭连接计数
    pub input_byte:     PrefCounter,    //连接输入字节
    pub output_byte:    PrefCounter,    //连接输出字节
}

impl NetCounter {
    pub fn new(addr: SocketAddr) -> Self {
        let port = &addr.port().to_string();
        NetCounter {
            connect_count: GLOBAL_PREF_COLLECT.
                new_dynamic_counter(
                    Atom::from(TCP_SERVER_PREFIX.to_string() + port + TCP_SERVER_CONNECT_COUNT_SUFFIX), 0).unwrap(),
            accepted_count: GLOBAL_PREF_COLLECT.
                new_dynamic_counter(
                    Atom::from(TCP_SERVER_PREFIX.to_string() + port + TCP_SERVER_ACCEPTED_COUNT_SUFFIX), 0).unwrap(),
            closed_count: GLOBAL_PREF_COLLECT.
                new_dynamic_counter(
                    Atom::from(TCP_SERVER_PREFIX.to_string() + port + TCP_SERVER_CLOSED_COUNT_SUFFIX), 0).unwrap(),
            input_byte: GLOBAL_PREF_COLLECT.
                new_dynamic_counter(
                    Atom::from(TCP_SERVER_PREFIX.to_string() + port + TCP_SERVER_INPUT_BYTE_COUNT_SUFFIX), 0).unwrap(),
            output_byte: GLOBAL_PREF_COLLECT.
                new_dynamic_counter(
                    Atom::from(TCP_SERVER_PREFIX.to_string() + port + TCP_SERVER_OUTPUT_BYTE_COUNT_SUFFIX), 0).unwrap(),
        }
    }
}

pub struct NetHandler {
    pub port: u16,
    pub poll: Poll,
    pub slab: Slab<NetData>,
    pub sender: Sender<SendClosureFn>,
    pub recv_comings: Arc<RwLock<Vec<Token>>>,
    pub net_timers: Arc<RwLock<NetTimers<TimerCallback>>>,
    pub counter: Option<NetCounter>,
}

impl Drop for NetHandler {
    fn drop(&mut self) {
        //注销服务器端口信息
        unregister_server_port(self.port);
    }
}