use std::io::Result;
use std::boxed::FnBox;
use std::sync::{Arc, RwLock};
use std::sync::mpsc::Sender;
use std::net::SocketAddr;
use std::time::{Duration};
use std::collections::VecDeque;

use slab::Slab;

use mio::{Poll, Ready, Token};
use mio::net::{TcpListener, TcpStream};
use timer::{NetTimer, NetTimers, TimerCallback};

pub type SendClosureFn = Box<FnBox(&mut NetHandler) + Send>;

// close_callback(stream_id: usize, reason: Result<()>);
pub type CloseFn = Box<FnBox(usize, Result<()>)>;

// recv_callback(vec: Result<Arc<Vec<u8>>>);
pub type RecvFn = Box<FnBox(Result<Arc<Vec<u8>>>)>;

pub type ListenerFn = Box<Fn(Result<(Socket, Arc<RwLock<Stream>>)>, Result<SocketAddr>) + Send>;

#[derive(Clone)]
pub struct Socket {
    pub socket: usize,
    pub sender: Sender<SendClosureFn>,
}

pub enum Protocol {
    TCP,
}

pub struct Config {
    pub protocol: Protocol,
    pub server_addr: Option<SocketAddr>,
}

// all size's unit is byte
pub struct Stream {
    pub state: State,
    pub token: Token,
    pub interest: Ready,

    pub max_send_size: usize,
    pub send_buf_offset: usize,  // the current offset of current buffer
    pub send_remain_size: usize, // all bufs's remain size
    pub send_bufs: VecDeque<Arc<Vec<u8>>>,

    pub recv_timeout: Option<Duration>,
    pub recv_timer: Option<NetTimer<Token>>,

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
}

pub enum State {
    Run = 0,
    WouldClose = 1,
    Closed = 2,
}

pub enum NetData {
    TcpServer(ListenerFn, TcpListener),
    TcpStream(Arc<RwLock<Stream>>, TcpStream),
}

pub struct NetHandler {
    pub poll: Poll,
    pub slab: Slab<NetData>,
    pub sender: Sender<SendClosureFn>,
    pub recv_comings: Arc<RwLock<Vec<Token>>>,
    pub net_timers: Arc<RwLock<NetTimers<TimerCallback>>>,
}