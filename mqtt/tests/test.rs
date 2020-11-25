use std::thread;
use std::sync::Arc;
use std::time::Duration;
use std::io::{ErrorKind, Result, Error};

use mqtt311::{TopicPath, Topic};

use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;
use tcp::util::TlsConfig;
use ws::{server::WebsocketListenerFactory,
         connect::WsSocket,
         frame::WsHead,
         util::{ChildProtocol, ChildProtocolFactory, WsSession}};
use mqtt::{server::{register_listener, register_service,
                    MqttBrokerProtocol, WsMqttBrokerFactory, WssMqttBrokerFactory},
           broker::{MqttBrokerListener, MqttBrokerService},
           session::MqttConnect,
           util::{PathTree, BrokerSession, AsyncResult}};

#[test]
fn test_topic_tree() {
    let mut tree: PathTree<usize> = PathTree::empty();

    tree.insert(TopicPath::from(r"sport/tennis/#"), 100);
    tree.insert(TopicPath::from(r"sport/tennis/+"), 300);
    tree.insert(TopicPath::from(r"sport/tennis/player1/+/abc/+/+/abc/+/#"), 1000);
    tree.insert(TopicPath::from(r"sport/tennis/+/+/abc/+/+/abc/+/+/+"), 3000);
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1")) {
        assert_eq!(&vec[..], &[100, 300]);
    }
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc")) {
        assert_eq!(&vec[..], &[100, 1000, 3000]);
    }

    tree.remove(TopicPath::from(r"sport/tennis/#"), 100);
    tree.remove(TopicPath::from(r"sport/tennis/+"), 300);
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1")) {
        let buf: &[usize] = &[];
        assert_eq!(&vec[..], buf);
    }
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc")) {
        assert_eq!(&vec[..], &[1000, 3000]);
    }

    tree.remove(TopicPath::from(r"sport/tennis/player1/+/abc/+/+/abc/+/#"), 1000);
    tree.remove(TopicPath::from(r"sport/tennis/+/+/abc/+/+/abc/+/+/+"), 3000);
    if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc")) {
        let buf: &[usize] = &[];
        assert_eq!(&vec[..], buf);
    }
}

struct TestBrokerService;

impl MqttBrokerListener for TestBrokerService {
    fn connected(&self, broker: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>) -> AsyncResult {
        println!("mqtt connected, connect: {:?}", connect);
        AsyncResult::with(Ok(()))
    }

    fn closed(&self, broker: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, context: BrokerSession, reason: Result<()>) {
        if let Err(e) = reason {
            return println!("mqtt closed, connect: {:?}, reason: {:?}", connect, e);
        }

        println!("mqtt closed, connect: {:?}", connect);
    }
}

impl MqttBrokerService for TestBrokerService {
    //指定Mqtt客户端订阅指定主题的服务
    fn subscribe(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, topics: Vec<(String, u8)>) -> AsyncResult {
        println!("mqtt subscribe, connect: {:?}", connect);
        AsyncResult::with(Ok(()))
    }

    //指定Mqtt客户端取消订阅指定主题的服务
    fn unsubscribe(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, topics: Vec<String>) -> Result<()> {
        println!("mqtt unsubscribe, connect: {:?}", connect);
        Ok(())
    }

    //指定Mqtt客户端发布指定主题的服务
    fn publish(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, topic: String, payload: Arc<Vec<u8>>) -> Result<()> {
        connect.send(&topic, payload)
    }
}

#[test]
fn test_mqtt_311() {
    let protocol_name = "mqttv3.1";
    let broker_name = "test_ws_mqtt";
    let port = 38080;

    let broker_factory = Arc::new(WsMqttBrokerFactory::new(protocol_name, broker_name, port));
    let service = Arc::new(TestBrokerService);
    register_listener(broker_name, service.clone());
    register_service(broker_name, service);

    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     broker_factory)));
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 2 * 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_tls_mqtt_311() {
    let protocol_name = "mqttv3.1";
    let broker_name = "test_wss_mqtt";
    let port = 38080;

    let broker_factory = Arc::new(WssMqttBrokerFactory::new(protocol_name, broker_name, port));
    let service = Arc::new(TestBrokerService);
    register_listener(broker_name, service.clone());
    register_service(broker_name, service);

    let mut factory = AsyncPortsFactory::<TlsSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListenerFactory::<TlsSocket>::with_protocol_factory(
                     broker_factory)));
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./3376363_msg.highapp.com.pem",
                                           "./3376363_msg.highapp.com.key",
                                           "",
                                           "",
                                           "",
                                           512,
                                           false,
                                           "").unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(port, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    match SocketListener::bind(factory, buffer, config, 1024, 2 * 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

struct TestPassiveBrokerService;

impl MqttBrokerListener for TestPassiveBrokerService {
    fn connected(&self, broker: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>) -> AsyncResult {
        println!("mqtt connected, connect: {:?}", connect);
        connect.passive_receive(true);
        AsyncResult::with(Ok(()))
    }

    fn closed(&self, broker: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, context: BrokerSession, reason: Result<()>) {
        if let Err(e) = reason {
            return println!("mqtt closed, connect: {:?}, reason: {:?}", connect, e);
        }

        println!("mqtt closed, connect: {:?}", connect);
    }
}

impl MqttBrokerService for TestPassiveBrokerService {
    //指定Mqtt客户端订阅指定主题的服务
    fn subscribe(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, topics: Vec<(String, u8)>) -> AsyncResult {
        println!("mqtt subscribe, connect: {:?}", connect);
        AsyncResult::with(Ok(()))
    }

    //指定Mqtt客户端取消订阅指定主题的服务
    fn unsubscribe(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, topics: Vec<String>) -> Result<()> {
        println!("mqtt unsubscribe, connect: {:?}", connect);
        Ok(())
    }

    //指定Mqtt客户端发布指定主题的服务
    fn publish(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect>, topic: String, payload: Arc<Vec<u8>>) -> Result<()> {
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(5000));
            connect.send(&topic, payload);
            connect.wakeup();
        });

        Ok(())
    }
}

#[test]
fn test_mqtt_311_passive() {
    let protocol_name = "mqttv3.1";
    let broker_name = "test_ws_mqtt";
    let port = 38080;

    let broker_factory = Arc::new(WsMqttBrokerFactory::new(protocol_name, broker_name, port));
    let service = Arc::new(TestPassiveBrokerService);
    register_listener(broker_name, service.clone());
    register_service(broker_name, service);

    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     broker_factory)));
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 2 * 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_tls_mqtt_311_passive() {
    let protocol_name = "mqttv3.1";
    let broker_name = "test_wss_mqtt";
    let port = 38080;

    let broker_factory = Arc::new(WssMqttBrokerFactory::new(protocol_name, broker_name, port));
    let service = Arc::new(TestPassiveBrokerService);
    register_listener(broker_name, service.clone());
    register_service(broker_name, service);

    let mut factory = AsyncPortsFactory::<TlsSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListenerFactory::<TlsSocket>::with_protocol_factory(
                     broker_factory)));
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./3376363_msg.highapp.com.pem",
                                           "./3376363_msg.highapp.com.key",
                                           "",
                                           "",
                                           "",
                                           512,
                                           false,
                                           "").unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(port, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    match SocketListener::bind(factory, buffer, config, 1024, 2 * 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}