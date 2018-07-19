extern crate pi_lib;
extern crate mqtt;
extern crate mqtt3;
extern crate net;

use std::io::Result;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::thread;
use std::thread::sleep;
use std::time::Duration;

use mqtt::client::ClientNode;
use mqtt::data::Client;
use pi_lib::atom::Atom;

use mqtt3::{LastWill, QoS};
use net::{Config, NetManager, Protocol, Socket, Stream};

fn handle_close(stream_id: usize, reason: Result<()>) {
    println!(
        "client handle_close, stream_id = {}, reson = {:?}",
        stream_id, reason
    );
}
//
fn handle_mqtt(client: &mut ClientNode) {
    let mut subscribe = Vec::new();
    subscribe.push((String::from("a/b/c"), QoS::AtMostOnce));
    client.set_topic_handler(
        Atom::from(String::from("a/b/c").as_str()),
        Box::new(|r| println!("subscribe ok!!!!!!! r:{:?}", r.unwrap().1)),
    );
    //订阅主题
    match client.subscribe(
        subscribe,
        Some(Box::new(|_r| println!("subscribe ok##############"))),
    ) {
        Ok(_) => println!("client subscribe ok!!!!!!!!!!!!"),
        Err(e) => println!("client subscribe err!!!!!!!!!!!!e:{}", e),
    };
}

fn handle_connect(peer: Result<(Socket, Arc<RwLock<Stream>>)>, addr: Result<SocketAddr>) {
    let (socket, stream) = peer.unwrap();
    println!(
        "client handle_connect: addr = {:?}, socket:{}",
        addr.unwrap(),
        socket.socket
    );
    {
        let stream = &mut stream.write().unwrap();

        stream.set_close_callback(Box::new(|id, reason| handle_close(id, reason)));
        stream.set_send_buf_size(1024 * 1024);
        stream.set_recv_timeout(500 * 1000);
    }

    let mut client_node = ClientNode::new();
    client_node.set_stream(socket, stream);
    //遗言
    let last_will = LastWill {
        topic: String::from("test_last_will"),
        message: String::from("{clientid:1, msg:'xxx'}"),
        qos: QoS::AtMostOnce,
        retain: false,
    };
    client_node.connect(
        30,
        Some(last_will),
        Some(Box::new(|_r| println!("client handle_close ok "))),
        Some(Box::new(|_r| {
            println!("client connect ok!!!!!!!!!");
        })),
    );
    handle_mqtt(&mut client_node);
    thread::spawn(move || client_handle(client_node));
}

pub fn start_client() -> NetManager {
    let mgr = NetManager::new();
    let config = Config {
        protocol: Protocol::TCP,
        addr: "127.0.0.1:1234".parse().unwrap(),
    };
    mgr.connect(config, Box::new(|peer, addr| handle_connect(peer, addr)));

    return mgr;
}

fn client_handle(mut client: ClientNode) {
    //取消订阅
    thread::spawn(move || {
        sleep(Duration::from_secs(6));
        println!("取消订阅");
        client.unsubscribe(
            vec![String::from("a/b/c")],
            Some(Box::new(|_r| println!("unsub ok!!!!!!!!!!!!"))),
        );
        //发布主题
        thread::spawn(move || {
            sleep(Duration::from_secs(7));
            println!("客户端发布主题");
            client.publish(
                false,
                QoS::AtMostOnce,
                Atom::from(String::from("a/b/c").as_str()),
                vec![10],
            );
            //关闭连接
            // thread::spawn(move || {
            //     sleep(Duration::from_secs(8));
            //     println!("关闭连接");
            //     client.disconnect()
            // });
        });
    });
}
