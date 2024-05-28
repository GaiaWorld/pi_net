#![feature(once_cell)]

use std::thread;
use std::sync::Arc;
use std::str::FromStr;
use std::cell::OnceCell;
use std::marker::PhantomData;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use futures::future::{FutureExt, LocalBoxFuture};
use quinn_proto::{TransportConfig, VarInt};
use env_logger;
use pi_async_rt::prelude::{MultiTaskRuntimeBuilder, WorkerRuntime};

use pi_async_rt::rt::{AsyncRuntime, AsyncRuntimeBuilder as RTBuilder, startup_global_time_loop,
                      serial::AsyncRuntimeBuilder,
                      multi_thread::MultiTaskRuntime};
use pi_atom::Atom;
use pi_gray::GrayVersion;
use pi_handler::{Args, Handler};

use tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus, SocketEvent,
          connect::TcpSocket,
          tls_connect::TlsSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::{TlsConfig, Ready}};
use ws::server::WebsocketListener;
use udp::terminal::UdpTerminal;
use quic::{server::{QuicListener, ClientCertVerifyLevel},
           utils::QuicSocketReady};
use mqtt::server::{WsMqttBrokerFactory, WssMqttBrokerFactory, QuicMqttBrokerFactory,
                   register_mqtt_listener, register_mqtt_service, register_quic_mqtt_service,
                   register_mqtts_listener, register_mqtts_service, register_quic_mqtt_listener,
                   publish_topic};

use pi_mqtt_proxy::{service::{MqttEvent, MqttConnectHandle, MqttProxyListener, MqttProxyService},
                    quic_service::{MqttEvent as QuicMqttEvent, MqttConnectHandle as QuicMqttConnectHandle, MqttProxyListener as QuicMqttProxyListener, MqttProxyService as QuicMqttProxyService}};

static mut handler_runtime: OnceCell<MultiTaskRuntime<()>> = OnceCell::new();

struct TestMqttConnectHandler<S: Socket>(PhantomData<S>);

unsafe impl<S: Socket> Send for TestMqttConnectHandler<S> {}
unsafe impl<S: Socket> Sync for TestMqttConnectHandler<S> {}

impl<S: Socket> Handler for TestMqttConnectHandler<S> {
    type A = MqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self,
              env: Arc<dyn GrayVersion>, _: Atom,
              args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        async move {
            let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle<S>) };
            match args {
                Args::OneArgs(MqttEvent::Connect(socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd)) => {
                    //处理Mqtt连接
                    if let Some(hibernate) = connect.hibernate(Ready::ReadWrite) {
                        let connect_copy = connect.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(1000));
                            while !connect_copy.wakeup(Ok(())) {
                                thread::sleep(Duration::from_millis(1));
                            }
                        });
                        let start = Instant::now();
                        if let Err(e) = hibernate.await {
                            //唤醒后返回错误，则立即关闭当前连接
                            connect.close(Err(e));
                        }
                        println!("!!!!!!wakeup hibernate ok, time: {:?}", start.elapsed());
                    }
                    println!("!!!!!!Connect, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, keep_alive: {:?}, is_clean_session: {:?}, user: {:?}, pwd: {:?}", socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd);
                }
                Args::OneArgs(MqttEvent::Disconnect(socket_id, broker_name, client_id, reason)) => {
                    //处理Mqtt连接关闭
                    println!("!!!!!!Disconnect, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, reason: {:?}", socket_id, broker_name, client_id, reason);
                },
                _ => {
                    println!("!!!!!!Invalid mqtt event");
                },
            }
        }.boxed_local()
    }
}

impl<S: Socket> TestMqttConnectHandler<S> {
    pub fn new() -> Self {
        TestMqttConnectHandler(PhantomData)
    }
}

struct TestMqttRequestHandler<S: Socket>(PhantomData<S>);

unsafe impl<S: Socket> Send for TestMqttRequestHandler<S> {}
unsafe impl<S: Socket> Sync for TestMqttRequestHandler<S> {}

