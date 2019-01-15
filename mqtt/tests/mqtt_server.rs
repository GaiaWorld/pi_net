extern crate mqtt;
extern crate mqtt3;
extern crate net;
extern crate pi_lib;

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::thread;

use mqtt::data::Server;
use mqtt::server::{ServerNode, ClientStub};
use mqtt3::QoS;
use net::{Config, NetManager, Protocol, RawSocket, RawStream};

use pi_lib::atom::Atom;

use std::thread::sleep;
use std::time::Duration;
use std::io::Result as IOResult;

fn handle_close(stream_id: usize, reason: IOResult<()>) {
    println!(
        "server handle_close, stream_id = {}, reason = {:?}",
        stream_id, reason
    );
}

fn handle_publish(server: &mut ServerNode) {
    sleep(Duration::from_secs(10));
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

fn handle_bind(peer: IOResult<(RawSocket, Arc<RwLock<RawStream>>)>, addr: IOResult<SocketAddr>) {
    let (socket, stream) = peer.unwrap();
    println!(
        "server handle_bind: addr = {:?}, socket:{}",
        addr.unwrap(),
        socket.socket
    );
    let mut server = ServerNode::new();
    {
        let s = &mut stream.write().unwrap();

        // s.set_close_callback(Box::new(|id, reason| handle_close(id, reason)));
        //通过MQTT设置回调(自动注册遗言)
        server.set_close_callback(s, Box::new(|id, reason| handle_close(id, reason)));
        s.set_send_buf_size(1024 * 1024);
        s.set_recv_timeout(500 * 1000);
        s.set_socket(socket.clone());
    }

    server.add_stream(socket, stream);
    server.set_topic_meta(
        Atom::from(String::from("a/b/c").as_str()),
        true,
        true,
        //None,
        Box::new(|c, r| println!("a/b/c  publish ok!!! r:{:?}", r.unwrap())),
    );
    //遗言
    server.set_topic_meta(
        Atom::from(String::from("$last_will").as_str()),
        true,
        true,
        //None,
        Box::new(|c, r| println!("last_will  publish 遗言 ok!!! r:{:?}", r.unwrap())),
    );

    set_mqtt_topic(&server, "testTopic1".to_string(), true, true);
    set_mqtt_topic(&server, "testTopic2".to_string(), true, true);
    set_mqtt_topic(&server, "testTopic3".to_string(), true, true);
    set_mqtt_topic(&server, "testTopic4".to_string(), true, true);
    thread::spawn(move || handle_publish(&mut server));
}

fn set_mqtt_topic(server_node: &ServerNode, topic: String, can_publish: bool, can_subscribe: bool) -> Result<bool, String> {
    let topic = Atom::from(topic);
    let server_node1 = server_node.clone();
    match server_node.set_topic_meta(topic.clone(), can_publish,can_subscribe, Box::new(move |_c:ClientStub, r:IOResult<Arc<Vec<u8>>>| {
        match r {
            Ok(v) => {
                match server_node1.publish(false, mqtt3::QoS::AtMostOnce, topic.clone(),Vec::from(v.as_slice())) {
                    Ok(_) => (),
                    Err(s) => {println!("{}, topic:{}", s.to_string(), topic.as_str());},
                }
            },
            Err(s) => {
                println!("{}, topic:{}", s.to_string(), topic.as_str());
            },
        }
    })) {
        Ok(_) => Ok(true),
        Err(s) => Err(s.to_string()),
    } 
}

pub fn start_server() -> NetManager {
    let mgr = NetManager::new();
    let config = Config {
        protocol: Protocol::TCP,
        addr: "0.0.0.0:1234".parse().unwrap(),
    };
    mgr.bind(
        config,
        Box::new(move |peer, addr| handle_bind(peer, addr)),
    );
    return mgr;
}
