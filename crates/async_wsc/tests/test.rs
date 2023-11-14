use std::thread;
use std::rc::Rc;
use std::sync::Arc;
use std::cell::RefCell;
use std::time::Duration;
use std::sync::atomic::AtomicUsize;
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, BoxFuture};
use futures_util::{sink::SinkExt, stream::StreamExt};
use actix_rt::System;
use actix_codec::Framed;
use actix_http::ws::Codec;
use awc::{Client, BoxedSocket, ws};
use crossbeam_channel::unbounded;
use bytes::Bytes;
use bytestring::ByteString;
use futures_util::future::LocalBoxFuture;

use pi_async_rt::rt::{AsyncRuntime,
                      serial::AsyncRuntimeBuilder,
                      multi_thread::{StealableTaskPool, MultiTaskRuntimeBuilder, MultiTaskRuntime}};
use tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus, SocketEvent,
          connect::TcpSocket,
          tls_connect::TlsSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::{TlsConfig, Ready}};
use wss::{server::WebsocketListener,
          connect::WsSocket,
          utils::{ChildProtocol, WsSession}};

use pi_async_wsc::{AsyncWebsocketClient, AsyncWebsocket, AsyncWebsocketHandler, AsyncWebsocketMessage, AsyncWebsocketCloseCode};

struct TestChildProtocol;

impl<S: Socket> ChildProtocol<S> for TestChildProtocol {
    fn protocol_name(&self) -> &str {
        "echo"
    }

    fn decode_protocol(&self,
                       connect: WsSocket<S>,
                       context: &mut WsSession) -> LocalBoxFuture<'static, Result<()>> {
        let msg = context.pop_msg();
        let msg_type = context.get_type();
        println!("!!!!!!receive ok, msg: {:?}", String::from_utf8(msg.clone()));

        async move {
            for _ in 0..3 {
                if let Err(e) = connect.send(msg_type.clone(), msg.clone()) {
                    return Err(e);
                }
            }

            Ok(())
        }.boxed_local()
    }

    fn close_protocol(&self,
                      connect: WsSocket<S>,
                      context: WsSession,
                      reason: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = reason {
                return println!("websocket closed, reason: {:?}", e);
            }

            println!("websocket closed");
        }.boxed_local()
    }

    fn protocol_timeout(&self,
                        connect: WsSocket<S>,
                        context: &mut WsSession,
                        event: SocketEvent) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            println!("websocket timeout");

            Ok(())
        }.boxed_local()
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

    let rt0 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt1 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt2 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt3 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt4 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt5 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt6 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt7 = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListener::with_protocol(Arc::new(TestChildProtocol))));
    let mut config = SocketConfig::new("0.0.0.0", factory.ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt0, rt1, rt2, rt3, rt4, rt5, rt6, rt7],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(1000)) {
        Err(e) => {
            println!("!!!> Websocket Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Websocket Listener Bind Ok");
        }
    }

    let (sender, receiver) = unbounded();
    thread::spawn(move || {
        let mut runner = System::new();
        runner.block_on(async move {
            loop {
                match receiver.recv() {
                    Err(e) => panic!("Receive websocket request failed, reason: {:?}", e),
                    Ok(req) => {
                        match req {
                            0 => {
                                //连接
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
                            },
                            1 => {
                                //发送消息
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
                            },
                            2 => {
                                //接收消息
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
                            },
                            _ => {
                                //关闭连接
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

                                //关闭运行时
                                break;
                            },
                        }
                    },
                }
            }
        });
    });

    sender.send(0);

    thread::sleep(Duration::from_millis(1000));

    sender.send(1);

    thread::sleep(Duration::from_millis(1000));

    sender.send(2);

    thread::sleep(Duration::from_millis(1000));

    sender.send(3);

    thread::sleep(Duration::from_millis(10000000000));
}

