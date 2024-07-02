extern crate core;

use core::time;
use std::thread;
use time::Duration;
use bytes::Buf;

use futures::future::{FutureExt, LocalBoxFuture};
use env_logger;

use pi_async_rt::rt::{serial::AsyncRuntimeBuilder};

use pi_tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus,
             connect::TcpSocket,
             tls_connect::TlsSocket,
             server::{PortsAdapterFactory, SocketListener},
             utils::TlsConfig};

#[test]
fn test_accept_connect() {
    use std::net::SocketAddr;
    use mio::{Events, Poll, Interest, Token};
    use mio::net::TcpListener;

    let addr: SocketAddr = "127.0.0.1:38880".parse().unwrap();
    let mut server = TcpListener::bind(addr).unwrap();

    let mut poll = Poll::new().unwrap();
    poll.registry().register(&mut server, Token(0), Interest::READABLE).unwrap();

    let mut events = Events::with_capacity(1024);
    loop {
        poll.poll(&mut events, None).unwrap();

        for event in &events {
            println!("!!!!!!event: {:?}", event);
            let (_, addr) = server.accept().unwrap();
            println!("!!!!!!connected, addr: {:?}", addr);
            poll.registry().reregister(&mut server, Token(0), Interest::READABLE).unwrap();
        }
    }
}

struct TestService;

impl<S: Socket> AsyncService<S> for TestService {
    fn handle_connected(&self,
                        handle: SocketHandle<S>,
                        status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async move {
            let token = handle.get_token().clone();
            match status {
                SocketStatus::Connected(Err(e)) => {
                    println!("!!!> Connect Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                },
                SocketStatus::Connected(Ok(_)) => {
                    println!("===> Connect Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());
                },
                _ => unimplemented!(),
            }
        }.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async move {
            println!("!!!!!!callback readed");
            let token = handle.get_token().clone();
            match status {
                SocketStatus::Readed(Err(e)) => {
                    println!("!!!> Socket Receive Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                },
                SocketStatus::Readed(Ok(_)) => {
                    println!("===> Socket Receive Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());

                    let mut ready_len = 0;
                    if let Some(buf) = unsafe { (&mut *handle.get_read_buffer().get()) } {
                        if buf.remaining() == 0 {
                            //当前读缓冲中没有数据，则异步准备读取数据
                            println!("!!!!!!readed, read ready start, len: 0");
                            ready_len = match handle.read_ready(0) {
                                Err(len) => len,
                                Ok(value) => {
                                    println!("!!!!!!wait read_ready");
                                    let r = value.await;
                                    println!("!!!!!!wakeup read_ready, len: {}", r);
                                    r
                                },
                            };

                            if ready_len == 0 {
                                //当前连接已关闭，则立即退出
                                return;
                            }
                        }
                        println!("===> Socket Read Ok, token: {:?}, data: {:?}", token, String::from_utf8_lossy(buf.as_ref()));

                        //读成功，开始写
                        let mut bin = b"HTTP/1.0 200 OK\r\nContent-Length: 35\r\nConnection: close\r\n\r\nHello world from rust web server!\r\n";
                        if let Ok(_) = handle.write_ready(bin) {
                            println!("===> Socket Write Ok, token: {:?}", token);
                        }
                    } else {
                        return;
                    }
                },
                _ => unimplemented!(),
            }
        }.boxed_local()
    }

    fn handle_writed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async move {
            let token = handle.get_token().clone();
            match status {
                SocketStatus::Writed(Err(e)) => {
                    println!("!!!> Socket Send Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                },
                SocketStatus::Writed(Ok(_)) => {
                    println!("===> Socket Send Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());
                },
                _ => unimplemented!(),
            }
        }.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async move {
            let token = handle.get_token().clone();
            match status {
                SocketStatus::Closed(Err(e)) => {
                    println!("!!!> Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                },
                SocketStatus::Closed(Ok(_)) => {
                    println!("===> Socket Close Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());
                },
                _ => unimplemented!(),
            }
        }.boxed_local()
    }

    fn handle_timeouted(&self,
                        handle: SocketHandle<S>,
                        _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async move {
            let token = handle.get_token().clone();
            println!("!!!> Socket Timeout, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());
        }.boxed_local()
    }
}

#[test]
fn test_tcp_connect() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(38080, Box::new(TestService));
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
                               Some(100)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv4 & Ipv6 Address Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Socket Listener Bind Ipv4 & Ipv6 Address Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_tls_connect() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<TlsSocket>::new();
    factory.bind(38080, Box::new(TestService));
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./tests/17youx.cn.pem",
                                           "./tests/17youx.cn.key",
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
                               Some(100)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv4 & Ipv6 Address Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Socket Listener Bind Ipv4 & Ipv6 Address Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_close_tcp_listener() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(38080, Box::new(TestService));
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
                               Some(100)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv4 & Ipv6 Address Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Socket Listener Bind Ipv4 & Ipv6 Address Ok");

            thread::sleep(Duration::from_millis(10000));

            driver.close(Ok(()));
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}