impl<S: Socket> Handler for TestMqttRequestHandler<S> {
    type A = MqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        async move {
            let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle<S>) };
            match args {
                Args::OneArgs(MqttEvent::Sub(socket_id, broker_name, client_id, topics)) => {
                    //处理Mqtt订阅主题
                    for (topic, _) in topics.clone() {
                        connect.sub(topic);
                    }

                    if let Some(hibernate) = connect.hibernate(Ready::ReadWrite) {
                        let connect_copy = connect.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(1000));
                            while !connect_copy.wakeup(Ok(())) {
                                thread::sleep(Duration::from_millis(1));
                            }
                        });
                        let start = Instant::now();
                        if let Err(e) = hibernate.await {
                            //唤醒后返回错误，则立即关闭当前连接
                            connect.close(Err(e));
                        }
                        println!("!!!!!!wakeup hibernate ok, time: {:?}", start.elapsed());
                    }
                    println!("!!!!!!Sub, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}", socket_id, broker_name, client_id, topics);
                },
                Args::OneArgs(MqttEvent::Unsub(socket_id, broker_name, client_id, topics)) => {
                    //处理Mqtt退订主题
                    println!("!!!!!!Unsub, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}", socket_id, broker_name, client_id, topics);

                    for topic in topics {
                        connect.unsub(topic);
                    }
                },
                Args::OneArgs(MqttEvent::Publish(socket_id, broker_name, client_id, address, topic, payload)) => {
                    //处理Mqtt发布主题
                    let rt = unsafe {
                        handler_runtime.get_or_init(move || {
                            RTBuilder::default_multi_thread(None, None, None, None)
                        })
                    };

                    let rt_copy = rt.clone();
                    rt.spawn(async move {
                        rt_copy.timeout(10).await;
                        connect.send(&"rpc/send".to_string(), address.unwrap().to_string().into_bytes());
                        connect.reply(payload.as_slice().to_vec());
                    });
                },
                _ => {
                    println!("!!!!!!Invalid mqtt event");
                },
            }
        }.boxed_local()
    }
}

impl<S: Socket> TestMqttRequestHandler<S> {
    pub fn new() -> Self {
        TestMqttRequestHandler(PhantomData)
    }
}

#[test]
fn test_mqtt_proxy_service() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let _handle = startup_global_time_loop(10);
    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let protocol_name = "mqttv3.1";
    let broker_name = "test_ws_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WsMqttBrokerFactory::new(protocol_name,
                                                           broker_name,
                                                           port));
    let event_handler = Arc::new(TestMqttConnectHandler::<TcpSocket>::new());
    let rpc_handler = Arc::new(TestMqttRequestHandler::<TcpSocket>::new());
    let listener = Arc::new(MqttProxyListener::with_handler(Some(event_handler)));
    let service = Arc::new(MqttProxyService::with_handler(Some(rpc_handler)));
    register_mqtt_listener(broker_name, listener);
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
            println!("!!!> Rpc Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Rpc Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_tls_mqtt_proxy_service() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let protocol_name = "mqttv3.1";
    let broker_name = "test_wss_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WssMqttBrokerFactory::new(protocol_name, broker_name, port));
    let event_handler = Arc::new(TestMqttConnectHandler::<TlsSocket>::new());
    let rpc_handler = Arc::new(TestMqttRequestHandler::<TlsSocket>::new());
    let listener = Arc::new(MqttProxyListener::with_handler(Some(event_handler)));
    let service = Arc::new(MqttProxyService::with_handler(Some(rpc_handler)));
    register_mqtts_listener(broker_name, listener);
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
            println!("!!!> Rpc Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Rpc Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

struct TestMqttSubUnsubRequestHandler<R: AsyncRuntime, S: Socket>(R, PhantomData<S>);

unsafe impl<R: AsyncRuntime, S: Socket> Send for TestMqttSubUnsubRequestHandler<R, S> {}
unsafe impl<R: AsyncRuntime, S: Socket> Sync for TestMqttSubUnsubRequestHandler<R, S> {}

impl<R: AsyncRuntime, S: Socket> Handler for TestMqttSubUnsubRequestHandler<R, S> {
    type A = MqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        let rt = self.0.clone();
        async move {
            let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle<S>) };
            match args {
                Args::OneArgs(MqttEvent::Sub(socket_id, broker_name, client_id, topics)) => {
                    //处理Mqtt订阅主题
                    let rt_copy = rt.clone();
                    let topics_copy = topics.clone();
                    rt.spawn(async move {
                        let mut buf = Vec::with_capacity(1000);
                        loop {
                            if buf.len() >= 1000 {
                                println!("======> sub time: {:?}ns", buf.iter().sum::<Duration>().as_nanos() / buf.len() as u128);
                                buf.clear();
                            }
                            rt_copy.timeout(10).await;
                            for (topic, _) in topics_copy.clone() {
                                let now = Instant::now();
                                connect.sub(topic);
                                buf.push(now.elapsed());
                            }
                        }
                    });

                    let rt_copy = rt.clone();
                    let broker_name_copy = broker_name.clone();
                    let topics_copy = topics.clone();
                    rt.spawn(async move {
                        let payload = Arc::new(vec![]);
                        let mut buf = Vec::with_capacity(1000);
                        loop {
                            if buf.len() >= 1000 {
                                println!("======> publish time: {:?}ns", buf.iter().sum::<Duration>().as_nanos() / buf.len() as u128);
                                buf.clear();
                            }
                            rt_copy.timeout(10).await;
                            for (topic, _) in topics_copy.clone() {
                                let now = Instant::now();
                                let r = publish_topic(Some(broker_name_copy.clone()), true, topic, 0, None, payload.clone());
                                buf.push(now.elapsed());
                                assert!(r.is_ok());
                            }
                        }
                    });

                    println!("!!!!!!Sub, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}", socket_id, broker_name, client_id, topics);
                },
                Args::OneArgs(MqttEvent::Unsub(socket_id, broker_name, client_id, topics)) => {
                    //处理Mqtt退订主题
                    println!("!!!!!!Unsub, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}", socket_id, broker_name, client_id, topics);

                    let rt_copy = rt.clone();
                    rt.spawn(async move {
                        let mut buf = Vec::with_capacity(1000);
                        loop {
                            if buf.len() >= 1000 {
                                println!("======> unsub time: {:?}ns", buf.iter().sum::<Duration>().as_nanos() / buf.len() as u128);
                                buf.clear();
                            }
                            rt_copy.timeout(10).await;
                            for topic in topics.clone() {
                                let now = Instant::now();
                                connect.unsub(topic);
                                buf.push(now.elapsed());
                            }
                        }
                    });
                },
                Args::OneArgs(MqttEvent::Publish(socket_id, broker_name, client_id, address, topic, payload)) => {
                    //处理Mqtt发布主题
                    let rt = unsafe {
                        handler_runtime.get_or_init(move || {
                            RTBuilder::default_multi_thread(None, None, None, None)
                        })
                    };

                    let rt_copy = rt.clone();
                    rt.spawn(async move {
                        rt_copy.timeout(10).await;
                        connect.send(&"rpc/send".to_string(), address.unwrap().to_string().into_bytes());
                        connect.reply(payload.as_slice().to_vec());
                    });
                },
                _ => {
                    println!("!!!!!!Invalid mqtt event");
                },
            }
        }.boxed_local()
    }
}

