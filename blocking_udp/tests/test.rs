use std::thread;
use std::io::Result;
use std::str::FromStr;
use std::time::Duration;
use std::net::{IpAddr, SocketAddr};

use futures::future::{FutureExt, LocalBoxFuture};
use env_logger;

use pi_async_rt::rt::{serial::{AsyncRuntime, AsyncRuntimeBuilder},
                      serial_local_thread::LocalTaskRuntime};

use blocking_udp::{Socket, AsyncService, SocketHandle,
                   terminal::UdpTerminal};

struct TestService(bool);

impl AsyncService for TestService {
    fn bind_runtime(&mut self, _rt: LocalTaskRuntime<()>) {

    }

    fn handle_binded(&self,
                     handle: SocketHandle,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Ok(_) = result {
                println!("===> Binded Ok, uid: {:?}, remote: {:?}, local: {:?}",
                         handle.get_uid(),
                         handle.get_remote(),
                         handle.get_local());
            } else {
                println!("!!!> Binded Failed, reason: {:?}", result);
            }
        }.boxed_local()
    }

    fn handle_received(&self,
                       handle: SocketHandle,
                       result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()> {
        let is_server = self.0;
        async move {
            if is_server {
                //服务端
                if let Ok((bin, peer)) = result {
                    if &bin[..] == &[0] {
                        handle.close(Ok(()));
                        return;
                    }

                    handle.write(bin.clone(), peer).unwrap();
                    handle.write(bin.clone(), peer).unwrap();
                    handle.write(bin.clone(), peer).unwrap();
                    println!("===> Server Receive Ok, uid: {:?}, remote: {:?}, local: {:?}, packet: {:?}",
                             handle.get_uid(),
                             peer,
                             handle.get_local(),
                             String::from_utf8(bin));
                } else {
                    println!("!!!> Server Receive Failed, reason: {:?}", result);
                }
            } else {
                //客户端
                if let Ok((bin, peer)) = result {
                    println!("===> Client Receive Ok, uid: {:?}, remote: {:?}, local: {:?}, packet: {:?}",
                             handle.get_uid(),
                             peer,
                             handle.get_local(),
                             String::from_utf8(bin));
                } else {
                    println!("!!!> Client Receive Failed, reason: {:?}", result);
                }
            }
        }.boxed_local()
    }

    fn handle_sended(&self,
                     handle: SocketHandle,
                     result: Result<usize>) -> LocalBoxFuture<'static, ()> {
        let is_server = self.0;
        async move {
            if is_server {
                //服务端
                if let Ok(_) = result {
                    println!("===> Server Send Ok, uid: {:?}, remote: {:?}, local: {:?}",
                             handle.get_uid(),
                             handle.get_remote(),
                             handle.get_local());
                } else {
                    println!("!!!> Server Send Failed, reason: {:?}", result);
                }
            } else {
                //客户端
                if let Ok(_) = result {
                    println!("===> Client Send Ok, uid: {:?}, remote: {:?}, local: {:?}",
                             handle.get_uid(),
                             handle.get_remote(),
                             handle.get_local());
                } else {
                    println!("!!!> Client Send Failed, reason: {:?}", result);
                }
            }
        }.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        let is_server = self.0;
        async move {
            if is_server {
                //服务端
                if let Ok(_) = result {
                    println!("===> Server Close Ok, uid: {:?}, remote: {:?}, local: {:?}",
                             handle.get_uid(),
                             handle.get_remote(),
                             handle.get_local());
                } else {
                    println!("!!!> Server Close Failed, reason: {:?}", result);
                }
            } else {
                //客户端
                if let Ok(_) = result {
                    println!("===> Client Close Ok, uid: {:?}, remote: {:?}, local: {:?}",
                             handle.get_uid(),
                             handle.get_remote(),
                             handle.get_local());
                } else {
                    println!("!!!> Client Close Failed, reason: {:?}", result);
                }
            }

        }.boxed_local()
    }
}

