use std::thread;
use std::any::Any;
use std::sync::Arc;
use std::io::Result;
use std::mem::transmute;
use std::time::Duration;
use std::net::SocketAddr;

use handler::{Args, Handler};

use tcp::connect::TcpSocket;
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;
use ws::server::WebsocketListenerFactory;
use mqtt::v311::{WS_MQTT3_BROKER, WsMqtt311, WsMqtt311Factory};

use base::{service::{BaseListener, BaseService}, connect::{BaseInnerListener, BaseInnerService, BaseConnect}};

struct TestExtProtocol;

impl BaseInnerListener for TestExtProtocol {
    fn connected(&self, connect: &mut BaseConnect) -> Result<()> {
        println!("!!!!!!base connected");
        Ok(())
    }

    fn closed(&self, connect: &mut BaseConnect, reason: Result<()>) {
        println!("!!!!!!base closed, reason: {:?}", reason);
    }
}

impl BaseInnerService for TestExtProtocol {
    fn request(&self, connect: &BaseConnect, topic: String, payload: Arc<Vec<u8>>) -> Result<()> {
        println!("!!!!!!request, compress level: {}, compare: {}, version: {}", connect.get_compress_level(), connect.is_compare(), connect.get_version());
        Ok(())
    }
}

#[test]
fn test_base_service() {
    let ext_protocol = Arc::new(TestExtProtocol);
    let listener = Arc::new(BaseListener::with_listener(ext_protocol.clone()));
    let service = Arc::new(BaseService::with_service(ext_protocol));
    WS_MQTT3_BROKER.register_listener(listener);
    WS_MQTT3_BROKER.register_service("rpc/test".to_string(), service);

    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     Arc::new(WsMqtt311Factory::with_name("mqttv3.1")))));
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 2 * 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Base Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Base Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}