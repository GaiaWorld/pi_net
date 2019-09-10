use std::thread;
use std::sync::Arc;
use std::time::Duration;
use std::io::{ErrorKind, Result, Error};

use tcp::connect::TcpSocket;
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;

use ws::{server::WebsocketListenerFactory,
         connect::WsSocket,
         frame::WsHead,
         util::{ChildProtocol, ChildProtocolFactory, WsContext}};

use mqtt::impls::{WsMqttProtocol, WsMqttProtocolFactory};

#[test]
fn test_mqtt_311() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     Arc::new(WsMqttProtocolFactory::with_name("mqttv3.1")))));
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