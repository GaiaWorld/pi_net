use std::thread;
use std::boxed::FnBox;
use std::sync::{Arc, RwLock};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::io::{Cursor, Result};
use std::net::SocketAddr;

use fnv::FnvHashMap;
use mio::Token;

use tls;
use data::{CloseFn, RecvFn, RawStream, Config, ListenerFn, NetHandler, SendClosureFn, RawSocket, State, Protocol};
use net::{handle_bind, handle_close, handle_connect, handle_net, handle_send};
use gray::GrayVersion;

use timer::{NetTimer, NetTimers, TimerCallback};
use websocket::ws::Sender as SenderT;
use websocket::message::CloseData;
use websocket::sender::{Sender as WsSender};
use websocket::OwnedMessage;

/*
* TLS连接管理器
*/
pub struct TlsManager {
    sender: Sender<tls::TlsExtRequest>,
}

impl TlsManager {
    //构建连接管理器
    pub fn new(recv_buff_size: usize) -> Self {
        if recv_buff_size == 0 || recv_buff_size > tls::MAX_TLS_RECV_SIZE {
            panic!("Invalid recv buff size, size: {}", recv_buff_size);
        }

        let (sender, receiver) = mpsc::channel::<tls::TlsExtRequest>();

        //启动TLS线程
        let sender_copy = sender.clone();

        thread::Builder::new()
            .name("TlsManagerThread".to_string())
            .stack_size(10 * 1024 * 1024)
            .spawn(move || {
                tls::startup(sender_copy, receiver, recv_buff_size);
            });

        Self { sender }
    }

    //在指定地址和端口上绑定tls的连接监听器
    pub fn bind(&self, config: tls::TlsConfig, func: tls::ListenerHandler) {
        let bind = Box::new(move |handler: &mut tls::TlsHandler| {
            tls::handle_bind(handler, config, func);
        });

        self.sender.send(bind).unwrap(); //将处理绑定的函数发送到TLS线程上执行
    }
}

//测试用socket实现
#[cfg(test)]
impl tls::TlsSocket {
    pub fn send(&self, buf: Arc<Vec<u8>>) {
        let socket = self.socket;
        let send = Box::new(move |handler: &mut tls::TlsHandler| {
            println!("!!!!!!send start");
            tls::handle_send(handler, socket, buf);
        });
        self.sender.send(send).unwrap();
    }
}

//测试安全的tcp服务器
#[test]
fn test_tls() {
    let mgr = TlsManager::new(tls::MAX_TLS_RECV_SIZE);
    let config = tls::TlsConfig::new(Protocol::TCP, "0.0.0.0:443".parse().unwrap(), "./1595835_herominer.net.pem", "./1595835_herominer.net.key");
    mgr.bind(
        config,
        Box::new(move |peer, addr| {
            //连接成功
            println!("!!!!!!connect ok, peer addr: {:?}", addr);

            let (socket, stream) = peer.unwrap();
            {
                let s = &mut stream.write().unwrap();
                s.set_send_buf_size(1024 * 1024);
                s.set_recv_timeout(500 * 1000);
                s.set_socket(socket.clone());
                match s.wakeup_readable(0, Box::new(move |result: Result<Arc<Vec<u8>>>| {
                    //接收成功
                    match result {
                        Err(e) => println!("read error, e: {:?}", e),
                        Ok(bin) => {
                            println!("!!!!!!read ok, data: {:?}", String::from_utf8_lossy(&bin[..]));
                            let response = b"HTTP/1.0 200 OK\r\nContent-Length: 35\r\nConnection: close\r\n\r\nHello world from rustls tlsserver\r\n";
                            socket.send(Arc::new(response.to_vec()));
                        }
                    }
                })) {
                    None => println!("!!!!!!async wait recv"), //等待异步接收数据
                    Some((cb, r)) => cb(r), //同步读取已接收未读取的数据
                }
            }
        }),
    );
    thread::sleep_ms(1000000);
}

pub struct NetManager {
    net_sender: Sender<SendClosureFn>,
}

