use std::thread;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, LocalBoxFuture};
use httparse::{EMPTY_HEADER, Request};
use bytes::{Buf, BufMut, BytesMut};
use env_logger;

use pi_async::rt::{serial::{AsyncRuntimeBuilder, AsyncValue}};

use tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus, SocketEvent,
          connect::TcpSocket,
          tls_connect::TlsSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::{TlsConfig, Ready}};
use ws::{server::WebsocketListener,
         connect::WsSocket,
         frame::WsHead,
         utils::{ChildProtocol, WsSession}};

#[test]
fn test_parse_http_header() {
    let part0 = b"GET /index.html HTTP/1.1";
    let part1 = b"\r\nHost: example.domain\r\n\r\n";

    let mut bytes = BytesMut::new();
    bytes.put_slice(part0);
    bytes.put_slice(part1);
    let mut headers = [EMPTY_HEADER; 16];
    let mut req = Request::new(&mut headers);
    if let Ok(status) = req.parse(bytes.as_ref()) {
        if status.is_partial() {
            match req.parse(part1) {
                Err(e) => panic!(e),
                Ok(status) => {
                    println!("{}", status.is_partial());
                },
            }
        } else {
            println!("!!!!!!{:?}", req);
        }
    }
}

struct TestChildProtocol;

impl<S: Socket> ChildProtocol<S> for TestChildProtocol {
    fn protocol_name(&self) -> &str {
        "echo"
    }

    fn decode_protocol(&self, connect: WsSocket<S>, context: &mut WsSession) -> LocalBoxFuture<'static, Result<()>> {
        let msg = context.pop_msg();
        let msg_type = context.get_type();
        println!("!!!!!!receive ok, msg: {:?}", String::from_utf8(msg.clone()));

        async move {
            if let Some(hibernate) = connect.hibernate(Ready::Writable) {
                let connect_copy = connect.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(1000));
                    while !connect_copy.wakeup(Ok(())) {
                        //唤醒被阻塞，则休眠指定时间后继续尝试唤醒
                        thread::sleep(Duration::from_millis(15));
                    }
                });
                let start = Instant::now();
                if let Err(e) = hibernate.await {
                    //唤醒后返回错误，则立即返回错误原因
                    return Err(e);
                }
                println!("!!!!!!wakeup hibernate ok, time: {:?}", start.elapsed());
            }

            for _ in 0..3 {
                if let Err(e) = connect.send(msg_type.clone(), msg.clone()) {
                    return Err(e);
                }
            }

            println!("reply msg ok");
            Ok(())
        }.boxed_local()
    }

    fn close_protocol(&self, connect: WsSocket<S>, context: WsSession, reason: Result<()>) {
        if let Err(e) = reason {
            return println!("websocket closed, reason: {:?}", e);
        }

        println!("websocket closed");
    }

    fn protocol_timeout(&self, connect: WsSocket<S>, context: &mut WsSession, event: SocketEvent) -> Result<()> {
        println!("websocket timeout");

        Ok(())
    }
}

#[test]
fn test_websocket_listener() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListener::with_protocol(Arc::new(TestChildProtocol))));
    let mut config = SocketConfig::new("0.0.0.0", factory.ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(10)) {
        Err(e) => {
            println!("!!!> Websocket Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Websocket Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_tls_websocket_listener() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<TlsSocket>::new();
    factory.bind(38080,
                 Box::new(WebsocketListener::with_protocol(Arc::new(TestChildProtocol))));
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./tests/7285407__17youx.cn.pem",
                                           "./tests/7285407__17youx.cn.key",
                                           "",
                                           "",
                                           "",
                                           512,
                                           false,
                                           "").unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(38080, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               4096,
                               4096,
                               Some(10)) {
        Err(e) => {
            println!("!!!> Websocket Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Websocket Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}