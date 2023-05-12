use std::future::Future;
use std::thread;
use std::str::FromStr;
use std::time::Duration;
use std::net::{IpAddr, SocketAddr};
use std::io::{Cursor, Result, Error, ErrorKind};

use futures::future::{FutureExt, LocalBoxFuture};
use env_logger;

use pi_async::rt::{serial::{AsyncRuntime, AsyncRuntimeBuilder}, serial_local_thread::LocalTaskRuntime};

use udp::{Socket, AsyncService, SocketHandle,
          connect::UdpSocket,
          server::{PortsAdapterFactory, SocketListener},
          client::{ClientAdapterFactory, UdpClient},
          utils::AsyncServiceContext};
use udp::utils::NotifyUdpClientEvent;

struct TestServerService;

impl<S: Socket> AsyncService<S> for TestServerService {
    fn get_context(&self) -> Option<&AsyncServiceContext<S>> {
        None
    }

    fn set_context(&mut self, _context: AsyncServiceContext<S>) {

    }

    fn bind_runtime(&mut self, _rt: LocalTaskRuntime<()>) {

    }

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

                handle.write_ready(Cursor::new(bin.clone()), peer).unwrap();
                handle.write_ready(Cursor::new(bin.clone()), peer).unwrap();
                handle.write_ready(Cursor::new(bin.clone()), peer).unwrap();
                println!("===> Readed Ok, token: {:?}, remote: {:?}, local: {:?}, packet: {:?}",
                         handle.get_token(),
                         peer,
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
    factory.bind(38080, Box::new(TestServerService));
    let addrs = vec![SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080)];

    match SocketListener::bind(vec![rt],
                               factory,
                               addrs,
                               1024,
                               1024,
                               0xffff,
                               0xffff,
                               Some(1)) {
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
    factory.bind(38080, Box::new(TestServerService));
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

struct TestClientService<S: Socket>(Option<AsyncServiceContext<S>>);

impl<S: Socket> AsyncService<S> for TestClientService<S> {
    fn get_context(&self) -> Option<&AsyncServiceContext<S>> {
        self.0.as_ref()
    }

    fn set_context(&mut self, context: AsyncServiceContext<S>) {
        self.0 = Some(context);
    }

    fn bind_runtime(&mut self, _rt: LocalTaskRuntime<()>) {

    }

    fn handle_binded(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        if let Some(context) = self.get_context() {
            //通知客户端连接事件
            let uid = handle.get_uid();
            if let Err(e) = result {
                context.notify_connect(uid, Err(e));
            } else {
                context.notify_connect(uid, Ok(handle));
            }
        }

        async move {

        }.boxed_local()
    }

    fn handle_readed(&self,
                     _handle: SocketHandle<S>,
                     result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()> {
        if let Some(context) = self.get_context() {
            //通知客户端接收事件
            let task = match result {
                Err(e) => {
                    context.notify_receive(Err(e))
                },
                Ok((bin, _)) => {
                    context.notify_receive(Ok(bin))
                },
            };

            async move {
                task.await;
            }.boxed_local()
        } else {
            async move {

            }.boxed_local()
        }
    }

    fn handle_writed(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        if let Some(context) = self.get_context() {
            //通知客户端关闭事件
            context.notify_close(handle.get_uid(), result);
        }

        async move {

        }.boxed_local()
    }
}

#[test]
fn test_udp_client() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    //启动服务端
    let mut factory = PortsAdapterFactory::<UdpSocket>::new();
    factory.bind(38080, Box::new(TestServerService));
    let addrs = vec![SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080)];

    match SocketListener::bind(vec![rt.clone()],
                               factory,
                               addrs,
                               1024,
                               1024,
                               0xffff,
                               0xffff,
                               Some(1)) {
        Err(e) => {
            println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
        },
        Ok(_) => {
            println!("===> Socket Listener Bind Ipv4 Address Ok");
        }
    }

    //启动客户端
    let client = UdpClient::<UdpSocket>::new(vec![rt.clone()],
                                             1024,
                                             1024,
                                             0xffff,
                                             0xffff,
                                             Some(1))
        .unwrap();
    let client_copy = client.clone();
    thread::spawn(move || {
        loop {
            client.poll_event();
            thread::sleep(Duration::from_millis(1));
        }
    });

    rt.spawn(async move {
        for index in 0..10 {
            match client_copy.connect(("127.0.0.1:".to_string() + (5000 + index).to_string().as_str()).parse().unwrap(),
                                      "127.0.0.1:38080".parse().unwrap(),
                                      None,
                                      None,
                                      TestClientService(None)).await {
                Err(e) => {
                    println!("Client connect failed, reason: {:?}", e);
                },
                Ok(connection) => {
                    println!("Client connect ok, uid: {:?}, remote: {:?}, local: {:?}",
                             connection.get_uid(),
                             connection.get_remote(),
                             connection.get_local());

                    connection.write(Cursor::new(b"Hello World!"));

                    let mut count = 3;
                    while let Some(result) = connection.read().await {
                        match result {
                            Err(e) => {
                                println!("Client connection receive failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                         connection.get_uid(),
                                         connection.get_remote(),
                                         connection.get_local(),
                                         e);
                                break;
                            },
                            Ok(bin) => {
                                println!("Client connection receive ok, uid: {:?}, remote: {:?}, local: {:?}, count: {:?}, bin: {:?}",
                                         connection.get_uid(),
                                         connection.get_remote(),
                                         connection.get_local(),
                                         count,
                                         String::from_utf8(bin));

                                count -= 1;
                                if count == 0 {
                                    break;
                                }
                            },
                        }
                    }

                    let uid = connection.get_uid();
                    let remote = connection.get_remote().unwrap().clone();
                    let local = connection.get_local().clone();
                    let result = connection.close(Ok(())).await;
                    println!("Client connection close, uid: {:?}, remote: {:?}, local: {:?}, result: {:?}",
                             uid,
                             remote,
                             local,
                             result);
                },
            }
        }
    });

    thread::sleep(Duration::from_millis(10000000));
}



