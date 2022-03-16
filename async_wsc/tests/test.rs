use std::thread;
use std::rc::Rc;
use std::pin::Pin;
use std::sync::Arc;
use std::str::FromStr;
use std::cell::RefCell;
use std::time::Duration;
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, BoxFuture};
use futures_util::{sink::SinkExt, stream::StreamExt};
use url::Url;
use actix_rt::{System, Arbiter};
use actix_codec::Framed;
use actix_http::ws::Codec;
use awc::{Client, BoxedSocket, ws::{self, CloseReason}};
use crossbeam_channel::unbounded;
use bytes::Bytes;

use pi_async::rt::{AsyncRuntime, single_thread::{SingleTaskRunner, SingleTaskRuntime}};
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

use async_wsc::{AsyncWebsocketClient, AsyncWebsocket, AsyncWebsocketHandler, AsyncWebsocketMessage, AsyncWebsocketCloseCode};

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

/*
* Websocket连接
*/
thread_local! {
    static ASYNC_WEBSOCKET_CONNECTION: Rc<RefCell<Option<Framed<BoxedSocket, Codec>>>> = Rc::new(RefCell::new(None));
}

#[test]
fn test_awc() {
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

    let (send, recv) = unbounded();
    thread::spawn(move || {
        let mut runner = System::new("test_awc");
        println!("Connection thread: {:?}", thread::current().id());
        send.send(Arc::new(System::current().arbiter().clone()));

        runner.run();
    });

    println!("Main thread: {:?}", thread::current().id());

    let rt = recv.recv().unwrap();

    rt.send(Box::pin(async move {
        //连接
        Arbiter::spawn(async move {
            //建立指定url的连接
            match Client::new()
                .ws("ws://127.0.0.1:38080")
                .protocols(&["echo"])
                .connect()
                .await {
                Err(e) => {
                    println!("!!!!!!connect failed, reason: {:?}", e);
                },
                Ok((resp, connection)) => {
                    match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                        let mut last_ws_con = None;
                        if shared.borrow().is_some() {
                            //当前运行时有连接
                            last_ws_con = shared.borrow_mut().take();
                        }

                        //重置当前运行时的连接
                        *shared.borrow_mut() = Some(connection);
                        last_ws_con
                    }) {
                        Err(_) => (),
                        Ok(last_ws_con) => {
                            if let Some(mut last_ws_con) = last_ws_con {
                                //立即关闭旧连接
                                last_ws_con.send(ws::Message::Close(None)).await;
                            }
                            println!("!!!!!!connect ok, resp: {:?}", resp);
                        },
                    }
                }
            }
        });
    }));

    thread::sleep(Duration::from_millis(1000));

    rt.send(Box::pin(async move {
        //发送消息
        Arbiter::spawn(async move {
            match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                shared.clone()
            }) {
                Err(_) => (),
                Ok(shared) => {
                    if let Some(ws_con) = shared.borrow_mut().as_mut() {
                        let r = ws_con.send(ws::Message::Binary(Bytes::copy_from_slice(b"Hello awc!"))).await;
                        if let Err(e) = r {
                            println!("!!!!!!Send failed, reason: {:?}", e);
                        } else {
                            println!("!!!!!!Send ok");
                        }
                    }
                },
            }
        });
    }));

    thread::sleep(Duration::from_millis(1000));

    rt.send(Box::pin(async move {
        //接收消息
        Arbiter::spawn(async move {
            match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                shared.clone()
            }) {
                Err(_) => (),
                Ok(shared) => {
                    if let Some(ws_con) = shared.borrow_mut().as_mut() {
                        let r = ws_con.next().await;
                        if let Some(resp) = r {
                            match resp {
                                Err(e) => {
                                    println!("!!!!!!response failed, reason: {:?}", e);
                                },
                                Ok(frame) => {
                                    println!("!!!!!!response ok, msg: {:?}", frame);
                                },
                            }
                        }
                    }
                },
            }
        });
    }));

    thread::sleep(Duration::from_millis(1000));

    rt.send(Box::pin(async move {
        //关闭连接
        Arbiter::spawn(async move {
            match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                shared.clone()
            }) {
                Err(_) => (),
                Ok(shared) => {
                    if let Some(ws_con) = shared.borrow_mut().as_mut() {
                        ws_con.send(ws::Message::Close(None)).await;
                    }
                },
            }
        });

        //关闭运行时
        System::current().arbiter().stop();
    }));

    thread::sleep(Duration::from_millis(10000000000));
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

    //创建客户端
    let client = AsyncWebsocketClient::new(AsyncRuntime::Local(rt.clone()), "test_wsc".to_string()).unwrap();

    //设置Websocket协议，并绑定当前连接的事件处理回调函数
    let mut handler = AsyncWebsocketHandler::default();
    handler.set_on_open(Arc::new(move || {
        println!("!!!!!!Connect ok");
    }));
    handler.set_on_message(Arc::new(move |msg| {
        match msg {
            AsyncWebsocketMessage::Text(text) => {
                println!("!!!!!!Receive ok, msg: {}", text);
            },
            AsyncWebsocketMessage::Binary(bin) => {
                println!("!!!!!!Receive ok, msg: {:?}", bin);
            },
            _ => (),
        }
    }));
    handler.set_on_close(Arc::new(move |code, reason| {
        println!("!!!!!!Close start, code: {}, reason: {}", code, reason);
    }));
    handler.set_on_error(Arc::new(move |reason| {
        println!("!!!!!!Error, reason: {}", reason)
    }));

    //创建连接
    let ws = client.build("ws://127.0.0.1:38080", vec!["echo".to_string()], handler).unwrap();

    //开始连接，并获取连接的发送器
    let rt_copy = rt.clone();
    let task_id = rt.alloc();
    ws.set_task_id(task_id.clone());
    ws.set_send_frame_limit(127);
    rt.spawn(task_id.clone(), async move {
        println!("!!!!!!Websocket status: {:?}", ws.get_status());
        match ws.open(5000).await {
            Err(e) => {
                println!("!!!!!!Test open websocket failed, reason: {:?}", e);
            },
            Ok(_) => {
                println!("!!!!!!Websocket status: {:?}", ws.get_status());
                receive(rt_copy.clone(), ws.clone());

                for _ in 0..10 {
                    ws.send(AsyncWebsocketMessage::Text("Hello Ws!".to_string())).await;
                    ws.send(AsyncWebsocketMessage::Binary(Bytes::from("Hello Ws!"))).await;
                    ws.send(AsyncWebsocketMessage::Binary(Bytes::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))).await;
                    rt_copy.wait_timeout(1000).await;
                }
                ws.close(AsyncWebsocketCloseCode::Normal).await;
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
                                           "./17youx.cn.pem",
                                           "./17youx.cn.key",
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

    //初始化异步运行时
    let runner = SingleTaskRunner::new();
    let rt = runner.startup().unwrap();

    thread::spawn(move || {
        loop {
            runner.run();
            thread::sleep(Duration::from_millis(1));
        }
    });

    //创建客户端
    let client = AsyncWebsocketClient::new(AsyncRuntime::Local(rt.clone()), "test_tls_wsc".to_string()).unwrap();

    //设置Websocket协议，并绑定当前连接的事件处理回调函数
    let mut handler = AsyncWebsocketHandler::default();
    handler.set_on_open(Arc::new(move || {
        println!("!!!!!!Connect ok");
    }));
    handler.set_on_message(Arc::new(move |msg| {
        match msg {
            AsyncWebsocketMessage::Text(text) => {
                println!("!!!!!!Receive ok, msg: {}", text);
            },
            AsyncWebsocketMessage::Binary(bin) => {
                println!("!!!!!!Receive ok, msg: {:?}", bin);
            },
            _ => (),
        }
    }));
    handler.set_on_close(Arc::new(move |code, reason| {
        println!("!!!!!!Close start, code: {}, reason: {}", code, reason);
    }));
    handler.set_on_error(Arc::new(move |reason| {
        println!("!!!!!!Error, reason: {}", reason)
    }));

    //创建连接
    let ws = client.build("wss://test.17youx.cn:38080", vec!["echo".to_string()], handler).unwrap();

    //开始连接，并获取连接的发送器
    let rt_copy = rt.clone();
    let task_id = rt.alloc();
    ws.set_task_id(task_id.clone());
    ws.set_send_frame_limit(127);
    rt.spawn(task_id.clone(), async move {
        println!("!!!!!!Websocket status: {:?}", ws.get_status());
        match ws.open(5000).await {
            Err(e) => {
                println!("!!!!!!Test open websocket failed, reason: {:?}", e);
            },
            Ok(_) => {
                println!("!!!!!!Websocket status: {:?}", ws.get_status());
                receive(rt_copy.clone(), ws.clone());

                for _ in 0..10 {
                    ws.send(AsyncWebsocketMessage::Text("Hello Ws!".to_string())).await;
                    ws.send(AsyncWebsocketMessage::Binary(Bytes::from("Hello Ws!"))).await;
                    ws.send(AsyncWebsocketMessage::Binary(Bytes::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))).await;
                    rt_copy.wait_timeout(1000).await;
                }
                ws.close(AsyncWebsocketCloseCode::Normal).await;
            },
        }
    });

    thread::sleep(Duration::from_millis(10000000));
}

fn receive(rt: SingleTaskRuntime<()>, ws: AsyncWebsocket) {
    let rt_copy = rt.clone();
    rt.spawn(rt.alloc(), async move {
        if ws.get_status() > 0 && ws.get_status() < 3 {
            if let Ok(_) = ws.receive_once(None).await {
                println!("!!!!!!Websocket status: {:?}", ws.get_status());
                receive(rt_copy, ws);
            }
        }
    });
}