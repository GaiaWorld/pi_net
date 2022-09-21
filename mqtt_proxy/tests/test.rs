use std::thread;
use std::any::Any;
use std::sync::Arc;
use std::mem::transmute;
use std::time::Duration;
use std::net::SocketAddr;
use std::marker::PhantomData;

use env_logger;

use pi_async::rt::{serial::{AsyncRuntime, AsyncRuntimeBuilder, AsyncValue}};
use pi_atom::Atom;
use pi_gray::GrayVersion;
use pi_handler::{Args, Handler};

use tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus, SocketEvent,
          connect::TcpSocket,
          tls_connect::TlsSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::{TlsConfig, Ready}};
use ws::{server::WebsocketListener,
         connect::WsSocket,
         frame::WsHead,
         utils::{ChildProtocol, WsSession}};
use mqtt::server::{WsMqttBrokerFactory, WssMqttBrokerFactory,
                   register_mqtt_listener, register_mqtt_service,
                   register_mqtts_listener, register_mqtts_service};

use mqtt_proxy::service::{MqttEvent, MqttConnectHandle, MqttProxyListener, MqttProxyService};

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
              args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle<S>) };
        match args {
            Args::OneArgs(MqttEvent::Connect(socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd)) => {
                //处理Mqtt连接
                println!("!!!!!!Connect, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, keep_alive: {:?}, is_clean_session: {:?}, user: {:?}, pwd: {:?}", socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd);
                thread::spawn(move || {
                    while !connect.wakeup(Ok(())) {
                        thread::sleep(Duration::from_millis(1));
                    }
                });
            }
            Args::OneArgs(MqttEvent::Disconnect(socket_id, broker_name, client_id, reason)) => {
                //处理Mqtt连接关闭
                println!("!!!!!!Disconnect, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, reason: {:?}", socket_id, broker_name, client_id, reason);
            },
            _ => {
                println!("!!!!!!Invalid mqtt event");
            },
        }
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

    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle<S>) };
        match args {
            Args::OneArgs(MqttEvent::Sub(socket_id, broker_name, client_id, topics)) => {
                //处理Mqtt订阅主题
                println!("!!!!!!Sub, socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}", socket_id, broker_name, client_id, topics);

                for (topic, _) in topics {
                    connect.sub(topic);
                }

                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(1000));
                    while !connect.wakeup(Ok(())) {
                        thread::sleep(Duration::from_millis(1));
                    }
                });
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
                connect.send(&"rpc/send".to_string(), address.unwrap().to_string().into_bytes());
                if let Ok(bin) = Arc::try_unwrap(payload) {
                    connect.reply(bin);
                }
            },
            _ => {
                println!("!!!!!!Invalid mqtt event");
            },
        }
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
                               Some(10)) {
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

    let rt = AsyncRuntimeBuilder::default_worker_thread(None,
                                                        None,
                                                        None,
                                                        None);

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
                               Some(10)) {
        Err(e) => {
            println!("!!!> Rpc Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Rpc Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}
