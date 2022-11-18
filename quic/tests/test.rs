use std::thread;
use std::any::Any;
use std::sync::Arc;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use std::net::{IpAddr, SocketAddr};
use std::io::{Error, Result, ErrorKind};

use futures::{TryFutureExt,
              future::{FutureExt, BoxFuture, LocalBoxFuture},
              stream::StreamExt, AsyncWriteExt};
use pi_async::rt::{serial::{AsyncRuntime, AsyncRuntimeBuilder, AsyncValue}};
use quinn_proto::{EndpointConfig, StreamId, VarInt};
use tokio::runtime::Builder;
use rustls;
use quinn;
use bytes::Buf;
use env_logger;

use udp::{Socket, AsyncService,
          connect::UdpSocket,
          server::{PortsAdapterFactory, SocketListener}};

use quic::{AsyncService as QuicAsyncService, SocketHandle, SocketEvent,
           server::QuicListener,
           client::QuicClient,
           utils::{QuicSocketReady, load_certs, load_private_key}};
use quic::connect::QuicSocket;

#[test]
fn test_quinn() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let server_rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    //运行服务器
    server_rt.spawn(async move {
        let certs = load_certs("./tests/7285407__17youx.cn.pem").unwrap();
        let key = load_private_key("./tests/7285407__17youx.cn.key").unwrap();
        let mut server_crypto = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(certs, key).unwrap();

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        Arc::get_mut(&mut server_config.transport)
            .unwrap()
            .max_concurrent_uni_streams(0_u8.into());

        let (endpoint, mut incoming) = quinn::Endpoint::server(server_config,
                                                           "0.0.0.0:38080".parse().unwrap())
            .unwrap();
        println!("Server running");

        while let Some(conn) = incoming.next().await {
            tokio::spawn(
                handle_connection(conn).unwrap_or_else(move |e| {
                    println!("connection failed: {reason}", reason = e.to_string())
                }),
            );
        }
    });

    thread::sleep(Duration::from_millis(5000));

    let client_rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    //运行客户端
    client_rt.spawn(async move {
        //构建客户端证书
        let mut roots = rustls::RootCertStore::empty();
        roots.add(&rustls::Certificate(std::fs::read("./tests/DigiCert Global Root CA.der")
            .unwrap()))
            .unwrap();

        //构建quic客户端
        let mut client_crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0"
            .parse()
            .unwrap())
            .unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(client_crypto)));

        //连接指定的服务端
        let con = endpoint
            .connect("127.0.0.1:38080".parse().unwrap(), "test.17youx.cn")
            .unwrap()
            .await
            .map_err(|e| println!("failed to connect: {:?}", e))
            .expect("Client connect failed");
        println!("!!!!!!Client connect ok, conn: {:?}", con);

        let (mut send, recv) = con
            .connection
            .open_bi()
            .await
            .map_err(|e| println!("failed to open stream: {:?}", e))
            .expect("Open stream failed");

        loop_client(con, send, recv, 1000).await;
    });

    thread::sleep(Duration::from_millis(10000000));
}

async fn handle_connection(conn: quinn::Connecting) -> Result<()> {
    let quinn::NewConnection {
        connection,
        mut bi_streams,
        ..
    } = conn.await?;
    println!("!!!!!!Server accept ok, connection: {:?}", connection);

    let mut streams = None;
    while let Some(stream) = bi_streams.next().await {
        match stream {
            Err(e@quinn::ConnectionError::ApplicationClosed { .. }) => {
                println!("Connection closed, reason: {:?}", e);
                return Ok(());
            }
            Err(e) => {
                return Err(Error::new(ErrorKind::Other, format!("Open stream failed, reason: {:?}", e)));
            }
            Ok(s) => {
                println!("Connection ok, streams: {:?}", s);
                streams = Some(s);
                break;
            },
        }
    };

    if let Some((send, recv)) = streams {
        loop_server(send, recv).await;
    }

    Ok(())
}

