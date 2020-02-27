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
use base::service::{BaseListener, BaseService};

use rpc::{service::{RpcListener, RpcService}, connect::RpcConnect};

struct TestRpcEventHandler;

impl Handler for TestRpcEventHandler {
    type A = usize; //连接id
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = Result<(), String>;

    fn handle(&self, env: Arc<dyn GrayVersion>, event_name: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const RpcConnect) };
        if let Args::OneArgs(uid) = args {
            println!("!!!!!!rpc connect, uid: {:?}, event: {:?}, peer_addr: {:?}", uid, event_name.to_string(), connect.get_remote_addr());
        }

        Ok(())
    }
}

struct TestRpcHandler;

impl Handler for TestRpcHandler {
    type A = u8;
    type B = Option<SocketAddr>;
    type C = u32;
    type D = Arc<Vec<u8>>;
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const RpcConnect) };
        if let Args::FourArgs(_, address, rid, shared) = args {
            thread::spawn(move || {
                connect.send("rpc/send".to_string(), rid, address.unwrap().to_string().into_bytes());
                if let Ok(bin) = Arc::try_unwrap(shared) {
                    connect.reply(rid, bin);
                }
            });
        }
    }
}

#[test]
fn test_rpc_service() {
    let event_handler = Arc::new(TestRpcEventHandler);
    let rpc_handler = Arc::new(TestRpcHandler);
    let rpc_listener = Arc::new(RpcListener::with_handler(event_handler.clone(), event_handler.clone()));
    let rpc_service = Arc::new(RpcService::with_handler(rpc_handler));
    let listener = Arc::new(BaseListener::with_listener(rpc_listener));
    let service = Arc::new(BaseService::with_service(rpc_service));
    WS_MQTT3_BROKER.register_listener(listener);
    WS_MQTT3_BROKER.register_service("rpc/test".to_string(), service.clone());

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
fn test_tls_rpc_service() {
    let event_handler = Arc::new(TestRpcEventHandler);
    let rpc_handler = Arc::new(TestRpcHandler);
    let rpc_listener = Arc::new(RpcListener::with_handler(event_handler.clone(), event_handler.clone()));
    let rpc_service = Arc::new(RpcService::with_handler(rpc_handler));
    let listener = Arc::new(BaseListener::with_listener(rpc_listener));
    let service = Arc::new(BaseService::with_service(rpc_service));
    WSS_MQTT3_BROKER.register_listener(listener);
    WSS_MQTT3_BROKER.register_service("rpc/test".to_string(), service.clone());

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
