use std::thread;
use std::time::Duration;

use tcp::connect::TcpSocket;
use tcp::server::{AsyncPortsFactory, SocketListener};
use tcp::driver::{SocketConfig, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;

use ws::server::WebsocketListenerFactory;

#[test]
fn test_websocket_listener() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080, Box::new(WebsocketListenerFactory::<TcpSocket>::new()));
    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Socket Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}