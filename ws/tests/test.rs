use std::thread;
use std::time::Duration;
use std::marker::PhantomData;
use std::collections::HashMap;

use futures::future::{FutureExt, BoxFuture};

use tcp::connect::TcpSocket;
use tcp::server::{AsyncWaitsHandle, AsyncAdapter, PortsAdapter, SocketListener};
use tcp::driver::{Socket, SocketConfig, SocketAdapterFactory, AsyncServiceName, AsyncService, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;

use ws::server::WebsocketListener;

struct WebsocketListenerFactory<S: Socket>(PhantomData<S>);

impl<S: Socket> AsyncServiceFactory for WebsocketListenerFactory<S> {
    type Connect = S;
    type Waits = AsyncWaitsHandle;
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn new_service(&self) -> Box<dyn AsyncService<Self::Connect, Self::Waits, Out = Self::Out, Future = Self::Future>> {
        Box::new(WebsocketListener::<S, Self::Waits>::default())
    }
}

pub struct TestFactory<S> {
    ports: HashMap<u16, Box<dyn AsyncServiceFactory<Connect = S, Waits = AsyncWaitsHandle, Out = (), Future = BoxFuture<'static, ()>>>>,
}

impl<S: Socket> SocketAdapterFactory for TestFactory<S> {
    type Connect = S;
    type Adapter = PortsAdapter<S>;

    fn instance(&self) -> Self::Adapter {
        let mut ports_adapter = PortsAdapter::<S>::new();
        for (port, factory) in &self.ports {
            let service = factory.new_service();
            let async_adapter = Box::new(AsyncAdapter::<S, ()>::with_service(service));
            ports_adapter.set_adapter(port.clone(), async_adapter);
        }

        ports_adapter
    }
}

impl<S: Socket> TestFactory<S> {
    fn new() -> Self {
        TestFactory {
            ports: HashMap::new(),
        }
    }

    fn set(&mut self, port: u16, factory: Box<dyn AsyncServiceFactory<Connect = S, Waits = AsyncWaitsHandle, Out = (), Future = BoxFuture<'static, ()>>>) {
        self.ports.insert(port, factory);
    }
}

#[test]
fn test_websocket_listener() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    let mut factory = TestFactory::<TcpSocket>::new();
    factory.set(38080, Box::new(WebsocketListenerFactory::<TcpSocket>(PhantomData)));
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