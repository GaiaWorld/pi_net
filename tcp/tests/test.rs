extern crate mio;
extern crate crossbeam_channel;
extern crate tcp;

use std::thread;
use std::io::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::net::{IpAddr, SocketAddr};

use mio::{Events, Poll, PollOpt, Token, Ready, net::TcpListener};
use crossbeam_channel::{Sender, Receiver, unbounded};

use tcp::driver::{SocketAdapter, SocketConfig, Socket};
use tcp::connect::TcpSocket;
use tcp::server::{PortsAdapter, SocketListener};

//测试用adapter
struct TestSocketAdapter;

impl SocketAdapter for TestSocketAdapter {
    type Connect = TcpSocket;

    fn connected(&self, result: Result<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, reason)) => {
                println!("!!!> Socket Connect Error, remote: {:?}, local: {:?}, reason: {:?}",
                         socket.get_remote(), socket.get_local(), reason);
            },
            Ok(socket) => {
                println!("===> Socket Connect Ok, remote: {:?}, local: {:?}",
                         socket.get_remote(), socket.get_local());

                match socket.read(0) {
                    Err(e) => {
                        println!("!!!> Socket First Read Error, remote: {:?}, local: {:?}, reason: {:?}", socket.get_remote(), socket.get_local(), e);
                    },
                    Ok(Some(bin)) => {
                        println!("===> Socket First Sync Read, data: {:?}", String::from_utf8_lossy(&bin[..]));
                    },
                    Ok(None) => {
                        println!("===> Socket First Async Read Wait, remote: {:?}, local: {:?}", socket.get_remote(), socket.get_local());
                    },
                }
            },
        }
    }

    fn readed(&self, result: Result<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, reason)) => {
                println!("!!!> Socket Async Read Error, remote: {:?}, local: {:?}, reason: {:?}",
                         socket.get_remote(), socket.get_local(), reason);
            },
            Ok(socket) => {
                match socket.read(0) {
                    Err(e) => {
                        println!("!!!> Socket Async Read Error, remote: {:?}, local: {:?}, reason: {:?}", socket.get_remote(), socket.get_local(), e);
                    },
                    Ok(Some(bin)) => {
                        println!("===> Socket Async Read Ok, data: {:?}", String::from_utf8_lossy(&bin[..]));

                        let bin = b"HTTP/1.0 200 OK\r\nContent-Length: 35\r\nConnection: close\r\n\r\nHello world from rust web server!\r\n";
                        match socket.write(bin) {
                            Err(e) => {
                                println!("!!!> Socket Async Write Error, remote: {:?}, local: {:?}, reason: {:?}", socket.get_remote(), socket.get_local(), e);
                            },
                            Ok(_) => {
                                println!("===> Socket Async Write Start, remote: {:?}, local: {:?}", socket.get_remote(), socket.get_local());
                            },
                        }
                    },
                    Ok(None) => {
                        println!("===> Socket Async Read Wait, remote: {:?}, local: {:?}", socket.get_remote(), socket.get_local());
                    },
                }
            },
        }
    }

    fn writed(&self, result: Result<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, reason)) => {
                println!("!!!> Socket Async Write Error, remote: {:?}, local: {:?}, reason: {:?}",
                         socket.get_remote(), socket.get_local(), reason);
            },
            Ok(socket) => {
                println!("===> Socket Async Write Ok, remote: {:?}, local: {:?}",
                         socket.get_remote(), socket.get_local());
            },
        }
    }

    fn closed(&self, result: Result<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, reason)) => {
                println!("!!!> Socket Close Error, remote: {:?}, local: {:?}, reason: {:?}",
                         socket.get_remote(), socket.get_local(), reason);
            },
            Ok(socket) => {
                println!("===> Socket Close Ok, remote: {:?}, local: {:?}",
                         socket.get_remote(), socket.get_local());
            },
        }
    }
}

#[test]
fn test_socket_server() {
    let config = SocketConfig::new("0.0.0.0", &[38080]);
    let mut adapter = PortsAdapter::new();
    adapter.set_adapter(38080, Arc::new(TestSocketAdapter));
    match SocketListener::bind(adapter, config, 1024, 1024 * 1024, 1024, Some(10)) {
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
    let mut adapter = PortsAdapter::new();
    adapter.set_adapter(38080, Arc::new(TestSocketAdapter));
    match SocketListener::bind(adapter, config, 1024, 1024 * 1024, 1024, Some(10)) {
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
    let mut adapter = PortsAdapter::new();
    adapter.set_adapter(38080, Arc::new(TestSocketAdapter));
    match SocketListener::bind(adapter, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv4 & Ipv6 Address Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Socket Listener Bind Ipv4 & Ipv6 Address Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}