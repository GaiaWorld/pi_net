use std::thread;
use std::sync::Arc;
use std::str::FromStr;
use std::time::{Duration, Instant};
use std::net::{IpAddr, SocketAddr};
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, LocalBoxFuture};
use mqtt311::{TopicPath, Topic};
use env_logger;

use pi_async::rt::{serial::AsyncRuntimeBuilder, AsyncValue};

use tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus, SocketEvent,
          connect::TcpSocket,
          tls_connect::TlsSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::{TlsConfig, Ready}};
use ws::{server::WebsocketListener,
         connect::WsSocket,
         frame::WsHead,
         utils::{ChildProtocol, WsSession}};
use udp::terminal::UdpTerminal;
use quic::{server::{QuicListener, ClientCertVerifyLevel},
           utils::QuicSocketReady};
use pi_mqtt::{server::{register_mqtt_listener, register_mqtt_service,
                    register_mqtts_listener, register_mqtts_service,
                    register_quic_mqtt_listener, register_quic_mqtt_service,
                    MqttBrokerProtocol, WsMqttBrokerFactory, WssMqttBrokerFactory, QuicMqttBrokerFactory},
           broker::{MqttBrokerListener, MqttBrokerService},
           quic_broker::{MqttBrokerListener as QuicMqttBrokerListener, MqttBrokerService as QuicMqttBrokerService},
           session::MqttConnect,
           quic_session::MqttConnect as QuicMqttConnect,
           utils::{PathTree, BrokerSession, QuicBrokerSession}};

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
                 connect: Arc<dyn MqttConnect<S>>) -> LocalBoxFuture<'static, Result<()>> {
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
        }.boxed_local()
    }

    fn closed(&self,
              protocol: MqttBrokerProtocol,
              connect: Arc<dyn MqttConnect<S>>,
              context: BrokerSession,
              reason: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = reason {
                return println!("mqtt closed, connect: {:?}, reason: {:?}", connect, e);
            }

            println!("mqtt closed, connect: {:?}", connect);
        }.boxed_local()
    }
}

impl<S: Socket> MqttBrokerService<S> for TestBrokerService {
    //指定Mqtt客户端订阅指定主题的服务
    fn subscribe(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect<S>>,
                 topics: Vec<(String, u8)>) -> LocalBoxFuture<'static, Result<()>> {
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
        }.boxed_local()
    }

    //指定Mqtt客户端取消订阅指定主题的服务
    fn unsubscribe(&self,
                   protocol: MqttBrokerProtocol,
                   connect: Arc<dyn MqttConnect<S>>,
                   topics: Vec<String>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt unsubscribe, connect: {:?}", connect);
            Ok(())
        }.boxed_local()
    }

    //指定Mqtt客户端发布指定主题的服务
    fn publish(&self,
               protocol: MqttBrokerProtocol,
               connect: Arc<dyn MqttConnect<S>>,
               topic: String,
               payload: Arc<Vec<u8>>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            connect.send(&topic, payload)
        }.boxed_local()
    }
}

#[test]
fn test_mqtt_311() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

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
                               Some(1)) {
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

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

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
                               Some(1)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

struct TestQuicBrokerService;

impl QuicMqttBrokerListener for TestQuicBrokerService {
    fn connected(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn QuicMqttConnect>) -> LocalBoxFuture<'static, Result<()>> {
        async move{
            println!("mqtt connected, connect: {:?}", connect);

            if let Some(hibernate) = connect.hibernate(QuicSocketReady::ReadWrite) {
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
        }.boxed_local()
    }

    fn closed(&self,
              protocol: MqttBrokerProtocol,
              connect: Arc<dyn QuicMqttConnect>,
              context: QuicBrokerSession,
              reason: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = reason {
                return println!("mqtt closed, connect: {:?}, reason: {:?}", connect, e);
            }

            println!("mqtt closed, connect: {:?}", connect);
        }.boxed_local()
    }
}

