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

struct TestChildProtocol;

impl<S: Socket, H: AsyncIOWait> ChildProtocol<S, H> for TestChildProtocol {
    fn protocol_name(&self) -> &str {
        "echo"
    }

    fn decode_protocol(&self, connect: WsSocket<S, H>, context: &mut WsContext) -> Result<()> {
        for _ in 0..3 {
            let mut buf = connect.alloc();
            buf.get_iolist_mut().push_back(context.to_vec().into());
            if let Err(e) = connect.send(context.get_type(), buf) {
                return Err(e);
            }
        }

        Ok(())
    }
}

struct TestChildProtocolFactory;

impl ChildProtocolFactory for TestChildProtocolFactory {
    type Connect = TcpSocket;
    type Waits = AsyncWaitsHandle;

    fn new_protocol(&self) -> Arc<dyn ChildProtocol<Self::Connect, Self::Waits>> {
        Arc::new(TestChildProtocol)
    }
}

#[test]
fn test_websocket_listener() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     Arc::new(TestChildProtocolFactory))));
    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Websocket Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Websocket Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}