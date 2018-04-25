use std::thread;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::net::SocketAddr;

use data::{Config, ListenerFn, NetHandler, SendClosureFn, Socket};
use net::{handle_bind, handle_close, handle_connect, handle_net, handle_send};

pub struct NetManager {
    net_sender: Sender<SendClosureFn>,
}

impl NetManager {
    /// call by logic thread
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<Sender<SendClosureFn>>();

        // create net thread
        thread::spawn(move || {
            let (s, r) = mpsc::channel::<SendClosureFn>();
            sender.send(s.clone()).unwrap();

            handle_net(s, r);
        });

        // will block util receive data from net thread
        let net_sender = receiver.recv().unwrap();

        Self { net_sender }
    }

    /// call by logic thread
    pub fn bind(&self, addr: SocketAddr, config: Config, func: ListenerFn) {
        let data = Box::new(move |handler: &mut NetHandler| {
            handle_bind(handler, addr, config, func);
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

impl Socket {
    /// call by logic thread
    pub fn send(&self, buf: Arc<Vec<u8>>) {
        let socket = self.socket;
        let data = Box::new(move |handler: &mut NetHandler| {
            handle_send(handler, socket, buf);
        });

        self.sender.send(data).unwrap();
    }

    /// call by logic thread
    pub fn close(&self, force: bool) {
        let socket = self.socket;
        let data = Box::new(move |handler: &mut NetHandler| {
            handle_close(handler, socket, force);
        });

        self.sender.send(data).unwrap();
    }
}
