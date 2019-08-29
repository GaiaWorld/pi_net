use std::thread;
use std::time::Duration;
use std::collections::HashMap;

use tcp::connect::TcpSocket;
use tcp::server::{AsyncAdapter, PortsAdapter, SocketListener};
use tcp::driver::{SocketConfig, SocketAdapterFactory, AsyncServiceName};
use tcp::buffer_pool::WriteBufferPool;

use ws::server::WebsocketListener;

pub struct TestFactory {
    ports: HashMap<u16, String>,
}

impl SocketAdapterFactory for TestFactory {
    type Connect = TcpSocket;
    type Adapter = PortsAdapter<TcpSocket>;

    fn instance(&self) -> Self::Adapter {
        let mut adapter = PortsAdapter::<TcpSocket>::new();
        for (port, type_name) in &self.ports {
            match type_name {
                name if name == &WebsocketListener::service_name() => {
                    adapter.set_adapter(port.clone(), Box::new(AsyncAdapter::<TcpSocket, _>::with_service(WebsocketListener::default())));
                },
                _ => (),
            }
        }

        adapter
    }
}

impl TestFactory {
    fn new() -> Self {
        TestFactory {
            ports: HashMap::new(),
        }
    }

    fn set<T: AsyncServiceName>(&mut self, port: u16) {
        self.ports.insert(port, T::service_name());
    }
}

#[test]
fn test_handshake() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    let mut factory = TestFactory::new();
    factory.set::<WebsocketListener>(38080);
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