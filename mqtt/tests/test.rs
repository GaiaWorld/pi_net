use std::thread;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, BoxFuture};
use mqtt311::{TopicPath, Topic};
use env_logger;

use pi_async::rt::{AsyncRuntime, AsyncRuntimeBuilder, AsyncValue};

use tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus, SocketEvent,
          connect::TcpSocket,
          tls_connect::TlsSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::{TlsConfig, Ready}};
use ws::{server::WebsocketListener,
         connect::WsSocket,
         frame::WsHead,
         utils::{ChildProtocol, WsSession}};
use mqtt::{server::{register_mqtt_listener, register_mqtt_service,
                    register_mqtts_listener, register_mqtts_service,
                    MqttBrokerProtocol, WsMqttBrokerFactory, WssMqttBrokerFactory},
           broker::{MqttBrokerListener, MqttBrokerService},
           session::MqttConnect,
           utils::{PathTree, BrokerSession}};

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

impl<S: Socket> MqttBrokerListener<S> for TestBrokerService {
    fn connected(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect<S>>) -> BoxFuture<'static, Result<()>> {
        async move{
            println!("mqtt connected, connect: {:?}", connect);

            if let Some(hibernate) = connect.hibernate(Ready::ReadWrite) {
                let connect_copy = connect.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(1000));
                    while !connect_copy.wakeup(Ok(())) {
                        //唤醒被阻塞，则休眠指定时间后继续尝试唤醒
                        thread::sleep(Duration::from_millis(15));
                    }
                });
                let start = Instant::now();
                if let Err(e) = hibernate.await {
                    //唤醒后返回错误，则立即返回错误原因
                    return Err(e);
                }
                println!("!!!!!!wakeup hibernate ok, time: {:?}", start.elapsed());
            }

            println!("mqtt connected finish, connect: {:?}", connect);
            Ok(())
        }.boxed()
    }

    fn closed(&self,
              protocol: MqttBrokerProtocol,
              connect: Arc<dyn MqttConnect<S>>,
              context: BrokerSession,
              reason: Result<()>) {
        if let Err(e) = reason {
            return println!("mqtt closed, connect: {:?}, reason: {:?}", connect, e);
        }

        println!("mqtt closed, connect: {:?}", connect);
    }
}

impl<S: Socket> MqttBrokerService<S> for TestBrokerService {
    //指定Mqtt客户端订阅指定主题的服务
    fn subscribe(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect<S>>,
                 topics: Vec<(String, u8)>) -> BoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt subscribe, connect: {:?}", connect);

            if let Some(hibernate) = connect.hibernate(Ready::ReadWrite) {
                let connect_copy = connect.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(1000));
                    while !connect_copy.wakeup(Ok(())) {
                        //唤醒被阻塞，则休眠指定时间后继续尝试唤醒
                        thread::sleep(Duration::from_millis(15));
                    }
                });
                let start = Instant::now();
                if let Err(e) = hibernate.await {
                    //唤醒后返回错误，则立即返回错误原因
                    return Err(e);
                }
                println!("!!!!!!wakeup hibernate ok, time: {:?}", start.elapsed());
            }

            println!("mqtt subscribe finish, connect: {:?}", connect);
            Ok(())
        }.boxed()
    }

    //指定Mqtt客户端取消订阅指定主题的服务
    fn unsubscribe(&self,
                   protocol: MqttBrokerProtocol,
                   connect: Arc<dyn MqttConnect<S>>,
                   topics: Vec<String>) -> Result<()> {
        println!("mqtt unsubscribe, connect: {:?}", connect);
        Ok(())
    }

    //指定Mqtt客户端发布指定主题的服务
    fn publish(&self,
               protocol: MqttBrokerProtocol,
               connect: Arc<dyn MqttConnect<S>>,
               topic: String,
               payload: Arc<Vec<u8>>) -> Result<()> {
        connect.send(&topic, payload)
    }
}

