extern crate net;
extern crate mqtt;
extern crate mqtt3;
extern crate string_cache;

use std::io::Result;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::thread;

use net::{Config, NetManager, Protocol, Socket, Stream};
use mqtt::{ServerNode, Server};
use mqtt3::{QoS};
use string_cache::atom::DefaultAtom as Atom;

use std::thread::sleep;
use std::time::Duration;

fn handle_close(stream_id: usize, reason: Result<()>) {
    println!(
        "server handle_close, stream_id = {}, reason = {:?}",
        stream_id, reason
    );
}

fn handle_publish(server: &mut ServerNode) {
    sleep(Duration::from_secs(3));
    println!("发布订阅消息1");
    server.publish(
            false,
            QoS::AtMostOnce,
            Atom::from(String::from("a/b/c").as_str()),
            vec![1],
        );
    sleep(Duration::from_secs(3));
    println!("发布订阅消息2");
    server.publish(
            false,
            QoS::AtMostOnce,
            Atom::from(String::from("a/b/c").as_str()),
            vec![2],
        );
    
}

fn handle_bind(peer: Result<(Socket, Arc<RwLock<Stream>>)>, addr: Result<SocketAddr>) {
    
    let (socket, stream) = peer.unwrap();
    println!("server handle_bind: addr = {:?}, socket:{}", addr.unwrap(), socket.socket);
    {
        let s = &mut stream.write().unwrap();

        s.set_close_callback(Box::new(|id, reason| handle_close(id, reason)));
        s.set_send_buf_size(1024 * 1024);
        s.set_recv_timeout(500 * 1000);
    }

    let mut server = ServerNode::new();
    server.add_stream(socket, stream);
    server.set_topic_meta(Atom::from(String::from("a/b/c").as_str()), true, true, None, Box::new(|r| println!("a/b/c  publish ok!!! r:{:?}", r.unwrap())));
    thread::spawn(move || handle_publish(&mut server));
}

pub fn start_server() -> NetManager {
    let mgr = NetManager::new();
    let config = Config {
        protocol: Protocol::TCP,
        server_addr: None,
    };
    let addr = "127.0.0.1:1234".parse().unwrap();
    mgr.bind(addr, config, Box::new(move |peer, addr| handle_bind(peer, addr)));
    return mgr;
}
