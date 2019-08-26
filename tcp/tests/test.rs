#![feature(async_await)]

extern crate mio;
extern crate crossbeam_channel;
extern crate tcp;
extern crate fnv;
extern crate futures;

use std::thread;
use std::net::Shutdown;
use std::time::Duration;

use iovec::{MAX_LENGTH, IoVec};
use futures::future::{FutureExt, BoxFuture};

use tcp::connect::TcpSocket;
use tcp::server::{AsyncAdapter, PortsAdapter, PortsAdapterFactory, SocketListener};
use tcp::driver::{SocketConfig, Socket, AsyncIOWait, AsyncService, SocketStatus, SocketHandle, AsyncReadTask, AsyncWriteTask};
use tcp::buffer_pool::WriteBufferPool;
use tcp::util::{IoArr, IoList};

struct TestService;

impl<S: Socket, H: AsyncIOWait> AsyncService<S, H> for TestService {
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn handle_connected(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Connected(result) = status {
                let token = handle.as_handle().unwrap().as_ref().borrow().get_token().unwrap().clone();
                if let Err(e) = result {
                    println!("!!!> Connect Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.as_handle().unwrap().as_ref().borrow().get_remote(), handle.as_handle().unwrap().as_ref().borrow().get_local(), e);
                } else {
                    println!("===> Connect Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.as_handle().unwrap().as_ref().borrow().get_remote(), handle.as_handle().unwrap().as_ref().borrow().get_local());

                    //连接成功，开始读
                    if token.0 % 2 == 0 {
                        //准备异步读
                        if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(0) {
                            println!("!!!> Read Ready Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.as_handle().unwrap().as_ref().borrow().get_remote(), handle.as_handle().unwrap().as_ref().borrow().get_local(), e);
                        }
                    } else {
                        //直接异步读
                        let mut buf = handle.as_handle().as_ref().unwrap().borrow().get_write_buffer().alloc().ok().unwrap().unwrap();
                        match AsyncReadTask::async_read(handle.clone(), waits.clone(), 0).await {
                            Err(e) => {
                                println!("!!!> Socket Read Error, token: {:?}, reason: {:?}", token, e);
                            },
                            Ok(bin) => {
                                println!("===> Socket Read Ok, token: {:?}, data: {:?}", token, String::from_utf8_lossy(bin));

                                //读成功，开始写
                                let mut arr = IoArr::from(b"HTTP/1.0 200 OK\r\nContent-Length: 35\r\nConnection: close\r\n\r\nHello world from rust web server!\r\n");
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
                let token = handle.as_handle().unwrap().as_ref().borrow().get_token().unwrap().clone();
                if let Err(e) = result {
                    println!("!!!> Socket Receive Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, handle.as_handle().unwrap().as_ref().borrow().get_remote(), handle.as_handle().unwrap().as_ref().borrow().get_local(), e);
                } else {
                    println!("===> Socket Receive Ok, token: {:?}, remote: {:?}, local: {:?}", token, handle.as_handle().unwrap().as_ref().borrow().get_remote(), handle.as_handle().unwrap().as_ref().borrow().get_local());

                    let mut buf = handle.as_handle().as_ref().unwrap().borrow().get_write_buffer().alloc().ok().unwrap().unwrap();
                    match AsyncReadTask::async_read(handle.clone(), waits.clone(), 0).await {
                        Err(e) => {
                            println!("!!!> Socket Read Error, token: {:?}, reason: {:?}", token, e);
                        },
                        Ok(bin) => {
                            println!("===> Socket Read Ok, token: {:?}, data: {:?}", token, String::from_utf8_lossy(bin));

                            //读成功，开始写
                            let mut arr = IoArr::from(b"HTTP/1.0 200 OK\r\nContent-Length: 35\r\nConnection: close\r\n\r\nHello world from rust web server!\r\n");
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
                if let Some(socket) = handle.as_handle() {
                    let token = socket.as_ref().borrow_mut().get_token().unwrap().clone();
                    if let Err(e) = result {
                        println!("!!!> Socket Send Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.as_ref().borrow().get_remote(), socket.as_ref().borrow().get_local(), e);
                    } else {
                        println!("===> Socket Send Ok, token: {:?}, remote: {:?}, local: {:?}", token, socket.as_ref().borrow().get_remote(), socket.as_ref().borrow().get_local());

                        //发送成功，则关闭
                        if let Err(e) = socket.as_ref().borrow().close(Shutdown::Both) {
                            println!("!!!> Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.as_ref().borrow().get_remote(), socket.as_ref().borrow().get_local(), e);
                        }
                    }
                }
            }
        };
        future.boxed()
    }

    fn handle_closed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Closed(result) = status {
                if let Some(socket) = handle.as_handle() {
                    let token = socket.as_ref().borrow_mut().get_token().unwrap().clone();
                    if let Err(e) = result {
                        println!("!!!> Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.as_ref().borrow().get_remote(), socket.as_ref().borrow().get_local(), e);
                    } else {
                        println!("===> Socket Close Ok, token: {:?}, remote: {:?}, local: {:?}", token, socket.as_ref().borrow().get_remote(), socket.as_ref().borrow().get_local());
                    }
                }
            }
        };
        future.boxed()
    }
}

struct TestFactory;

impl PortsAdapterFactory for TestFactory {
    type Connect = TcpSocket;

    fn instance(&self) -> PortsAdapter<Self::Connect> {
        let mut adapter = PortsAdapter::<TcpSocket>::new();
        adapter.set_adapter(38080, Box::new(AsyncAdapter::<TcpSocket, TestService>::with_service(TestService)));
        adapter
    }
}

#[test]
fn test_socket_server() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    match SocketListener::bind(TestFactory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
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
    let config = SocketConfig::new("fe80::c0bc:ecf0:e91:2b3a", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    match SocketListener::bind(TestFactory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
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
    let config = SocketConfig::new("::", &[38080]);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    match SocketListener::bind(TestFactory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
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
    let arr = IoArr::from(vec![10, 10, 10]);
    let mut iolist = IoList::with_capacity(10);
    iolist.push_back(arr);
    let vec = Vec::from(iolist);
    let values = vec.iter().map(|arr| {
        arr.as_ref().into()
    }).collect::<Vec<&IoVec>>();
    let iovec = values.as_slice();
    println!("iovec max length: {}", MAX_LENGTH);
}