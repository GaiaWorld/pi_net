use std::thread;
use std::sync::Arc;
use std::time::Duration;
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, BoxFuture};

use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;
use tcp::util::{SocketEvent, TlsConfig};

use ws::{server::WebsocketListenerFactory,
         connect::WsSocket,
         frame::WsHead,
         util::{ChildProtocol, ChildProtocolFactory, WsSession}};

struct TestChildProtocol;

impl<S: Socket, H: AsyncIOWait> ChildProtocol<S, H> for TestChildProtocol {
    fn protocol_name(&self) -> &str {
        "echo"
    }

    fn decode_protocol(&self, connect: WsSocket<S, H>, waits: H, context: &mut WsSession) -> BoxFuture<'static, Result<()>> {
        async move {
            for _ in 0..3 {
                if let Some(mut buf) = connect.alloc() {
                    buf.get_iolist_mut().push_back(context.to_vec().into());
                    if let Err(e) = connect.send(context.get_type(), buf) {
                        return Err(e);
                    }
                } else {
                    return Err(Error::new(ErrorKind::Other, "test mqtt response failed, reason: alloc write buffer failed"));
                }
            }

            Ok(())
        }.boxed()
    }

    fn close_protocol(&self, connect: WsSocket<S, H>, context: WsSession, reason: Result<()>) {
        if let Err(e) = reason {
            return println!("websocket closed, reason: {:?}", e);
        }

        println!("websocket closed");
    }

    fn protocol_timeout(&self, connect: WsSocket<S, H>, context: &mut WsSession, event: SocketEvent) -> Result<()> {
        println!("websocket timeout");

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
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TcpSocket>::with_protocol_factory(
                     Arc::new(TestChildProtocolFactory))));
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

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

struct TestTlsChildProtocolFactory;

impl ChildProtocolFactory for TestTlsChildProtocolFactory {
    type Connect = TlsSocket;
    type Waits = AsyncWaitsHandle;

    fn new_protocol(&self) -> Arc<dyn ChildProtocol<Self::Connect, Self::Waits>> {
        Arc::new(TestChildProtocol)
    }
}

#[test]
fn test_tls_websocket_listener() {
    let mut factory = AsyncPortsFactory::<TlsSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListenerFactory::<TlsSocket>::with_protocol_factory(
                     Arc::new(TestTlsChildProtocolFactory))));
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