impl QuicMqttBrokerService for TestQuicBrokerService {
    //指定Mqtt客户端订阅指定主题的服务
    fn subscribe(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn QuicMqttConnect>,
                 topics: Vec<(String, u8)>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt subscribe, connect: {:?}", connect);

            if let Some(hibernate) = connect.hibernate(QuicSocketReady::ReadWrite) {
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
        }.boxed_local()
    }

    //指定Mqtt客户端取消订阅指定主题的服务
    fn unsubscribe(&self,
                   protocol: MqttBrokerProtocol,
                   connect: Arc<dyn QuicMqttConnect>,
                   topics: Vec<String>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt unsubscribe, connect: {:?}", connect);
            Ok(())
        }.boxed_local()
    }

    //指定Mqtt客户端发布指定主题的服务
    fn publish(&self,
               protocol: MqttBrokerProtocol,
               connect: Arc<dyn QuicMqttConnect>,
               topic: String,
               payload: Arc<Vec<u8>>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            connect.send(&topic, payload)
        }.boxed_local()
    }
}

#[test]
fn test_quic_mqtt_311() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_udp_rt"), None);
    let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_quic_rt"), None);

    let broker_name = "test_quic_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(QuicMqttBrokerFactory::new(broker_name,
                                                             port));
    let service = Arc::new(TestQuicBrokerService);
    register_quic_mqtt_listener(broker_name, service.clone());
    register_quic_mqtt_service(broker_name, service);

    let listener = QuicListener::new(vec![quic_rt],
                                     "./tests/quic.com.crt",
                                     "./tests/quic.com.key",
                                     ClientCertVerifyLevel::Ignore,
                                     Default::default(),
                                     65535,
                                     65535,
                                     broker_factory.new_quic_service(),
                                     10)
        .expect("Create quic listener failed");
    let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), port);

    //用于Quic的udp连接监听器，有且只允许有一个运行时
    match UdpTerminal::bind(addrs,
                            udp_rt,
                            8 * 1024 * 1024,
                            8 * 1024 * 1024,
                            0xffff,
                            0xffff,
                            Box::new(listener)) {
        Err(e) => {
            println!("!!!> Quic mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Quic mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

struct TestPassiveBrokerService;

impl<S: Socket> MqttBrokerListener<S> for TestPassiveBrokerService {
    fn connected(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect<S>>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt connected, connect: {:?}", connect);

            connect.passive_receive(true);

            Ok(())
        }.boxed_local()
    }

    fn closed(&self,
              protocol: MqttBrokerProtocol,
              connect: Arc<dyn MqttConnect<S>>,
              context: BrokerSession,
              reason: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = reason {
                return println!("mqtt closed, connect: {:?}, reason: {:?}", connect, e);
            }

            println!("mqtt closed, connect: {:?}", connect);
        }.boxed_local()
    }
}

impl<S: Socket> MqttBrokerService<S> for TestPassiveBrokerService {
    //指定Mqtt客户端订阅指定主题的服务
    fn subscribe(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect<S>>,
                 topics: Vec<(String, u8)>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt subscribe, connect: {:?}, topic: {:?}", connect, topics);
            Ok(())
        }.boxed_local()
    }

    //指定Mqtt客户端取消订阅指定主题的服务
    fn unsubscribe(&self,
                   protocol: MqttBrokerProtocol,
                   connect: Arc<dyn MqttConnect<S>>,
                   topics: Vec<String>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            println!("mqtt unsubscribe, connect: {:?}", connect);
            Ok(())
        }.boxed_local()
    }

    //指定Mqtt客户端发布指定主题的服务
    fn publish(&self,
               protocol: MqttBrokerProtocol,
               connect: Arc<dyn MqttConnect<S>>,
               topic: String,
               payload: Arc<Vec<u8>>) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(5000));
                connect.send(&topic, payload);
                while !connect.wakeup(Ok(())) {
                    //唤醒被阻塞，则休眠指定时间后继续尝试唤醒
                    thread::sleep(Duration::from_millis(15));
                }
            });

            Ok(())
        }.boxed_local()
    }
}

#[test]
fn test_mqtt_311_passive() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

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
                               Some(1)) {
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

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

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
                               Some(1)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}