#[test]
fn test_mqtt_311() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_worker_thread(None,
                                                        None,
                                                        None,
                                                        None);

    let protocol_name = "mqttv3.1";
    let broker_name = "test_ws_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WsMqttBrokerFactory::new(protocol_name,
                                                           broker_name,
                                                           port));
    let service = Arc::new(TestBrokerService);
    register_mqtt_listener(broker_name, service.clone());
    register_mqtt_service(broker_name, service);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListener::with_protocol(broker_factory.new_child_protocol())));
    let mut config = SocketConfig::new("0.0.0.0", factory.ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(10)) {
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
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_worker_thread(None,
                                                        None,
                                                        None,
                                                        None);

    let protocol_name = "mqttv3.1";
    let broker_name = "test_wss_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WssMqttBrokerFactory::new(protocol_name, broker_name, port));
    let service = Arc::new(TestBrokerService);
    register_mqtts_listener(broker_name, service.clone());
    register_mqtts_service(broker_name, service);

    let mut factory = PortsAdapterFactory::<TlsSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListener::with_protocol(broker_factory.new_child_protocol())));
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./tests/7285407__17youx.cn.pem",
                                           "./tests/7285407__17youx.cn.key",
                                           "",
                                           "",
                                           "",
                                           512,
                                           false,
                                           "").unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(port, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(10)) {
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

impl<S: Socket> MqttBrokerListener<S> for TestPassiveBrokerService {
    fn connected(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect<S>>) -> BoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt connected, connect: {:?}", connect);

            connect.passive_receive(true);

            Ok(())
        }.boxed()
    }

    fn closed(&self,
              protocol: MqttBrokerProtocol,
              connect: Arc<dyn MqttConnect<S>>,
              context: BrokerSession,
              reason: Result<()>) {
        if let Err(e) = reason {
            return println!("mqtt closed, connect: {:?}, reason: {:?}", connect, e);
        }

        println!("mqtt closed, connect: {:?}", connect);
    }
}

impl<S: Socket> MqttBrokerService<S> for TestPassiveBrokerService {
    //指定Mqtt客户端订阅指定主题的服务
    fn subscribe(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect<S>>,
                 topics: Vec<(String, u8)>) -> BoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt subscribe, connect: {:?}, topic: {:?}", connect, topics);
            Ok(())
        }.boxed()
    }

    //指定Mqtt客户端取消订阅指定主题的服务
    fn unsubscribe(&self,
                   protocol: MqttBrokerProtocol,
                   connect: Arc<dyn MqttConnect<S>>,
                   topics: Vec<String>) -> Result<()> {
        println!("mqtt unsubscribe, connect: {:?}", connect);
        Ok(())
    }

    //指定Mqtt客户端发布指定主题的服务
    fn publish(&self,
               protocol: MqttBrokerProtocol,
               connect: Arc<dyn MqttConnect<S>>,
               topic: String,
               payload: Arc<Vec<u8>>) -> Result<()> {
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(5000));
            connect.send(&topic, payload);
            while !connect.wakeup(Ok(())) {
                //唤醒被阻塞，则休眠指定时间后继续尝试唤醒
                thread::sleep(Duration::from_millis(15));
            }
        });

        Ok(())
    }
}

#[test]
fn test_mqtt_311_passive() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_worker_thread(None,
                                                        None,
                                                        None,
                                                        None);

    let protocol_name = "mqttv3.1";
    let broker_name = "test_ws_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WsMqttBrokerFactory::new(protocol_name, broker_name, port));
    let service = Arc::new(TestPassiveBrokerService);
    register_mqtt_listener(broker_name, service.clone());
    register_mqtt_service(broker_name, service);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListener::with_protocol(broker_factory.new_child_protocol())));
    let mut config = SocketConfig::new("0.0.0.0", factory.ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(10)) {
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
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_worker_thread(None,
                                                        None,
                                                        None,
                                                        None);

    let protocol_name = "mqttv3.1";
    let broker_name = "test_wss_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WssMqttBrokerFactory::new(protocol_name, broker_name, port));
    let service = Arc::new(TestPassiveBrokerService);
    register_mqtts_listener(broker_name, service.clone());
    register_mqtts_service(broker_name, service);

    let mut factory = PortsAdapterFactory::<TlsSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListener::with_protocol(broker_factory.new_child_protocol())));
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./tests/7285407__17youx.cn.pem",
                                           "./tests/7285407__17youx.cn.key",
                                           "",
                                           "",
                                           "",
                                           512,
                                           false,
                                           "").unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(port, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(10)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}