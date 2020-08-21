use std::thread;
use std::sync::Arc;
use std::str::FromStr;
use std::time::Duration;
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, BoxFuture};
use url::Url;

use r#async::rt::{AsyncRuntime, single_thread::{SingleTaskRunner, SingleTaskRuntime}};
use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;
use tcp::util::{SocketEvent, TlsConfig};
use wss::{server::WebsocketListenerFactory,
          connect::WsSocket,
          frame::WsHead,
          util::{ChildProtocol, ChildProtocolFactory, WsSession}};

use async_wsc::{AsyncWebsocketBuilder, AsyncWebsocket, AsyncWebsocketMessage, AsyncWebsocketCloseCode};

struct TestChildProtocol;

impl<S: Socket, H: AsyncIOWait> ChildProtocol<S, H> for TestChildProtocol {
    fn protocol_name(&self) -> &str {
        "echo"
    }

    fn decode_protocol(&self, connect: WsSocket<S, H>, waits: H, context: &mut WsSession) -> BoxFuture<'static, Result<()>> {
        let bin = context.to_vec();
        let ty = context.get_type();

        async move {
            for _ in 0..3 {
                if let Some(mut buf) = connect.alloc() {
                    buf.get_iolist_mut().push_back(bin.clone().into());
                    if let Err(e) = connect.send(ty.clone(), buf) {
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
fn test_wsc() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    //启动Websocket服务器
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

    //初始化异步运行时
    let runner = SingleTaskRunner::new();
    let rt = runner.startup().unwrap();

    thread::spawn(move || {
        loop {
            runner.run();
            thread::sleep(Duration::from_millis(1));
        }
    });

    //建立连接
    let mut builder = AsyncWebsocketBuilder::new();
    let mut ws =
        builder
        .enable_strict_client_masking(false)
        .enable_strict_server_key(false)
        .set_queue_size(32)
        .set_frame_buffer_size(32)
        .enable_expansion_frame_buffer(true)
        .set_send_frame_limit(64 * 1024)
        .set_receive_frame_limit(1024 * 1024 * 1024)
        .set_receive_buffer_size(8 * 1024)
        .enable_expansion_receive_buffer(true)
        .set_send_buffer_size(8 * 1024)
        .enable_expansion_send_buffer(true)
        .enable_nodelay(false)
        .build().unwrap();

    //设置Websocket协议，并绑定当前连接的事件处理回调函数
    let handler = ws.get_handler();
    handler.add_protocol("echo");
    handler.set_on_open(Arc::new(move |peer_addr, local_addr| {
        println!("!!!!!!Connect ok, peer_addr: {:?}, local_addr: {:?}", peer_addr, local_addr);
    }));
    handler.set_on_message(Arc::new(move |msg| {
        match msg {
            AsyncWebsocketMessage::Text(text) => {
                println!("!!!!!!Receive ok, msg: {}", text);
            },
            AsyncWebsocketMessage::Binary(bin) => {
                println!("!!!!!!Receive ok, msg: {:?}", bin);
            },
        }
    }));
    handler.set_on_close(Arc::new(move |code, reason| {
        println!("!!!!!!Close start, code: {}, reason: {}", code, reason);
    }));
    handler.set_on_error(Arc::new(move |reason| {
        println!("!!!!!!Error, reason: {}", reason)
    }));

    //开始连接，并获取连接的发送器
    let task_id = rt.alloc();
    ws.set_runtime(AsyncRuntime::Single(rt.clone()));
    ws.set_task_id(task_id.clone());
    let rt_copy = rt.clone();
    rt.spawn(task_id.clone(), async move {
        match ws.open("ws://127.0.0.1:38080", 5000).await {
            Err(e) => {
                println!("!!!!!!Open websocket failed, reason: {:?}", e);
            },
            Ok(sender) => {
                sender.send(AsyncWebsocketMessage::Text("Hello Ws!".to_string())).unwrap();
                rt_copy.wait_timeout(3000).await;
                sender.send(AsyncWebsocketMessage::Text("Hello Ws!".to_string())).unwrap();
                rt_copy.wait_timeout(3000).await;
                sender.close(AsyncWebsocketCloseCode::Normal);
            },
        }
    });

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
fn test_tls_wsc() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

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