impl<R: AsyncRuntime, S: Socket> TestMqttSubUnsubRequestHandler<R, S> {
    pub fn new(rt: R) -> Self {
        TestMqttSubUnsubRequestHandler(rt, PhantomData)
    }
}

#[test]
fn test_mqtt_sub_and_unsub() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let _handle = startup_global_time_loop(10);
    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt_worker = RTBuilder::default_multi_thread(None, None, None, None);

    let protocol_name = "mqttv3.1";
    let broker_name = "test_ws_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WsMqttBrokerFactory::new(protocol_name,
                                                           broker_name,
                                                           port));
    let event_handler = Arc::new(TestMqttConnectHandler::<TcpSocket>::new());
    let rpc_handler = Arc::new(TestMqttSubUnsubRequestHandler::<MultiTaskRuntime, TcpSocket>::new(rt_worker));
    let listener = Arc::new(MqttProxyListener::with_handler(Some(event_handler)));
    let service = Arc::new(MqttProxyService::with_handler(Some(rpc_handler)));
    register_mqtt_listener(broker_name, listener);
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
            println!("!!!> Rpc Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Rpc Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

struct TestQuicMqttConnectHandler;

unsafe impl Send for TestQuicMqttConnectHandler {}
unsafe impl Sync for TestQuicMqttConnectHandler {}

impl Handler for TestQuicMqttConnectHandler {
    type A = QuicMqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self,
              env: Arc<dyn GrayVersion>, _: Atom,
              args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        async move {
            let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const QuicMqttConnectHandle) };
            match args {
                Args::OneArgs(QuicMqttEvent::Connect(socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd)) => {
                    //处理Mqtt连接
                    if let Some(hibernate) = connect.hibernate(QuicSocketReady::ReadWrite) {
                        let connect_copy = connect.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(1000));
                            while !connect_copy.wakeup(Ok(())) {
                                thread::sleep(Duration::from_millis(1));
                            }
                        });
                        let start = Instant::now();
                        if let Err(e) = hibernate.await {
                            //唤醒后返回错误，则立即关闭当前连接
                            connect.close(1000000, Err(e));
                        }
                        println!("!!!!!!wakeup hibernate ok, time: {:?}", start.elapsed());
                    }
                    println!("!!!!!!Connect, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, keep_alive: {:?}, is_clean_session: {:?}, user: {:?}, pwd: {:?}", socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd);
                }
                Args::OneArgs(QuicMqttEvent::Disconnect(socket_id, broker_name, client_id, reason)) => {
                    //处理Mqtt连接关闭
                    println!("!!!!!!Disconnect, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, reason: {:?}", socket_id, broker_name, client_id, reason);
                },
                _ => {
                    println!("!!!!!!Invalid mqtt event");
                },
            }
        }.boxed_local()
    }
}

