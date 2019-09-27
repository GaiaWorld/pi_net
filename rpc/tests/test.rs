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
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;
use ws::server::WebsocketListenerFactory;
use mqtt::{v311::{WS_MQTT3_BROKER, WsMqtt311, WsMqtt311Factory},
           broker::{MQTT_CONNECT_SYS_TOPIC, MQTT_CLOSE_SYS_TOPIC}};

use rpc::{service::RpcService, connect::RpcConnect};

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
    type C = Arc<Vec<u8>>;
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const RpcConnect) };
        if let Args::ThreeArgs(_, address, shared) = args {
            thread::spawn(move || {
                connect.send("rpc/send".to_string(), address.unwrap().to_string().into_bytes());
                if let Ok(bin) = Arc::try_unwrap(shared) {
                    connect.reply(bin);
                }
            });
        }
    }
}

#[test]
fn test_rpc_service() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     Arc::new(WsMqtt311Factory::with_name("mqttv3.1")))));

    let event_handler = Arc::new(TestRpcEventHandler);
    let rpc_handler = Arc::new(TestRpcHandler);
    let service = Arc::new(RpcService::new(event_handler.clone(),rpc_handler, event_handler.clone()));
    WS_MQTT3_BROKER.register_service(MQTT_CONNECT_SYS_TOPIC.clone(), service.clone());
    WS_MQTT3_BROKER.register_service("rpc/test".to_string(), service.clone());
    WS_MQTT3_BROKER.register_service(MQTT_CLOSE_SYS_TOPIC.clone(), service);

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