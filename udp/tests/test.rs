use std::thread;
use std::str::FromStr;
use std::time::Duration;
use std::net::{IpAddr, SocketAddr};
use std::io::{Cursor, Result, Error, ErrorKind};

use futures::future::{FutureExt, LocalBoxFuture};
use env_logger;

use pi_async::rt::{serial::{AsyncRuntime, AsyncRuntimeBuilder}};

use udp::{Socket, AsyncService, SocketHandle,
          connect::UdpSocket,
          server::{PortsAdapterFactory, SocketListener}};

struct TestService;

impl<S: Socket> AsyncService<S> for TestService {
    fn handle_binded(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Ok(_) = result {
                println!("===> Binded Ok, token: {:?}, remote: {:?}, local: {:?}",
                         handle.get_token(),
                         handle.get_remote(),
                         handle.get_local());
            }
        }.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Ok((bin, peer)) = result {
                if &bin[..] == &[0] {
                    handle.close(Ok(()));
                    return;
                }

                if handle.get_remote().is_none() {
                    //还未绑定则绑定
                    if let Err(e) = handle.bind(peer) {
                        println!("===> Bind failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                 handle.get_token(),
                                 handle.get_remote(),
                                 handle.get_local(),
                                 e);
                    }
                }

                handle.write_ready(Cursor::new(bin.clone()), None).unwrap();
                handle.write_ready(Cursor::new(bin.clone()), None).unwrap();
                handle.write_ready(Cursor::new(bin.clone()), None).unwrap();
                println!("===> Readed Ok, token: {:?}, remote: {:?}, local: {:?}, packet: {:?}",
                         handle.get_token(),
                         handle.get_remote(),
                         handle.get_local(),
                         String::from_utf8(bin));
            }
        }.boxed_local()
    }

    fn handle_writed(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Ok(_) = result {
                println!("===> Socket Send Ok, token: {:?}, remote: {:?}, local: {:?}",
                         handle.get_token(),
                         handle.get_remote(),
                         handle.get_local());
            }
        }.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Ok(_) = result {
                println!("===> Socket Close Ok, token: {:?}, remote: {:?}, local: {:?}",
                         handle.get_token(),
                         handle.get_remote(),
                         handle.get_local());
            }
        }.boxed_local()
    }
}

#[test]
fn test_udp_connect() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<UdpSocket>::new();
    factory.bind(38080, Box::new(TestService));
    let addrs = vec![SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080)];

    match SocketListener::bind(vec![rt],
                               factory,
                               addrs,
                               1024,
                               1024,
                               0xffff,
                               0xffff,
                               Some(10)) {
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

    let mut factory = PortsAdapterFactory::<UdpSocket>::new();
    factory.bind(38080, Box::new(TestService));
    let addrs = vec![SocketAddr::new(IpAddr::from_str("::1").unwrap(), 38080)];

    match SocketListener::bind(vec![rt],
                               factory,
                               addrs,
                               1024,
                               1024,
                               0xffff,
                               0xffff,
                               Some(10)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv6 Address Error, reason: {:?}", e);
        },
        Ok(_) => {
            println!("===> Socket Listener Bind Ipv6 Address Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}