fn loop_server(mut send_stream: quinn::SendStream,
               mut recv_stream: quinn::RecvStream)
    -> BoxFuture<'static, ()> {
    async move {
        let mut buf = Vec::with_capacity(3600);
        buf.resize(3600, 0);
        match recv_stream.read(buf.as_mut_slice()).await {
            Err(e) => println!("Server recv failed, reason: {:?}", e),
            Ok(Some(len)) => {
                println!("Server recv ok, len: {}", len);
                buf.truncate(len);
                send_stream.write_all(buf.as_slice()).await;
            },
            Ok(None) => (),
        }

        tokio::spawn(loop_server(send_stream, recv_stream));
    }.boxed()
}

fn loop_client(conn: quinn::NewConnection,
               mut send_stream: quinn::SendStream,
               mut recv_stream: quinn::RecvStream,
               count: usize) -> BoxFuture<'static, ()> {
    async move {
        if count == 0 {
            conn.connection.close(VarInt::from_u32(1000000000), b"Normal");
            return;
        }

        send_stream.write_all(b"Hello World!").await;
        let mut buf = Vec::with_capacity(3600);
        buf.resize(3600, 0);
        match recv_stream.read(buf.as_mut_slice()).await {
            Err(e) => println!("Client recv failed, reason: {:?}", e),
            Ok(Some(len)) => {
                buf.truncate(len);
                println!("Client recv ok, len: {}, buf: {:?}", len, String::from_utf8(buf))
            },
            Ok(None) => (),
        }

        tokio::spawn(loop_client(conn, send_stream, recv_stream, count - 1));
    }.boxed()
}

struct TestService;

impl<S: Socket> QuicAsyncService<S> for TestService {
    fn handle_connected(&self,
                        handle: SocketHandle<S>,
                        result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = result {
                println!("===> Connect Quic failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                         handle.get_uid(),
                         handle.get_remote(),
                         handle.get_local(),
                         e);
                return;
            }
            println!("===> Connect Quic ok, uid: {:?}, remote: {:?}, local: {:?}, is_0rtt: {:?}, main_stream_id: {:?}",
                     handle.get_uid(),
                     handle.get_remote(),
                     handle.get_local(),
                     handle.is_0rtt(),
                     handle.get_main_stream_id());

            handle.set_ready(QuicSocketReady::Readable); //开始首次读
        }.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     result: Result<usize>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = result {
                println!("===> Socket read failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                         handle.get_uid(),
                         handle.get_remote(),
                         handle.get_local(),
                         e);
                return;
            }

            let mut ready_len = 0;
            let remaining = if let Some(len) = handle.read_buffer_remaining() {
                len
            } else {
                return;
            };

            if remaining == 0 {
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

            if let Some(buf) = handle.get_read_buffer().lock().as_mut() {
                println!("===> Socket read ok after connect, uid: {:?}, remote: {:?}, local: {:?}, is_0rtt: {:?}, main_stream_id: {:?}, data: {:?}",
                         handle.get_uid(),
                         handle.get_remote(),
                         handle.get_local(),
                         handle.is_0rtt(),
                         handle.get_main_stream_id(),
                         String::from_utf8_lossy(buf.copy_to_bytes(buf.remaining()).as_ref()));

                let bin = b"Hello World!";
                let _ = handle.write_ready(bin);
            }
        }.boxed_local()
    }

    fn handle_writed(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = result {
                println!("===> Socket write failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                         handle.get_uid(),
                         handle.get_remote(),
                         handle.get_local(),
                         e);
                return;
            }
            println!("===> Socket Write Ok, uid: {:?}", handle.get_uid());
        }.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle<S>,
                     stream_id: Option<StreamId>,
                     code: u32,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            println!("===> Socket closed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, code: {:?}, reason: {:?}",
                     handle.get_uid(),
                     handle.get_remote(),
                     handle.get_local(),
                     stream_id,
                     code,
                     result);
        }.boxed_local()
    }

    /// 异步处理已超时
    fn handle_timeouted(&self,
                        handle: SocketHandle<S>,
                        result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }
}

