use std::thread;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::io::Cursor;

use data::{Config, ListenerFn, NetHandler, SendClosureFn, Socket, State};
use net::{handle_bind, handle_close, handle_connect, handle_net, handle_send};
use websocket::ws::Sender as SenderT;
use websocket::message::CloseData;
use websocket::sender::{Sender as WsSender};
use websocket::OwnedMessage;

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

impl Socket {
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