#[test]
fn test_udp_connect() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

    match UdpTerminal::bind(addrs,
                            rt,
                            8 * 1024 * 1024,
                            8 * 1024 * 1024,
                            0xffff,
                            0xffff,
                            Box::new(TestService(true))) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
        },
        Ok(_) => {
            println!("===> Socket Listener Bind Ipv4 Address Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_udp_connect_by_v6() {
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let addrs = SocketAddr::new(IpAddr::from_str("::1").unwrap(), 38080);

    match UdpTerminal::bind(addrs,
                            rt,
                            8 * 1024 * 1024,
                            8 * 1024 * 1024,
                            0xffff,
                            0xffff,
                            Box::new(TestService(true))) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv6 Address Error, reason: {:?}", e);
        },
        Ok(_) => {
            println!("===> Socket Listener Bind Ipv6 Address Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_udp_multi_client_to_single_server() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let server_addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

    match UdpTerminal::bind(server_addrs,
                            rt,
                            8 * 1024 * 1024,
                            8 * 1024 * 1024,
                            0xffff,
                            0xffff,
                            Box::new(TestService(true))) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
        },
        Ok(_) => {
            println!("===> Socket Listener Bind Ipv4 Address Ok");
        }
    }

    //启动客户端
    let server_addrs = SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), 38080);

    for index in 0..10 {
        let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
        let client_addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38081 + index);
        match UdpTerminal::bind(client_addrs,
                                   rt,
                                   8 * 1024 * 1024,
                                   8 * 1024 * 1024,
                                   0xffff,
                                   0xffff,
                                   Box::new(TestService(false))) {
            Err(e) => {
                println!("Client bind failed, reason: {:?}", e);
            },
            Ok(terminal) => {
                match terminal.connect_to_peer(server_addrs) {
                    Err(e) => {
                        println!("Client connect failed, reason: {:?}", e);
                    },
                    Ok(connection) => {
                        println!("Client connect ok, uid: {:?}, remote: {:?}, local: {:?}",
                                 connection.get_uid(),
                                 connection.get_remote(),
                                 connection.get_local());

                        connection.write(b"Hello World!".to_vec(), Some(server_addrs));
                        thread::sleep(Duration::from_millis(100));

                        let result = connection.close(Ok(()));
                        println!("Client connection close, uid: {:?}, remote: {:?}, local: {:?}, result: {:?}",
                                 connection.get_uid(),
                                 connection.get_remote(),
                                 connection.get_local(),
                                 result);
                    },
                }
            }
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_udp_single_client_to_multi_server() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    for index in 0..10 {
        let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
        let server_addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080 + index);
        match UdpTerminal::bind(server_addrs,
                                rt,
                                8 * 1024 * 1024,
                                8 * 1024 * 1024,
                                0xffff,
                                0xffff,
                                Box::new(TestService(true))) {
            Err(e) => {
                println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
            },
            Ok(_) => {
                println!("===> Socket Listener Bind Ipv4 Address Ok");
            }
        }
    }

    //启动客户端
    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let client_addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 18080);
    match UdpTerminal::bind(client_addrs,
                               rt,
                               8 * 1024 * 1024,
                               8 * 1024 * 1024,
                               0xffff,
                               0xffff,
                               Box::new(TestService(false))) {
        Err(e) => {
            println!("Client bind failed, reason: {:?}", e);
        },
        Ok(terminal) => {
            let mut connections = Vec::with_capacity(10);
            for index in 0..10 {
                let server_addrs = SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), 38080 + index);
                match terminal.connect_to_peer(server_addrs) {
                    Err(e) => {
                        println!("Client connect failed, reason: {:?}", e);
                    },
                    Ok(connection) => {
                        println!("Client connect ok, uid: {:?}, remote: {:?}, local: {:?}",
                                 connection.get_uid(),
                                 server_addrs,
                                 connection.get_local());
                        connections.push(connection);
                    },
                }
            }

            let mut index = 0;
            for connection in connections {
                connection.write(b"Hello World!".to_vec(), Some(SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), 38080 + index)));
                index += 1;
            }
            thread::sleep(Duration::from_millis(1000));

            let result = terminal.close(Ok(()));
            println!("Client connection close, result: {:?}", result);
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}


