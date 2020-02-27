#![feature(async_await)]

extern crate mio;
extern crate crossbeam_channel;
extern crate tcp;
extern crate fnv;
extern crate futures;

use std::thread;
use std::net::Shutdown;
use std::time::Duration;
use std::any::{Any, TypeId};
use std::marker::PhantomData;
use std::collections::HashMap;

use iovec::{MAX_LENGTH, IoVec};
use futures::future::{FutureExt, BoxFuture};

use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;
use tcp::server::{AsyncWaitsHandle, AsyncAdapter, PortsAdapter, AsyncPortsFactory, SocketListener};
use tcp::driver::{SocketConfig, Socket, AsyncIOWait, SocketAdapterFactory, AsyncService, AsyncServiceFactory, SocketStatus, SocketHandle, AsyncReadTask, AsyncWriteTask};
use tcp::buffer_pool::WriteBufferPool;
use tcp::util::{close_socket, IoBytes, IoList, TlsConfig};
use tcp::driver::SocketConfig::Tls;

struct TestService;

impl<S: Socket, H: AsyncIOWait> AsyncService<S, H> for TestService {
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn handle_connected(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Connected(result) = status {
                let token = handle.get_token().clone();
                if let Err(e) = result {
                    println!("!!!> Connect Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                } else {
                    println!("===> Connect Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());

                    //连接成功，开始读
                    if token.0 % 2 == 0 {
                        //准备异步读
                        if let Err(e) = handle.read_ready(0) {
                            println!("!!!> Read Ready Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                        }
                    } else {
                        //直接异步读
                        let mut buf = handle.alloc().ok().unwrap().unwrap();
                        match AsyncReadTask::async_read(handle.clone(), waits.clone(), 0).await {
                            Err(e) => {
                                println!("!!!> Socket Read Error, token: {:?}, reason: {:?}", token, e);
                            },
                            Ok(bin) => {
                                println!("===> Socket Read Ok, token: {:?}, data: {:?}", token, String::from_utf8_lossy(bin));

                                //读成功，开始写
                                let mut arr = b"HTTP/1.0 200 OK\r\nContent-Length: 35\r\nConnection: close\r\n\r\nHello world from rust web server!\r\n".into();
                                buf.get_iolist_mut().push_back(arr);

                                if let Some(buf) = buf.finish() {
                                    match AsyncWriteTask::async_write(handle, waits, buf).await {
                                        Err(e) => {
                                            println!("!!!> Socket Write Error, token: {:?}, reason: {:?}", token, e);
                                        },
                                        Ok(_) => {
                                            println!("===> Socket Write Ok, token: {:?}", token);
                                        },
                                    }
                                }
                            },
                        }
                    }
                }
            }
        };
        future.boxed()
    }

    fn handle_readed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Readed(result) = status {
                let token = handle.get_token().clone();
                if let Err(e) = result {
                    println!("!!!> Socket Receive Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                } else {
                    println!("===> Socket Receive Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());

                    let mut buf = handle.alloc().ok().unwrap().unwrap();
                    match AsyncReadTask::async_read(handle.clone(), waits.clone(), 0).await {
                        Err(e) => {
                            println!("!!!> Socket Read Error, token: {:?}, reason: {:?}", token, e);
                        },
                        Ok(bin) => {
                            println!("===> Socket Read Ok, token: {:?}, data: {:?}", token, String::from_utf8_lossy(bin));

                            //读成功，开始写
                            let mut arr = IoBytes::from(b"HTTP/1.0 200 OK\r\nContent-Length: 35\r\nConnection: close\r\n\r\nHello world from rust web server!\r\n");
                            buf.get_iolist_mut().push_back(arr);

                            if let Some(buf) = buf.finish() {
                                match AsyncWriteTask::async_write(handle, waits, buf).await {
                                    Err(e) => {
                                        println!("!!!> Socket Write Error, token: {:?}, reason: {:?}", token, e);
                                    },
                                    Ok(_) => {
                                        println!("===> Socket Write Ok, token: {:?}", token);
                                    },
                                }
                            }
                        },
                    }
                }
            }
        };
        future.boxed()
    }

    fn handle_writed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Writed(result) = status {
                let token = handle.get_token().clone();
                if let Err(e) = result {
                    println!("!!!> Socket Send Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                } else {
                    println!("===> Socket Send Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());

                    //发送成功，则关闭
                    if let Err(e) = handle.close(Ok(())) {
                        println!("!!!> Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                    }
                }
            }
        };
        future.boxed()
    }

    fn handle_closed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Closed(result) = status {
                let token = handle.get_token().clone();
                if let Err(e) = result {
                    println!("!!!> Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.get_remote(), handle.get_local(), e);
                } else {
                    println!("===> Socket Close Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());
                }
            }
        };
        future.boxed()
    }

    fn handle_timeouted(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            let token = handle.get_token().clone();
            println!("!!!> Socket Timeout, token: {:?}, remote: {:?}, local: {:?}", token, handle.get_remote(), handle.get_local());
        };
        future.boxed()
    }
}

struct TestServiceFactory<S: Socket>(PhantomData<S>);

impl<S: Socket> AsyncServiceFactory for TestServiceFactory<S> {
    type Connect = S;
    type Waits = AsyncWaitsHandle;
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn new_service(&self) -> Box<dyn AsyncService<Self::Connect, Self::Waits, Out = Self::Out, Future = Self::Future>> {
        Box::new(TestService)
    }
}

#[test]
fn test_socket_server() {
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080, Box::new(TestServiceFactory::<TcpSocket>(PhantomData)));
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

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

#[test]
fn test_socket_server_ipv6() {
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080, Box::new(TestServiceFactory::<TcpSocket>(PhantomData)));
    let mut config = SocketConfig::new("fe80::c0bc:ecf0:e91:2b3a", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv6 Address Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Socket Listener Bind Ipv6 Address Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_socket_server_shared() {
    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(38080, Box::new(TestServiceFactory::<TcpSocket>(PhantomData)));
    let mut config = SocketConfig::new("::", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
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
fn test_tls_socket_server_shared() {
    let mut factory = AsyncPortsFactory::<TlsSocket>::new();
    factory.bind(38080, Box::new(TestServiceFactory::<TlsSocket>(PhantomData)));
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
    let mut config = SocketConfig::with_tls("::", &[(38080, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
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
fn test_io_list() {
    let arr = IoBytes::from(vec![10, 10, 10]);
    let mut iolist = IoList::with_capacity(10);
    iolist.push_back(arr);
    let vec = Vec::from(iolist);
    let values = vec.iter().map(|arr| {
        arr.as_ref().into()
    }).collect::<Vec<&IoVec>>();
    let iovec = values.as_slice();
    println!("iovec max length: {}", MAX_LENGTH);
}