#[test]
fn test_server() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<UdpSocket>::new();
    //Quic连接监听器可以有多个运行时
    let listener = QuicListener::new(vec![rt.clone()],
                                     "./tests/7285407__17youx.cn.pem",
                                     "./tests/7285407__17youx.cn.key",
                                     Default::default(),
                                     65535,
                                     65535,
                                     Arc::new(TestService),
                                     Some(10))
        .expect("Create quic listener failed");
    factory.bind(38080, Box::new(listener));
    let addrs = vec![SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080)];

    //用于Quic的udp连接监听器，有且只允许有一个运行时
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

            let tokio_rt = Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();

            tokio_rt.spawn(async move {
                //构建客户端证书
                let mut roots = rustls::RootCertStore::empty();
                roots.add(&rustls::Certificate(std::fs::read("./tests/DigiCert Global Root CA.der")
                    .unwrap()))
                    .unwrap();

                //构建quic客户端
                let mut client_crypto = rustls::ClientConfig::builder()
                    .with_safe_defaults()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let mut endpoint = quinn::Endpoint::client("127.0.0.1:0"
                    .parse()
                    .unwrap())
                    .unwrap();
                endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(client_crypto)));

                //连接指定的服务端
                let mut con = endpoint
                    .connect("127.0.0.1:38080".parse().unwrap(), "test.17youx.cn")
                    .unwrap()
                    .await
                    .map_err(|e| println!("failed to connect: {:?}", e))
                    .expect("Client connect failed");
                println!("Client connect ok, connect: {:?}", con);

                let (mut send, recv) = con
                    .connection
                    .open_bi()
                    .await
                    .map_err(|e| println!("failed to open stream: {:?}", e))
                    .expect("Open stream failed");

                loop_client(con, send, recv, 10).await;
            });

            thread::sleep(Duration::from_millis(10000000));
        }
    }
}

#[test]
fn test_client() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let udp_rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let quic_rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    let mut factory = PortsAdapterFactory::<UdpSocket>::new();
    //Quic连接监听器可以有多个运行时
    let listener = QuicListener::new(vec![quic_rt.clone()],
                                     "./tests/7285407__17youx.cn.pem",
                                     "./tests/7285407__17youx.cn.key",
                                     Default::default(),
                                     65535,
                                     65535,
                                     Arc::new(TestService),
                                     Some(1))
        .expect("Create quic listener failed");
    factory.bind(38080, Box::new(listener));
    let addrs = vec![SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080)];

    //用于Quic的udp连接监听器，有且只允许有一个运行时
    match SocketListener::bind(vec![udp_rt.clone()],
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

            let client = QuicClient::<UdpSocket>::with_cert_file(udp_rt.clone(),
                                                                 vec![quic_rt],
                                                                 "./tests/DigiCert Global Root CA.der",
                                                                 EndpointConfig::default(),
                                                                 65535,
                                                                 65535,
                                                                 65535,
                                                                 65535,
                                                                 5000,
                                                                 Some(10),
                                                                 None)
                .unwrap();

            udp_rt.spawn(async move {
                match client.connect("127.0.0.1:5000".parse().unwrap(),
                                     "127.0.0.1:38080".parse().unwrap(),
                                     "test.17youx.cn",
                                     None,
                                     None).await {
                    Err(e) => {
                        println!("!!!!!!Quic client connect failed, reason: {:?}", e);
                    },
                    Ok(connection) => {
                        println!("!!!!!!Quic client connect ok, uid: {:?}, remote: {:?}, local: {:?}",
                                 connection.get_uid(),
                                 connection.get_remote(),
                                 connection.get_local());

                        for index in 0..10 {
                            if let Err(e) = connection.write([b"Hello World ", index.to_string().as_bytes()].concat()) {
                                println!("Quic client send failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                         connection.get_uid(),
                                         connection.get_remote(),
                                         connection.get_local(),
                                         e);
                                break;
                            }

                            if let Some(resp) = connection.read().await {
                                println!("Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, bin: {:?}",
                                         connection.get_uid(),
                                         connection.get_remote(),
                                         connection.get_local(),
                                         String::from_utf8(resp.to_vec()));
                            }
                        }

                        let uid = connection.get_uid();
                        let remote = connection.get_remote();
                        let local = connection.get_local();
                        let result = connection
                            .close(10000,
                                   Err(Error::new(ErrorKind::Other, "Normal"))).await;
                        println!("Quic client close, uid: {:?}, remote: {:?}, local: {:?}, result: {:?}",
                                 uid,
                                 remote,
                                 local,
                                 result);
                    }
                }
            });

            thread::sleep(Duration::from_millis(10000000));
        }
    }


}