impl TestQuicMqttConnectHandler {
    pub fn new() -> Self {
        TestQuicMqttConnectHandler
    }
}

struct TestQuicMqttRequestHandler;

unsafe impl Send for TestQuicMqttRequestHandler {}
unsafe impl Sync for TestQuicMqttRequestHandler {}

impl Handler for TestQuicMqttRequestHandler {
    type A = QuicMqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        async move {
            let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const QuicMqttConnectHandle) };
            match args {
                Args::OneArgs(QuicMqttEvent::Sub(socket_id, broker_name, client_id, topics)) => {
                    //处理Mqtt订阅主题
                    for (topic, _) in topics.clone() {
                        connect.sub(topic);
                    }

                    if let Some(hibernate) = connect.hibernate(QuicSocketReady::ReadWrite) {
                        let connect_copy = connect.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(1000));
                            while !connect_copy.wakeup(Ok(())) {
                                thread::sleep(Duration::from_millis(1));
                            }
                        });
                        let start = Instant::now();
                        if let Err(e) = hibernate.await {
                            //唤醒后返回错误，则立即关闭当前连接
                            connect.close(1000000, Err(e));
                        }
                        println!("!!!!!!wakeup hibernate ok, time: {:?}", start.elapsed());
                    }
                    println!("!!!!!!Sub, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}", socket_id, broker_name, client_id, topics);
                },
                Args::OneArgs(QuicMqttEvent::Unsub(socket_id, broker_name, client_id, topics)) => {
                    //处理Mqtt退订主题
                    println!("!!!!!!Unsub, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}", socket_id, broker_name, client_id, topics);

                    for topic in topics {
                        connect.unsub(topic);
                    }
                },
                Args::OneArgs(QuicMqttEvent::Publish(socket_id, broker_name, client_id, address, topic, payload)) => {
                    //处理Mqtt发布主题
                    connect.send(&"rpc/send".to_string(), address.unwrap().to_string().into_bytes());
                    connect.reply(payload.as_slice().to_vec());
                },
                _ => {
                    println!("!!!!!!Invalid mqtt event");
                },
            }
        }.boxed_local()
    }
}

impl TestQuicMqttRequestHandler {
    pub fn new() -> Self {
        TestQuicMqttRequestHandler
    }
}

#[test]
fn test_mqtt_proxy_service_by_quic() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_udp_rt"), None);
    let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_quic_rt"), None);

    let broker_name = "test_quic_mqtt";
    let port = 38080;

    //构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(QuicMqttBrokerFactory::new(broker_name,
                                                             port));
    let event_handler = Arc::new(TestQuicMqttConnectHandler::new());
    let rpc_handler = Arc::new(TestQuicMqttRequestHandler::new());
    let listener = Arc::new(QuicMqttProxyListener::with_handler(Some(event_handler)));
    let service = Arc::new(QuicMqttProxyService::with_handler(Some(rpc_handler)));
    register_quic_mqtt_listener(broker_name, listener);
    register_quic_mqtt_service(broker_name, service);

    let mut transport_config = TransportConfig::default();
    transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
    let listener = QuicListener::new(vec![quic_rt],
                                     "./tests/quic.com.crt",
                                     "./tests/quic.com.key",
                                     ClientCertVerifyLevel::Ignore,
                                     Default::default(),
                                     65535,
                                     65535,
                                     100000,
                                     Some(Arc::new(transport_config)),
                                     broker_factory.new_quic_service(),
                                     1)
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
            println!("!!!> Rpc Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Rpc Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}