impl NetManager {
    /// call by logic thread
    pub fn new() -> Self {
        let (s, r) = mpsc::channel::<SendClosureFn>();
        let net_sender = s.clone();

        // create net thread
        thread::spawn(move || {
            handle_net(s, r);
        });

        Self { net_sender }
    }

    /// call by logic thread
    pub fn bind(&self, config: Config, func: ListenerFn) {
        let data = Box::new(move |handler: &mut NetHandler| {
            handle_bind(handler, config, func);
        });

        self.net_sender.send(data).unwrap();
    }

    /// call by logic thread
    pub fn connect(&self, config: Config, func: ListenerFn) {
        let data = Box::new(move |handler: &mut NetHandler| {
            handle_connect(handler, config, func);
        });

        self.net_sender.send(data).unwrap();
    }
}

#[derive(Debug)]
pub enum WSControlType {
    Close(u16, String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
}

impl RawSocket {
    /// call by logic thread
    pub fn send(&self, buf: Arc<Vec<u8>>) {
        let mut sender = WsSender::new(false);
        let mut reader = Cursor::new(vec![]);
        let buf  = Vec::from(buf.as_slice());
        let message = OwnedMessage::Binary(buf);
        sender.send_dataframe(&mut reader, &message).is_ok();
        let buf = Arc::new(reader.into_inner());
        //println!("send------------------------{:?}", buf);
        let socket = self.socket;
        let data = Box::new(move |handler: &mut NetHandler| {
            handle_send(handler, socket, buf);
        });

        self.sender.send(data).unwrap();
    }

    pub fn send_bin(&self, buf: Arc<Vec<u8>>) {
        let socket = self.socket;
        //println!("send_bin-------------------------{:?}", buf);
        let data = Box::new(move |handler: &mut NetHandler| {
            handle_send(handler, socket, buf);
        });
        self.sender.send(data).unwrap();
    }

    //发送控制消息
    pub fn send_control(&self, msg: WSControlType) {
        let mut sender = WsSender::new(false);
        let mut reader = Cursor::new(vec![]);
        let socket = self.socket;
        let message = match msg {
            WSControlType::Close(state, reason) => {
                OwnedMessage::Close(Some(CloseData::new(state, reason)))
            },
            WSControlType::Ping(bin) => {
                OwnedMessage::Ping(bin)
            },
            WSControlType::Pong(bin) => {
                OwnedMessage::Pong(bin)
            },
        };
        sender.send_message(&mut reader, &message).expect(&format!("send control error, msg: {:?}", message));

        let cb = Box::new(move |handler: &mut NetHandler| {
            handle_send(handler, socket, Arc::new(reader.into_inner()));
        });
        self.sender.send(cb).unwrap();
    }

    /// call by logic thread
    pub fn close(&self, _force: bool) {
        //主动向对端发送连接关闭消息
        if let Ok(_) = self.state.compare_exchange(State::Run as usize, State::WouldClose as usize, Ordering::SeqCst, Ordering::Acquire) {
            self.send_control(WSControlType::Close(0, "server closed connect".to_string()));
        }
    }

    //实际关闭连接
    pub fn rclose(&self, force: bool) {
        if let Ok(_) = self.state.compare_exchange(State::WouldClose as usize, State::Closed as usize, Ordering::SeqCst, Ordering::Acquire) {
            let socket = self.socket;
            let cb = Box::new(move |handler: &mut NetHandler| {
                handle_close(handler, socket, force);
            });
            self.sender.send(cb).unwrap();
        }
    }
}

/*
* 公共socket
*/
#[derive(Clone)]
pub enum Socket {
    Raw(RawSocket),
    Tls(tls::TlsSocket),
}

unsafe impl Sync for Socket {}
unsafe impl Send for Socket {}

impl Socket {
    pub fn new_raw(id: usize, local: SocketAddr, peer: Option<SocketAddr>, sender: Sender<SendClosureFn>) -> Self {
        Socket::Raw(RawSocket::new(id, local, peer, sender))
    }

