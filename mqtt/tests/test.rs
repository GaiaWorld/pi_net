use std::thread;
use std::sync::Arc;
use std::time::Duration;
use std::io::{ErrorKind, Result, Error};

use mqtt311::{TopicPath, Topic};

use tcp::connect::TcpSocket;
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;

use ws::{server::WebsocketListenerFactory,
         connect::WsSocket,
         frame::WsHead,
         util::{ChildProtocol, ChildProtocolFactory, WsSession}};

use mqtt::{v311::{WsMqtt311, WsMqtt311Factory}, util::PathTree};

#[test]
fn test_topic_tree() {
    let path = TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc");

    let mut is_match = true;
    let pattern = TopicPath::from(r"sport/tennis/player1/+/abc/+/+/abc/+/#");

    let path_len = path.len();
    let pattern_len = pattern.len();
    if path_len < pattern_len {
        let level_diff = pattern_len - path_len;
        if level_diff > 1 {
            is_match = false;
        } else if level_diff == 1 {
            if pattern.is_multi(pattern_len - 1) {
                let index = pattern_len - 2;
                is_match = pattern.get(index).unwrap().fit(path.get(index).unwrap());
            } else {
                is_match = false;
            }
        } else {
            is_match = false;
        }
    } else {
        let level_diff = path_len - pattern_len;
        for index in 0..path_len {
            if pattern.is_final(index) {
                if pattern.is_multi(index) {
                    break;
                }
            }

            if let Some(p) = pattern.get(index) {
                if p.fit(path.get(index).unwrap()) {
                    continue;
                }
            }

            is_match = false;
            break;
        }
    }

    assert!(is_match, true);

    let mut tree: PathTree<usize> = PathTree::empty();
    println!("tree: {:?}", tree);

    tree.insert(TopicPath::from(r"sport/tennis/#"), 100);
    tree.insert(TopicPath::from(r"sport/tennis/+"), 300);
    tree.insert(TopicPath::from(r"sport/tennis/player1/+/abc/+/+/abc/+/#"), 1000);
    tree.insert(TopicPath::from(r"sport/tennis/+/+/abc/+/+/abc/+/+/+"), 3000);
    println!("tree: {:?}", tree);
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1")) {
        assert_eq!(&vec[..], &[100, 300]);
    }
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc")) {
        assert_eq!(&vec[..], &[100, 1000, 3000]);
    }

    tree.remove(TopicPath::from(r"sport/tennis/#"), 100);
    tree.remove(TopicPath::from(r"sport/tennis/+"), 300);
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1")) {
        assert_eq!(&vec[..], &[]);
    }
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc")) {
        assert_eq!(&vec[..], &[1000, 3000]);
    }

    tree.remove(TopicPath::from(r"sport/tennis/player1/+/abc/+/+/abc/+/#"), 1000);
    tree.remove(TopicPath::from(r"sport/tennis/+/+/abc/+/+/abc/+/+/+"), 3000);
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc")) {
        assert_eq!(&vec[..], &[]);
    }
}

#[test]
fn test_mqtt_311() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     Arc::new(WsMqtt311Factory::with_name("mqttv3.1")))));
    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}