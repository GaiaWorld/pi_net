use std::thread;
use std::any::Any;
use std::sync::Arc;
use std::mem::transmute;
use std::time::Duration;
use std::net::SocketAddr;

use atom::Atom;
use gray::GrayVersion;
use handler::{Args, Handler};

use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;
use tcp::util::TlsConfig;
use ws::server::WebsocketListenerFactory;
use mqtt::v311::{WS_MQTT3_BROKER, WsMqtt311Factory};
use mqtt::tls_v311::{WSS_MQTT3_BROKER, WssMqtt311Factory};

use mqtt_proxy::service::{MqttEvent, MqttConnectHandle, MqttProxyListener, MqttProxyService};

struct TestMqttConnectHandler;

impl Handler for TestMqttConnectHandler {
    type A = MqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, env: Arc<dyn GrayVersion>, _: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle) };
        match args {
            Args::OneArgs(MqttEvent::Connect(socket_id, client_id, keep_alive, is_clean_session, user, pwd, result)) => {
                //处理Mqtt连接
                println!("!!!!!!Connect, socket_id: {:?}, client_id: {:?}, keep_alive: {:?}, is_clean_session: {:?}, user: {:?}, pwd: {:?}", socket_id, client_id, keep_alive, is_clean_session, user, pwd);
                result.set(Ok(()));
                connect.wakeup();
            }
            Args::OneArgs(MqttEvent::Disconnect(socket_id, client_id, reason)) => {
                //处理Mqtt连接关闭
                println!("!!!!!!Disconnect, socket_id: {:?}, client_id: {:?}, reason: {:?}", socket_id, client_id, reason);
            },
            _ => {
                println!("!!!!!!Invalid mqtt event");
            },
        }
    }
}

struct TestMqttRequestHandler;

impl Handler for TestMqttRequestHandler {
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
        let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle) };
        match args {
            Args::OneArgs(MqttEvent::Sub(socket_id, client_id, topics, result)) => {
                //处理Mqtt订阅主题
                println!("!!!!!!Sub, socket_id: {:?}, client_id: {:?}, topics: {:?}", socket_id, client_id, topics);

                for (topic, _) in topics {
                    connect.sub(topic);
                }

                result.set(Ok(()));
                connect.wakeup();
            },
            Args::OneArgs(MqttEvent::Unsub(socket_id, client_id, topics)) => {
                //处理Mqtt退订主题
                println!("!!!!!!Unsub, socket_id: {:?}, client_id: {:?}, topics: {:?}", socket_id, client_id, topics);

                for topic in topics {
                    connect.unsub(topic);
                }
            },
            Args::OneArgs(MqttEvent::Publish(socket_id, client_id, address, topic, payload)) => {
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

#[test]
fn test_mqtt_proxy_service() {
    let event_handler = Arc::new(TestMqttConnectHandler);
    let rpc_handler = Arc::new(TestMqttRequestHandler);
    let listener = Arc::new(MqttProxyListener::with_handler(Some(event_handler)));
    let service = Arc::new(MqttProxyService::with_handler(Some(rpc_handler)));
    WS_MQTT3_BROKER.register_listener(listener);
    WS_MQTT3_BROKER.register_service(service.clone());

    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     Arc::new(WsMqtt311Factory::with_name("mqttv3.1")))));
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 2 * 1024 * 1024, 1024, Some(10)) {
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
    let event_handler = Arc::new(TestMqttConnectHandler);
    let rpc_handler = Arc::new(TestMqttRequestHandler);
    let listener = Arc::new(MqttProxyListener::with_handler(Some(event_handler)));
    let service = Arc::new(MqttProxyService::with_handler(Some(rpc_handler)));
    WSS_MQTT3_BROKER.register_listener(listener);
    WSS_MQTT3_BROKER.register_service(service.clone());

    let mut factory = AsyncPortsFactory::<TlsSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TlsSocket>::with_protocol_factory(
                     Arc::new(WssMqtt311Factory::with_name("mqttv3.1")))));
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
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(38080, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 2 * 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Rpc Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Rpc Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}