    pub fn new_tls(id: usize, local: SocketAddr, peer: Option<SocketAddr>, sender: Sender<tls::TlsExtRequest>) -> Self {
        Socket::Tls(tls::TlsSocket::new(id, local, peer, sender))
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        match self {
            Socket::Raw(socket) => {
                if let Some(addr) = socket.peer.as_ref() {
                    Some(addr.clone())
                } else {
                    None
                }
            },
            Socket::Tls(socket) => {
                if let Some(addr) = socket.peer.as_ref() {
                    Some(addr.clone())
                } else {
                    None
                }
            },
        }
    }
}

impl GrayVersion for Socket {
    fn get_gray(&self) -> &Option<usize> {
        match self {
            &Socket::Raw(ref s) => s.get_gray(),
            &Socket::Tls(ref s) => s.get_gray(),
        }
    }

    fn set_gray(&mut self, gray: Option<usize>) {
        match self {
            &mut Socket::Raw(ref mut s) => s.gray = gray,
            &mut Socket::Tls(ref mut s) => s.gray = gray,
        }
    }

    fn get_id(&self) -> usize {
        match self {
            &Socket::Raw(ref s) => s.socket,
            &Socket::Tls(ref s) => s.socket,
        }
    }
}

/*
* 公共流
*/
#[derive(Clone)]
pub enum Stream {
    Raw(Arc<RwLock<RawStream>>),
    Tls(Arc<RwLock<tls::TlsStream>>),
}

impl Stream {
    //构建一个公共流
    pub fn new(tls: bool, id: usize, recv_comings: Arc<RwLock<Vec<Token>>>, net_timers: Arc<RwLock<NetTimers<TimerCallback>>>) -> Self {
        if tls {
            return Stream::Tls(Arc::new(RwLock::new(tls::TlsStream::new(id, tls::MAX_TLS_RECV_SIZE, recv_comings, net_timers))));
        }
        Stream::Raw(Arc::new(RwLock::new(RawStream::new(id, recv_comings, net_timers))))
    }

    //设置关闭回调
    pub fn set_close_callback(&self, func: CloseFn) {
        match self {
            &Stream::Raw(ref s) => s.write().unwrap().set_close_callback(func),
            &Stream::Tls(ref s) => s.write().unwrap().set_close_callback(func),
        }
    }

    //设置发送缓冲区大小，单位byte
    pub fn set_send_buf_size(&self, size: usize) {
        match self {
            &Stream::Raw(ref s) => s.write().unwrap().set_send_buf_size(size),
            &Stream::Tls(ref s) => s.write().unwrap().set_send_buf_size(size),
        }
    }

    //设置接收超时时长，单位ms
    pub fn set_recv_timeout(&self, time: usize) {
        match self {
            &Stream::Raw(ref s) => s.write().unwrap().set_recv_timeout(time),
            &Stream::Tls(ref s) => s.write().unwrap().set_recv_timeout(time),
        }
    }

    //设置公共socket
    pub fn set_socket(&self, socket: Socket) {
        match socket {
            Socket::Raw(s) => {
                if let Stream::Raw(ref stream) = self {
                    stream.write().unwrap().set_socket((s));
                } else {
                    panic!("invalid raw socket");
                }
            },
            Socket::Tls(s) => {
                if let Stream::Tls(ref stream) = self {
                    stream.write().unwrap().set_socket(s);
                } else {
                    panic!("invalid tls socket");
                }
            }
        }
    }

    //接收处理，用于外部根据需要设置接收处理的相关参数
    pub fn recv_handle(&self, size: usize, func: RecvFn) -> Option<(RecvFn, Result<Arc<Vec<u8>>>)> {
        match self {
            &Stream::Raw(ref s) => s.write().unwrap().recv_handle(size, func),
            &Stream::Tls(ref s) => s.write().unwrap().wakeup_readable(size, func),
        }
    }
}

#[test]
pub fn test_net() {
    let mgr = NetManager::new();
    let config = Config {
        protocol: Protocol::TCP,
        addr: "0.0.0.0:443".parse().unwrap(),
    };

    mgr.bind(
        config,
        Box::new(move |peer, addr| {
            //连接成功
            println!("!!!!!!connect ok, peer addr: {:?}", addr);
        }),
    );

    thread::sleep_ms(1000000);
}