#[test]
fn test_wsc() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt0 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt1 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt2 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt3 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt4 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt5 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt6 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt7 = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListener::with_protocol(Arc::new(TestChildProtocol))));
    let mut config = SocketConfig::new("0.0.0.0", factory.ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt0, rt1, rt2, rt3, rt4, rt5, rt6, rt7],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(1000)) {
        Err(e) => {
            println!("!!!> Websocket Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Websocket Listener Bind Ok");
        }
    }

    //初始化异步运行时
    let builder = MultiTaskRuntimeBuilder::default();
    let rt = builder.build();

    //创建客户端
    let client = AsyncWebsocketClient::new(rt.clone(), "test-wsc-runtime".to_string(), 32).unwrap();

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
    let task_id = rt.alloc::<()>();
    ws.set_task_id(task_id.clone());
    ws.set_send_frame_limit(127);
    rt.spawn(async move {
        println!("!!!!!!Websocket status: {:?}", ws.get_status());
        match ws.open(true, 5000).await {
            Err(e) => {
                println!("!!!!!!Test open websocket failed, reason: {:?}", e);
            },
            Ok(_) => {
                println!("!!!!!!Websocket status: {:?}", ws.get_status());
                receive(rt_copy.clone(), ws.clone());

                for index in 0..10 {
                    println!("!!!!!!index: {}", index);
                    ws.send(AsyncWebsocketMessage::Text(ByteString::from("Hello Ws!".to_string()))).await;
                    ws.send(AsyncWebsocketMessage::Binary(Bytes::from("Hello Ws!"))).await;
                    ws.send(AsyncWebsocketMessage::Binary(Bytes::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))).await;
                    rt_copy.timeout(1000).await;
                }
                ws.close(AsyncWebsocketCloseCode::Normal).await;
            },
        }
    });

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_tls_wsc() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    // let rt0 = AsyncRuntimeBuilder::default_local_thread(None, None);
    // let rt1 = AsyncRuntimeBuilder::default_local_thread(None, None);
    // let rt2 = AsyncRuntimeBuilder::default_local_thread(None, None);
    // let rt3 = AsyncRuntimeBuilder::default_local_thread(None, None);
    // let rt4 = AsyncRuntimeBuilder::default_local_thread(None, None);
    // let rt5 = AsyncRuntimeBuilder::default_local_thread(None, None);
    // let rt6 = AsyncRuntimeBuilder::default_local_thread(None, None);
    // let rt7 = AsyncRuntimeBuilder::default_local_thread(None, None);

    // let mut factory = PortsAdapterFactory::<TlsSocket>::new();
    // factory.bind(38080,
    //              Box::new(WebsocketListener::with_protocol(Arc::new(TestChildProtocol))));
    // let tls_config = TlsConfig::new_server("",
    //                                        false,
    //                                        "./tests/7285407__17youx.cn.pem",
    //                                        "./tests/7285407__17youx.cn.key",
    //                                        "",
    //                                        "",
    //                                        "",
    //                                        512,
    //                                        false,
    //                                        "").unwrap();
    // let mut config = SocketConfig::with_tls("0.0.0.0", &[(38080, tls_config)]);
    // config.set_option(16384, 16384, 16384, 16);
    //
    // match SocketListener::bind(vec![rt0, rt1, rt2, rt3, rt4, rt5, rt6, rt7],
    //                            factory,
    //                            config,
    //                            1024,
    //                            1024 * 1024,
    //                            1024,
    //                            16,
    //                            4096,
    //                            4096,
    //                            Some(1000)) {
    //     Err(e) => {
    //         println!("!!!> Websocket Listener Bind Error, reason: {:?}", e);
    //     },
    //     Ok(driver) => {
    //         println!("===> Websocket Listener Bind Ok");
    //     }
    // }

    //初始化异步运行时
    let builder = MultiTaskRuntimeBuilder::default();
    let rt = builder.build();

    //创建客户端
    let client = AsyncWebsocketClient::new(rt.clone(), "test-tls-wsc-runtime".to_string(), 32).unwrap();

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
    // let ws = client.build("wss://test.17youx.cn:38080", vec!["mqttv3.1".to_string()], handler).unwrap();
    let ws = client.build("wss://boomwss.17youx.cn:20201/mqtt", vec!["mqttv3.1".to_string()], handler).unwrap();
    // let ws = client.build("ws://16.163.41.96:1234/mqtt", vec!["mqttv3.1".to_string()], handler).unwrap();
    // let ws = client.build("wss://meliws.17youx.cn:443", vec!["mqttv3.1".to_string()], handler).unwrap();

    //开始连接，并获取连接的发送器
    let mut err_count = Arc::new(AtomicUsize::new(0));
    let mut ok_count = Arc::new(AtomicUsize::new(0));

    for _ in 0..10 {
        let rt_copy = rt.clone();
        let task_id = rt.alloc::<()>();
        let ws_copy = ws.clone();
        ws_copy.set_task_id(task_id.clone());
        ws_copy.set_send_frame_limit(127);
        let err_count_copy = err_count.clone();
        let ok_count_copy = ok_count.clone();

        let (sender, receiver) = std::sync::mpsc::channel();
        rt.spawn(async move {
            println!("!!!!!!Websocket status: {:?}", ws_copy.get_status());
            let now = std::time::Instant::now();
            match ws_copy.open(true, 30000).await {
                Err(e) => {
                    err_count_copy.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("!!!!!!Test open websocket failed, time: {:?}, reason: {:?}", now.elapsed(), e);
                },
                Ok(_) => {
                    ok_count_copy.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("!!!!!!Websocket status: {:?}, time: {:?}", ws_copy.get_status(), now.elapsed());
                    receive(rt_copy.clone(), ws_copy.clone());

                    // for _ in 0..10 {
                    //     ws_copy.send(AsyncWebsocketMessage::Text(ByteString::from("Hello Ws!".to_string()))).await;
                    //     ws_copy.send(AsyncWebsocketMessage::Binary(Bytes::from("Hello Ws!"))).await;
                    //     ws_copy.send(AsyncWebsocketMessage::Binary(Bytes::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))).await;
                    //     rt_copy.wait_timeout(1000).await;
                    // }
                    ws_copy.close(AsyncWebsocketCloseCode::Normal).await;
                },
            }

            sender.send(());
        });

        receiver.recv().is_ok();
    }

    println!("!!!!!!test finish, ok: {}, error: {}", ok_count.load(std::sync::atomic::Ordering::Relaxed), err_count.load(std::sync::atomic::Ordering::Relaxed));

    thread::sleep(Duration::from_millis(10000000));
}

fn receive(rt: MultiTaskRuntime<()>, ws: AsyncWebsocket<StealableTaskPool<()>, MultiTaskRuntime<()>>) {
    let rt_copy = rt.clone();
    rt.spawn(async move {
        if ws.get_status() > 0 && ws.get_status() < 3 {
            if let Ok(_) = ws.receive_once(None, 1).await {
                // println!("!!!!!!Websocket status: {:?}", ws.get_status());
                receive(rt_copy, ws);
            }
        }
    });
}