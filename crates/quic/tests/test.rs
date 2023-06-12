// extern crate core;

// use std::thread;
// use std::fs::read;
// use std::any::Any;
// use std::sync::Arc;
// use std::path::Path;
// use std::str::FromStr;
// use std::net::{IpAddr, SocketAddr};
// use std::time::{Duration, Instant, SystemTime};
// use std::io::{Error, Result, ErrorKind};

// use futures::{TryFutureExt,
//               future::{FutureExt, BoxFuture, LocalBoxFuture},
//               stream::StreamExt, AsyncWriteExt};
// use pi_async::rt::{serial::{AsyncRuntime, AsyncRuntimeBuilder, AsyncValue}};
// use quinn_proto::{Dir, EndpointConfig, TransportConfig, StreamId, VarInt};
// use tokio::runtime::Builder;
// use rustls;
// use quinn;
// use bytes::Buf;
// use ed25519_dalek::PublicKey;
// use x509_parser::pem::Pem;
// use pem::parse;
// use der_parser::parse_ber;
// use env_logger;
// use tracing::Instrument;
// use tracing_chrome::ChromeLayerBuilder;
// use tracing_subscriber::{registry::Registry, prelude::*};

// use udp::{AsyncService,
//           terminal::UdpTerminal};

// use pi_quic::{AsyncService as QuicAsyncService, SocketHandle, SocketEvent,
//            connect::QuicSocket,
//            server::{QuicListener, ClientCertVerifyLevel},
//            client::{QuicClient, ServerCertVerifyLevel},
//            utils::{QuicSocketReady, load_certs_file, load_key_file}};

// #[test]
// fn test_quinn() {
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     let server_rt = Builder::new_multi_thread()
//         .worker_threads(2)
//         .enable_all()
//         .build()
//         .unwrap();

//     //运行服务器
//     server_rt.spawn(async move {
//         let certs = load_certs_file("./tests/7285407__17youx.cn.pem").unwrap();
//         let key = load_key_file("./tests/7285407__17youx.cn.key").unwrap();
//         let mut server_crypto = rustls::ServerConfig::builder()
//             .with_safe_defaults()
//             .with_no_client_auth()
//             .with_single_cert(certs, key).unwrap();

//         let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
//         Arc::get_mut(&mut server_config.transport)
//             .unwrap()
//             .max_concurrent_uni_streams(0_u8.into());

//         let (endpoint, mut incoming) = quinn::Endpoint::server(server_config,
//                                                            "0.0.0.0:38080".parse().unwrap())
//             .unwrap();
//         println!("Server running");

//         while let Some(conn) = incoming.next().await {
//             tokio::spawn(
//                 handle_connection(conn).unwrap_or_else(move |e| {
//                     println!("connection failed: {reason}", reason = e.to_string())
//                 }),
//             );
//         }
//     });

//     thread::sleep(Duration::from_millis(1000));

//     let client_rt = Builder::new_multi_thread()
//         .worker_threads(2)
//         .enable_all()
//         .build()
//         .unwrap();

//     //运行客户端
//     client_rt.spawn(async move {
//         //构建客户端证书
//         let mut roots = rustls::RootCertStore::empty();
//         roots.add(&rustls::Certificate(std::fs::read("./tests/DigiCert Global Root CA.der")
//             .unwrap()))
//             .unwrap();

//         //构建quic客户端
//         let mut client_crypto = rustls::ClientConfig::builder()
//             .with_safe_defaults()
//             .with_root_certificates(roots)
//             .with_no_client_auth();
//         let mut endpoint = quinn::Endpoint::client("127.0.0.1:0"
//             .parse()
//             .unwrap())
//             .unwrap();
//         endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(client_crypto)));

//         //连接指定的服务端
//         let now = Instant::now();
//         let con = endpoint
//             .connect("127.0.0.1:38080".parse().unwrap(), "test.17youx.cn")
//             .unwrap()
//             .await
//             .map_err(|e| println!("failed to connect: {:?}", e))
//             .expect("Client connect failed");
//         println!("!!!!!!Client connect ok, conn: {:?}", con);

//         let (mut send, recv) = con
//             .connection
//             .open_bi()
//             .await
//             .map_err(|e| println!("failed to open stream: {:?}", e))
//             .expect("Open stream failed");
//         println!("!!!!!!Client open bi ok, time: {:?}", now.elapsed());

//         thread::sleep(Duration::from_millis(1000));

//         loop_client(con, send, recv, 10).await;
//     });

//     thread::sleep(Duration::from_millis(10000000));
// }

// async fn handle_connection(conn: quinn::Connecting) -> Result<()> {
//     let quinn::NewConnection {
//         connection,
//         mut bi_streams,
//         ..
//     } = conn.await?;
//     println!("!!!!!!Server accept ok, connection: {:?}", connection);

//     let mut streams = None;
//     while let Some(stream) = bi_streams.next().await {
//         match stream {
//             Err(e@quinn::ConnectionError::ApplicationClosed { .. }) => {
//                 println!("Connection closed, reason: {:?}", e);
//                 return Ok(());
//             }
//             Err(e) => {
//                 return Err(Error::new(ErrorKind::Other, format!("Open stream failed, reason: {:?}", e)));
//             }
//             Ok(s) => {
//                 println!("Connection ok, streams: {:?}", s);
//                 streams = Some(s);
//                 break;
//             },
//         }
//     };

//     if let Some((send, recv)) = streams {
//         loop_server(send, recv).await;
//     }

//     Ok(())
// }

// fn loop_server(mut send_stream: quinn::SendStream,
//                mut recv_stream: quinn::RecvStream)
//     -> BoxFuture<'static, ()> {
//     async move {
//         loop {
//             let mut buf = Vec::with_capacity(3600);
//             buf.resize(3600, 0);
//             match recv_stream.read(buf.as_mut_slice()).await {
//                 Err(_e) => {

//                 },
//                 Ok(Some(len)) => {
//                     println!("Server recv success, len: {}", len);
//                     buf.truncate(len);
//                     send_stream.write_all(buf.as_slice()).await;
//                 },
//                 Ok(None) => (),
//             }
//         }
//     }.boxed()
// }

// fn loop_client(conn: quinn::NewConnection,
//                mut send_stream: quinn::SendStream,
//                mut recv_stream: quinn::RecvStream,
//                count: usize) -> BoxFuture<'static, ()> {
//     async move {
//         if count == 0 {
//             conn.connection.close(VarInt::from_u32(1000000000), b"Normal");
//             return;
//         }

//         let now = Instant::now();
//         send_stream.write_all(b"Hello World!").await;
//         let mut buf = Vec::with_capacity(3600);
//         buf.resize(3600, 0);
//         match recv_stream.read(buf.as_mut_slice()).await {
//             Err(e) => println!("Client recv failed, reason: {:?}", e),
//             Ok(Some(len)) => {
//                 buf.truncate(len);
//                 println!("Client recv success, time: {:?}, len: {:?}, buf: {:?}", now.elapsed(), len, String::from_utf8(buf))
//             },
//             Ok(None) => (),
//         }

//         tokio::spawn(loop_client(conn, send_stream, recv_stream, count - 1));
//     }.boxed()
// }

// struct TestService;

// impl QuicAsyncService for TestService {
//     fn handle_connected(&self,
//                         handle: SocketHandle,
//                         result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Connect Quic failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          e);
//                 return;
//             }
//             println!("===> Connect Quic ok, uid: {:?}, remote: {:?}, local: {:?}, is_0rtt: {:?}, main_stream_id: {:?}",
//                      handle.get_uid(),
//                      handle.get_remote(),
//                      handle.get_local(),
//                      handle.is_0rtt(),
//                      handle.get_main_stream_id());

//             handle
//                 .set_ready(handle.get_main_stream_id().unwrap().clone(),
//                            QuicSocketReady::Readable); //开始首次读
//         }.boxed_local()
//     }

//     fn handle_opened_expanding_stream(&self,
//                                       handle: SocketHandle,
//                                       stream_id: StreamId,
//                                       stream_type: Dir,
//                                       result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Open expanding stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, stream_type: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          stream_type,
//                          e);
//             } else {
//                 println!("===> Open expanding stream ok, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, stream_type: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          stream_type);

//                 handle
//                     .set_ready(stream_id,
//                                QuicSocketReady::Readable); //开始首次读
//             }
//         }.boxed_local()
//     }

//     fn handle_readed(&self,
//                      handle: SocketHandle,
//                      stream_id: StreamId,
//                      result: Result<usize>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Socket read failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          e);
//                 return;
//             }

//             let mut ready_len = 0;
//             let remaining = if let Some(len) = handle.read_buffer_remaining() {
//                 len
//             } else {
//                 return;
//             };

//             if remaining == 0 {
//                 //当前读缓冲中没有数据，则异步准备读取数据
//                 println!("!!!!!!readed, read ready start, len: 0");
//                 ready_len = match handle.read_ready( 0) {
//                     Err(len) => len,
//                     Ok(value) => {
//                         println!("!!!!!!wait read_ready");
//                         let r = value.await;
//                         println!("!!!!!!wakeup read_ready, len: {}", r);
//                         r
//                     },
//                 };

//                 if ready_len == 0 {
//                     //当前连接已关闭，则立即退出
//                     return;
//                 }
//             }

//             if let Some(buf) = handle.get_read_buffer(&stream_id).as_ref().unwrap().lock().as_mut() {
//                 println!("===> Socket read ok after connect, uid: {:?}, remote: {:?}, local: {:?}, is_0rtt: {:?}, stream_id: {:?}, data: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          handle.is_0rtt(),
//                          stream_id,
//                          String::from_utf8_lossy(buf.copy_to_bytes(buf.remaining()).as_ref()));

//                 let bin = b"Hello World!";
//                 let _ = handle.write_ready(stream_id, bin);
//             }
//         }.boxed_local()
//     }

//     fn handle_writed(&self,
//                      handle: SocketHandle,
//                      stream_id: StreamId,
//                      result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Socket write failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          e);
//                 return;
//             }
//             println!("===> Socket Write Ok, uid: {:?}", handle.get_uid());
//         }.boxed_local()
//     }

//     fn handle_closed(&self,
//                      handle: SocketHandle,
//                      stream_id: Option<StreamId>,
//                      code: u32,
//                      result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             println!("===> Socket closed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, code: {:?}, reason: {:?}",
//                      handle.get_uid(),
//                      handle.get_remote(),
//                      handle.get_local(),
//                      stream_id,
//                      code,
//                      result);
//         }.boxed_local()
//     }

//     /// 异步处理已超时
//     fn handle_timeouted(&self,
//                         handle: SocketHandle,
//                         result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()> {
//         async move {

//         }.boxed_local()
//     }
// }

// #[test]
// fn test_server_with_quinn_client() {
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     //Quic连接监听器可以有多个运行时
//     let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     let listener = QuicListener::new(vec![rt.clone()],
//                                      "./tests/quic.com.crt",
//                                      "./tests/quic.com.key",
//                                      ClientCertVerifyLevel::Ignore,
//                                      Default::default(),
//                                      65535,
//                                      65535,
//                                      100000,
//                                      None,
//                                      Arc::new(TestService),
//                                      1)
//         .expect("Create quic listener failed");
//     let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

//     //用于Quic的udp连接监听器，有且只允许有一个运行时
//     let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     match UdpTerminal::bind(addrs,
//                             rt,
//                             8 * 1024 * 1024,
//                             8 * 1024 * 1024,
//                             0xffff,
//                             0xffff,
//                             Box::new(listener)) {
//         Err(e) => {
//             println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
//         },
//         Ok(_) => {
//             println!("===> Socket Listener Bind Ipv4 Address Ok");

//             let tokio_rt = Builder::new_multi_thread()
//                 .worker_threads(2)
//                 .enable_all()
//                 .build()
//                 .unwrap();

//             tokio_rt.spawn(async move {
//                 //构建客户端证书
//                 let mut roots = rustls::RootCertStore::empty();
//                 roots.add(&rustls::Certificate(std::fs::read("./tests/example.com.der")
//                     .unwrap()))
//                     .unwrap();

//                 //构建quic客户端
//                 let mut client_crypto = rustls::ClientConfig::builder()
//                     .with_safe_defaults()
//                     .with_root_certificates(roots)
//                     .with_no_client_auth();
//                 let mut endpoint = quinn::Endpoint::client("127.0.0.1:0"
//                     .parse()
//                     .unwrap())
//                     .unwrap();
//                 endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(client_crypto)));

//                 //连接指定的服务端
//                 let now = Instant::now();
//                 let mut con = endpoint
//                     .connect("127.0.0.1:38080".parse().unwrap(), "test.quic.com")
//                     .unwrap()
//                     .await
//                     .map_err(|e| println!("failed to connect: {:?}", e))
//                     .expect("Client connect failed");
//                 println!("!!!!!!Client connect ok, conn: {:?}, time: {:?}", con, now.elapsed());

//                 let (mut send, recv) = con
//                     .connection
//                     .open_bi()
//                     .await
//                     .map_err(|e| println!("failed to open stream: {:?}", e))
//                     .expect("Open stream failed");

//                 thread::sleep(Duration::from_millis(1000));

//                 loop_client(con, send, recv, 10).await;
//             });

//             thread::sleep(Duration::from_millis(10000000));
//         }
//     }
// }

// #[test]
// fn test_client_with_quinn_server() {
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     let udp_rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     let quic_rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     let client_rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     let server_rt = Builder::new_multi_thread()
//         .worker_threads(2)
//         .enable_all()
//         .build()
//         .unwrap();

//     //运行服务器
//     server_rt.spawn(async move {
//         let certs = load_certs_file("./tests/7285407__17youx.cn.pem").unwrap();
//         let key = load_key_file("./tests/7285407__17youx.cn.key").unwrap();
//         let mut server_crypto = rustls::ServerConfig::builder()
//             .with_safe_defaults()
//             .with_no_client_auth()
//             .with_single_cert(certs, key).unwrap();

//         let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
//         Arc::get_mut(&mut server_config.transport)
//             .unwrap()
//             .max_concurrent_uni_streams(0_u8.into());

//         let (endpoint, mut incoming) = quinn::Endpoint::server(server_config,
//                                                                "0.0.0.0:38080".parse().unwrap())
//             .unwrap();
//         println!("Server running");

//         while let Some(conn) = incoming.next().await {
//             tokio::spawn(
//                 handle_connection(conn).unwrap_or_else(move |e| {
//                     println!("connection failed: {reason}", reason = e.to_string())
//                 }),
//             );
//         }
//     });

//     thread::sleep(Duration::from_millis(1000));

//     let mut transport_config = TransportConfig::default();
//     transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//     let client = QuicClient::new("127.0.0.1:5000".parse().unwrap(),
//                                  udp_rt.clone(),
//                                  vec![quic_rt],
//                                  ServerCertVerifyLevel::CaCertFile("./tests/DigiCert Global Root CA.der".into()),
//                                  EndpointConfig::default(),
//                                  65535,
//                                  65535,
//                                  8 * 1024 * 1024,
//                                  8 * 1024 * 1024,
//                                  65535,
//                                  65535,
//                                  Some(Arc::new(transport_config)),
//                                  None,
//                                  5000,
//                                  1)
//         .unwrap();
//     client_rt.spawn(async move {
//         let now = Instant::now();
//         match client.connect("127.0.0.1:38080".parse().unwrap(),
//                              "test.17youx.cn",
//                              None,
//                              None).await {
//             Err(e) => {
//                 println!("!!!!!!Quic client connect failed, reason: {:?}", e);
//             },
//             Ok(connection) => {
//                 println!("!!!!!!Quic client connect ok, uid: {:?}, remote: {:?}, local: {:?}, time: {:?}",
//                          connection.get_uid(),
//                          connection.get_remote(),
//                          connection.get_local(),
//                          now.elapsed());

//                 thread::sleep(Duration::from_millis(1000));

//                 for index in 0..10 {
//                     let main_stream_id = connection.get_handle().get_main_stream_id().unwrap().clone();
//                     if let Err(e) = connection.write(main_stream_id,
//                                                      [b"Hello World ", index.to_string().as_bytes()].concat()) {
//                         println!("Quic client send failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                                  connection.get_uid(),
//                                  connection.get_remote(),
//                                  connection.get_local(),
//                                  main_stream_id,
//                                  e);
//                         break;
//                     }

//                     if let Some(resp) = connection.read(&main_stream_id).await {
//                         println!("Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, bin: {:?}",
//                                  connection.get_uid(),
//                                  connection.get_remote(),
//                                  connection.get_local(),
//                                  main_stream_id,
//                                  String::from_utf8(resp.to_vec()));
//                     }
//                 }

//                 let uid = connection.get_uid();
//                 let remote = connection.get_remote();
//                 let local = connection.get_local();
//                 let result = connection
//                     .close(10000,
//                            Err(Error::new(ErrorKind::Other, "Normal"))).await;
//                 println!("Quic client close, uid: {:?}, remote: {:?}, local: {:?}, result: {:?}",
//                          uid,
//                          remote,
//                          local,
//                          result);
//             }
//         }
//     });

//     thread::sleep(Duration::from_millis(10000000));
// }

// #[test]
// fn test_client() {
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     let udp_rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     let quic_rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     let client_rt = AsyncRuntimeBuilder::default_local_thread(None, None);

//     let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     let listener = QuicListener::new(vec![rt.clone()],
//                                      "./tests/quic.com.crt",
//                                      "./tests/quic.com.key",
//                                      ClientCertVerifyLevel::Ignore,
//                                      Default::default(),
//                                      65535,
//                                      65535,
//                                      100000,
//                                      None,
//                                      Arc::new(TestService),
//                                      1)
//         .expect("Create quic listener failed");
//     let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

//     //用于Quic的udp连接监听器，有且只允许有一个运行时
//     let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
//     match UdpTerminal::bind(addrs,
//                             rt,
//                             8 * 1024 * 1024,
//                             8 * 1024 * 1024,
//                             0xffff,
//                             0xffff,
//                             Box::new(listener)) {
//         Err(e) => {
//             println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
//         },
//         Ok(_) => {
//             println!("===> Socket Listener Bind Ipv4 Address Ok");

//             let mut transport_config = TransportConfig::default();
//             transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//             let client = QuicClient::new("127.0.0.1:5000".parse().unwrap(),
//                                          udp_rt.clone(),
//                                          vec![quic_rt],
//                                          ServerCertVerifyLevel::CaCertFile("./tests/example.com.der".into()),
//                                          EndpointConfig::default(),
//                                          65535,
//                                          65535,
//                                          8 * 1024 * 1024,
//                                          8 * 1024 * 1024,
//                                          65535,
//                                          65535,
//                                          Some(Arc::new(transport_config)),
//                                          None,
//                                          5000,
//                                          1)
//                 .unwrap();
//             client_rt.spawn(async move {
//                 match client.connect("127.0.0.1:38080".parse().unwrap(),
//                                      "test.quic.com",
//                                      None,
//                                      None).await {
//                     Err(e) => {
//                         println!("!!!!!!Quic client connect failed, reason: {:?}", e);
//                     },
//                     Ok(connection) => {
//                         println!("!!!!!!Quic client connect ok, uid: {:?}, remote: {:?}, local: {:?}",
//                                  connection.get_uid(),
//                                  connection.get_remote(),
//                                  connection.get_local());

//                         for index in 0..10 {
//                             let main_stream_id = connection.get_handle().get_main_stream_id().unwrap().clone();
//                             if let Err(e) = connection.write(main_stream_id,
//                                                              [b"Hello World ", index.to_string().as_bytes()].concat()) {
//                                 println!("Quic client send failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                                          connection.get_uid(),
//                                          connection.get_remote(),
//                                          connection.get_local(),
//                                          main_stream_id,
//                                          e);
//                                 break;
//                             }

//                             if let Some(resp) = connection.read(&main_stream_id).await {
//                                 println!("Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, bin: {:?}",
//                                          connection.get_uid(),
//                                          connection.get_remote(),
//                                          connection.get_local(),
//                                          main_stream_id,
//                                          String::from_utf8(resp.to_vec()));
//                             }
//                         }

//                         let uid = connection.get_uid();
//                         let remote = connection.get_remote();
//                         let local = connection.get_local();
//                         let result = connection
//                             .close(10000,
//                                    Err(Error::new(ErrorKind::Other, "Normal"))).await;
//                         println!("Quic client close, uid: {:?}, remote: {:?}, local: {:?}, result: {:?}",
//                                  uid,
//                                  remote,
//                                  local,
//                                  result);
//                     }
//                 }
//             });

//             thread::sleep(Duration::from_millis(10000000));
//         }
//     }
// }

// #[test]
// fn test_client_with_self_signed_certificate() {
//     // let (chrome_layer, _guard) = ChromeLayerBuilder::new().build();
//     // tracing_subscriber::registry().with(chrome_layer).init();
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_udp_rt"), None);
//     let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_quic_rt"), None);

//     let listener = QuicListener::new(vec![quic_rt],
//                                      "./tests/quic.com.crt",
//                                      "./tests/quic.com.key",
//                                      ClientCertVerifyLevel::Ignore,
//                                      Default::default(),
//                                      65535,
//                                      65535,
//                                      100000,
//                                      None,
//                                      Arc::new(TestService),
//                                      1)
//         .expect("Create quic listener failed");
//     let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

//     //用于Quic的udp连接监听器，有且只允许有一个运行时
//     match UdpTerminal::bind(addrs,
//                             udp_rt,
//                             8 * 1024 * 1024,
//                             8 * 1024 * 1024,
//                             0xffff,
//                             0xffff,
//                             Box::new(listener)) {
//         Err(e) => {
//             println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
//         },
//         Ok(_) => {
//             println!("===> Socket Listener Bind Ipv4 Address Ok");

//             let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_udp_rt"), None);
//             let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_quic_rt"), None);
//             let client_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_rt"), None);

//             let mut transport_config = TransportConfig::default();
//             transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//             let client = QuicClient::new("127.0.0.1:5000".parse().unwrap(),
//                                          udp_rt,
//                                          vec![quic_rt],
//                                          ServerCertVerifyLevel::Custom(Arc::new(TestServerCertVerifier::new())),
//                                          EndpointConfig::default(),
//                                          65535,
//                                          65535,
//                                          8 * 1024 * 1024,
//                                          8 * 1024 * 1024,
//                                          65535,
//                                          65535,
//                                          Some(Arc::new(transport_config)),
//                                          None,
//                                          5000,
//                                          1)
//                 .unwrap();

//             let pem = parse(read("./tests/quic.com.pub").unwrap()).unwrap();
//             println!("!!!!!!public key: {:?}", parse_ber(pem.contents()));

//             client_rt.spawn(async move {
//                 let now = Instant::now();
//                 match client.connect("127.0.0.1:38080".parse().unwrap(),
//                                      "0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff",
//                                      None,
//                                      None).await {
//                     Err(e) => {
//                         println!("!!!!!!Quic client connect failed, reason: {:?}", e);
//                     },
//                     Ok(connection) => {
//                         println!("!!!!!!Quic client connect ok, uid: {:?}, remote: {:?}, local: {:?}, time: {:?}",
//                                  connection.get_uid(),
//                                  connection.get_remote(),
//                                  connection.get_local(),
//                                  now.elapsed());

//                         for _ in 0..3 {
//                             for index in 0..10 {
//                                 let now = Instant::now();
//                                 let main_stream_id = connection.get_handle().get_main_stream_id().unwrap().clone();
//                                 if let Err(e) = connection.write(main_stream_id, [b"Hello World ", index.to_string().as_bytes()].concat()) {
//                                     println!("Quic client send failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                                              connection.get_uid(),
//                                              connection.get_remote(),
//                                              connection.get_local(),
//                                              main_stream_id,
//                                              e);
//                                     break;
//                                 }

//                                 if let Some(resp) = connection.read(&main_stream_id).await {
//                                     println!("Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, bin: {:?}",
//                                              connection.get_uid(),
//                                              connection.get_remote(),
//                                              connection.get_local(),
//                                              main_stream_id,
//                                              String::from_utf8(resp.to_vec()));
//                                 }
//                                 println!("!!!!!!roll time: {:?}", now.elapsed());
//                             }
//                             thread::sleep(Duration::from_millis(1000));
//                         }

//                         let uid = connection.get_uid();
//                         let remote = connection.get_remote();
//                         let local = connection.get_local();
//                         let result = connection
//                             .close(10000,
//                                    Err(Error::new(ErrorKind::Other, "Normal"))).await;
//                         println!("Quic client close, uid: {:?}, remote: {:?}, local: {:?}, result: {:?}",
//                                  uid,
//                                  remote,
//                                  local,
//                                  result);

//                         thread::sleep(Duration::from_millis(1000000000));
//                     }
//                 }
//             });

//             thread::sleep(Duration::from_millis(10000000));
//         }
//     }
// }

// #[test]
// fn test_client_rebind() {
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_udp_rt"), None);
//     let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_quic_rt"), None);

//     let listener = QuicListener::new(vec![quic_rt],
//                                      "./tests/quic.com.crt",
//                                      "./tests/quic.com.key",
//                                      ClientCertVerifyLevel::Ignore,
//                                      Default::default(),
//                                      65535,
//                                      65535,
//                                      100000,
//                                      None,
//                                      Arc::new(TestService),
//                                      1)
//         .expect("Create quic listener failed");
//     let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

//     //用于Quic的udp连接监听器，有且只允许有一个运行时
//     match UdpTerminal::bind(addrs,
//                             udp_rt,
//                             8 * 1024 * 1024,
//                             8 * 1024 * 1024,
//                             0xffff,
//                             0xffff,
//                             Box::new(listener)) {
//         Err(e) => {
//             println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
//         },
//         Ok(_) => {
//             println!("===> Socket Listener Bind Ipv4 Address Ok");

//             let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_udp_rt"), None);
//             let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_quic_rt"), None);
//             let client_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_rt"), None);

//             let mut transport_config = TransportConfig::default();
//             transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//             let client = QuicClient::new("127.0.0.1:5000".parse().unwrap(),
//                                          udp_rt,
//                                          vec![quic_rt],
//                                          ServerCertVerifyLevel::Custom(Arc::new(TestServerCertVerifier::new())),
//                                          EndpointConfig::default(),
//                                          65535,
//                                          65535,
//                                          8 * 1024 * 1024,
//                                          8 * 1024 * 1024,
//                                          65535,
//                                          65535,
//                                          Some(Arc::new(transport_config)),
//                                          None,
//                                          5000,
//                                          1)
//                 .unwrap();

//             let pem = parse(read("./tests/quic.com.pub").unwrap()).unwrap();
//             println!("!!!!!!public key: {:?}", parse_ber(pem.contents()));

//             client_rt.spawn(async move {
//                 let now = Instant::now();
//                 match client.connect("127.0.0.1:38080".parse().unwrap(),
//                                      "0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff",
//                                      None,
//                                      None).await {
//                     Err(e) => {
//                         println!("!!!!!!Quic client connect failed, reason: {:?}", e);
//                     },
//                     Ok(connection) => {
//                         println!("!!!!!!Quic client connect ok, uid: {:?}, remote: {:?}, local: {:?}, time: {:?}",
//                                  connection.get_uid(),
//                                  connection.get_remote(),
//                                  connection.get_local(),
//                                  now.elapsed());

//                         for index in 0..3 {
//                             for index in 0..10 {
//                                 let now = Instant::now();
//                                 let main_stream_id = connection.get_handle().get_main_stream_id().unwrap().clone();
//                                 if let Err(e) = connection.write(main_stream_id, [b"Hello World ", index.to_string().as_bytes()].concat()) {
//                                     println!("Quic client send failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                                              connection.get_uid(),
//                                              connection.get_remote(),
//                                              connection.get_local(),
//                                              main_stream_id,
//                                              e);
//                                     break;
//                                 }

//                                 if let Some(resp) = connection.read(&main_stream_id).await {
//                                     println!("Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, bin: {:?}",
//                                              connection.get_uid(),
//                                              connection.get_remote(),
//                                              connection.get_local(),
//                                              main_stream_id,
//                                              String::from_utf8(resp.to_vec()));
//                                 }
//                                 println!("!!!!!!roll time: {:?}", now.elapsed());
//                             }

//                             thread::sleep(Duration::from_millis(1000));

//                             if index < 2 {
//                                 if let Err(e) = client.rebind(("127.0.0.1:600".to_string() + index.to_string().as_str()).parse().unwrap(),
//                                                               8 * 1024 * 1024,
//                                                               8 * 1024 * 1024,
//                                                               65535,
//                                                               65535,
//                                                               Err(Error::new(ErrorKind::Other, "Require swap udp"))) {
//                                     panic!("!!!!!!Rebind udp failed, index: {:?}, reason: {:?}", index, e);
//                                 } else {
//                                     println!("!!!!!!Rebind udp ok, index: {:?}", index);
//                                 }
//                             }
//                         }

//                         let uid = connection.get_uid();
//                         let remote = connection.get_remote();
//                         let local = connection.get_local();
//                         let latency = connection.get_latency();
//                         let result = connection
//                             .close(10000,
//                                    Err(Error::new(ErrorKind::Other, "Normal"))).await;
//                         println!("Quic client close, uid: {:?}, remote: {:?}, local: {:?}, latency: {:?}, result: {:?}",
//                                  uid,
//                                  remote,
//                                  local,
//                                  latency,
//                                  result);

//                         thread::sleep(Duration::from_millis(1000000000));
//                     }
//                 }
//             });

//             thread::sleep(Duration::from_millis(10000000));
//         }
//     }
// }

// struct TestServiceByServerOpenStream;

// impl QuicAsyncService for TestServiceByServerOpenStream {
//     fn handle_connected(&self,
//                         handle: SocketHandle,
//                         result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Connect Quic failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          e);
//                 return;
//             }
//             println!("===> Connect Quic ok, uid: {:?}, remote: {:?}, local: {:?}, is_0rtt: {:?}, main_stream_id: {:?}",
//                      handle.get_uid(),
//                      handle.get_remote(),
//                      handle.get_local(),
//                      handle.is_0rtt(),
//                      handle.get_main_stream_id());

//             let handle_copy = handle.clone();
//             thread::spawn(move || {
//                 //从服务端打开多个扩展流
//                 thread::sleep(Duration::from_millis(1000));
//                 let handle_clone = handle_copy.clone();
//                 handle_copy.spawn(async move {
//                     for index in 0..1 {
//                         match handle_clone.open_expanding_stream(Dir::Bi).await {
//                             Err(e) => {
//                                 panic!("!!!> Open expanding stream failed, index: {:?}, reason: {:?}",
//                                        index,
//                                        e);
//                             },
//                             Ok(stream_id) => {
//                                 if let Err(e) = handle_clone.write_ready(stream_id, b"Hello Client") {
//                                     panic!("!!!> Write stream failed, stream_id: {:?}, reason: {:?}",
//                                            stream_id,
//                                            e);
//                                 }
//                             },
//                         }
//                     }
//                 }.boxed_local());
//             });

//             //开始所有流的首次读
//             for stream_id in handle.get_stream_ids() {
//                 handle
//                     .set_ready(stream_id,
//                                QuicSocketReady::ReadWrite);
//             }
//         }.boxed_local()
//     }

//     fn handle_opened_expanding_stream(&self,
//                                       handle: SocketHandle,
//                                       stream_id: StreamId,
//                                       stream_type: Dir,
//                                       result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Open expanding stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, stream_type: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          stream_type,
//                          e);
//             } else {
//                 handle
//                     .set_ready(stream_id,
//                                QuicSocketReady::ReadWrite); //开始首次读写
//             }
//         }.boxed_local()
//     }

//     fn handle_readed(&self,
//                      handle: SocketHandle,
//                      stream_id: StreamId,
//                      result: Result<usize>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Socket read failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          e);
//                 return;
//             }

//             let mut ready_len = 0;
//             let remaining = if let Some(len) = handle.read_buffer_remaining(handle.get_main_stream_id().unwrap()) {
//                 len
//             } else {
//                 return;
//             };

//             if remaining == 0 {
//                 //当前读缓冲中没有数据，则异步准备读取数据
//                 println!("!!!!!!readed, read ready start, len: 0");
//                 ready_len = match handle.read_ready(&stream_id, 0) {
//                     Err(len) => len,
//                     Ok(value) => {
//                         println!("!!!!!!wait read_ready");
//                         let r = value.await;
//                         println!("!!!!!!wakeup read_ready, len: {}", r);
//                         r
//                     },
//                 };

//                 if ready_len == 0 {
//                     //当前连接已关闭，则立即退出
//                     return;
//                 }
//             }

//             if let Some(buf) = handle.get_read_buffer(&stream_id).as_ref().unwrap().lock().as_mut() {
//                 let bin = b"Hello World!";
//                 let _ = handle.write_ready(stream_id, bin);
//             }
//         }.boxed_local()
//     }

//     fn handle_writed(&self,
//                      handle: SocketHandle,
//                      stream_id: StreamId,
//                      result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Socket write failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          e);
//                 return;
//             }
//         }.boxed_local()
//     }

//     fn handle_closed(&self,
//                      handle: SocketHandle,
//                      stream_id: Option<StreamId>,
//                      code: u32,
//                      result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             println!("===> Socket closed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, code: {:?}, reason: {:?}",
//                      handle.get_uid(),
//                      handle.get_remote(),
//                      handle.get_local(),
//                      stream_id,
//                      code,
//                      result);
//         }.boxed_local()
//     }

//     /// 异步处理已超时
//     fn handle_timeouted(&self,
//                         handle: SocketHandle,
//                         result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()> {
//         async move {

//         }.boxed_local()
//     }
// }

// #[test]
// fn test_client_with_server_open_stream() {
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_udp_rt"), None);
//     let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_quic_rt"), None);

//     let mut transport_config = TransportConfig::default();
//     transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//     let listener = QuicListener::new(vec![quic_rt],
//                                      "./tests/quic.com.crt",
//                                      "./tests/quic.com.key",
//                                      ClientCertVerifyLevel::Ignore,
//                                      Default::default(),
//                                      65535,
//                                      65535,
//                                      100000,
//                                      Some(Arc::new(transport_config)),
//                                      Arc::new(TestServiceByServerOpenStream),
//                                      1)
//         .expect("Create quic listener failed");
//     let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

//     //用于Quic的udp连接监听器，有且只允许有一个运行时
//     match UdpTerminal::bind(addrs,
//                             udp_rt,
//                             8 * 1024 * 1024,
//                             8 * 1024 * 1024,
//                             0xffff,
//                             0xffff,
//                             Box::new(listener)) {
//         Err(e) => {
//             println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
//         },
//         Ok(_) => {
//             println!("===> Socket Listener Bind Ipv4 Address Ok");

//             let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_udp_rt"), None);
//             let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_quic_rt"), None);
//             let client_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_rt"), None);

//             let mut transport_config = TransportConfig::default();
//             transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//             let client = QuicClient::new("127.0.0.1:5000".parse().unwrap(),
//                                          udp_rt,
//                                          vec![quic_rt],
//                                          ServerCertVerifyLevel::Custom(Arc::new(TestServerCertVerifier::new())),
//                                          EndpointConfig::default(),
//                                          65535,
//                                          65535,
//                                          8 * 1024 * 1024,
//                                          8 * 1024 * 1024,
//                                          65535,
//                                          65535,
//                                          Some(Arc::new(transport_config)),
//                                          None,
//                                          5000,
//                                          1)
//                 .unwrap();

//             let pem = parse(read("./tests/quic.com.pub").unwrap()).unwrap();
//             println!("!!!!!!public key: {:?}", parse_ber(pem.contents()));

//             client_rt.spawn(async move {
//                 let now = Instant::now();
//                 match client.connect("127.0.0.1:38080".parse().unwrap(),
//                                      "0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff",
//                                      None,
//                                      None).await {
//                     Err(e) => {
//                         println!("!!!!!!Quic client connect failed, reason: {:?}", e);
//                     },
//                     Ok(connection) => {
//                         println!("!!!!!!Quic client connect ok, uid: {:?}, remote: {:?}, local: {:?}, time: {:?}",
//                                  connection.get_uid(),
//                                  connection.get_remote(),
//                                  connection.get_local(),
//                                  now.elapsed());

//                         //主流通讯
//                         if let Some(main_stream_id) = connection.get_handle().get_main_stream_id() {
//                             let now = Instant::now();
//                             if let Err(e) = connection.write(main_stream_id.clone(),
//                                                              [b"Hello World ", main_stream_id.to_string().as_bytes()].concat()) {
//                                 println!("!!!!!!Quic client send failed, uid: {:?}, remote: {:?}, local: {:?}, main_stream_id: {:?}, reason: {:?}",
//                                          connection.get_uid(),
//                                          connection.get_remote(),
//                                          connection.get_local(),
//                                          main_stream_id,
//                                          e);
//                             } else {
//                                 if let Some(resp) = connection.read(&main_stream_id).await {
//                                     println!("!!!!!!Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, main_stream_id: {:?}, time: {:?}, bin: {:?}",
//                                              connection.get_uid(),
//                                              connection.get_remote(),
//                                              connection.get_local(),
//                                              main_stream_id,
//                                              now.elapsed(),
//                                              String::from_utf8(resp.to_vec()));
//                                 }
//                             }
//                         }

//                         //每个流接收服务端发送的消息
//                         let mut count = 0;
//                         while count == 0 {
//                             for stream_id in connection.get_handle().get_stream_ids() {
//                                 if stream_id == StreamId(0) {
//                                     continue;
//                                 }

//                                 let now = Instant::now();
//                                 if let Some(resp) = connection.read(&stream_id).await {
//                                     println!("!!!!!!Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, time: {:?}, bin: {:?}",
//                                              connection.get_uid(),
//                                              connection.get_remote(),
//                                              connection.get_local(),
//                                              stream_id,
//                                              now.elapsed(),
//                                              String::from_utf8(resp.to_vec()));
//                                     count += 1;
//                                 }
//                             }
//                         }
//                         thread::sleep(Duration::from_millis(1000));

//                         let uid = connection.get_uid();
//                         let remote = connection.get_remote();
//                         let local = connection.get_local();
//                         let latency = connection.get_latency();
//                         let result = connection
//                             .close(10000,
//                                    Err(Error::new(ErrorKind::Other, "Normal"))).await;
//                         println!("Quic client close, uid: {:?}, remote: {:?}, local: {:?}, latency: {:?}, result: {:?}",
//                                  uid,
//                                  remote,
//                                  local,
//                                  latency,
//                                  result);

//                         thread::sleep(Duration::from_millis(10000000));
//                     }
//                 }
//             });

//             thread::sleep(Duration::from_millis(10000000));
//         }
//     }
// }

// struct TestServiceByMultiStreams;

// impl QuicAsyncService for TestServiceByMultiStreams {
//     fn handle_connected(&self,
//                         handle: SocketHandle,
//                         result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Connect Quic failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          e);
//                 return;
//             }
//             println!("===> Connect Quic ok, uid: {:?}, remote: {:?}, local: {:?}, is_0rtt: {:?}, main_stream_id: {:?}",
//                      handle.get_uid(),
//                      handle.get_remote(),
//                      handle.get_local(),
//                      handle.is_0rtt(),
//                      handle.get_main_stream_id());

//             handle
//                 .set_ready(handle.get_main_stream_id().unwrap().clone(),
//                            QuicSocketReady::Readable); //开始首次读
//         }.boxed_local()
//     }

//     fn handle_opened_expanding_stream(&self,
//                                       handle: SocketHandle,
//                                       stream_id: StreamId,
//                                       stream_type: Dir,
//                                       result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Open expanding stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, stream_type: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          stream_type,
//                          e);
//             } else {
//                 handle
//                     .set_ready(stream_id,
//                                QuicSocketReady::Readable); //开始首次读
//             }
//         }.boxed_local()
//     }

//     fn handle_readed(&self,
//                      handle: SocketHandle,
//                      stream_id: StreamId,
//                      result: Result<usize>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Socket read failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          e);
//                 return;
//             }

//             let mut ready_len = 0;
//             let remaining = if let Some(len) = handle.read_buffer_remaining(handle.get_main_stream_id().unwrap()) {
//                 len
//             } else {
//                 return;
//             };

//             if remaining == 0 {
//                 //当前读缓冲中没有数据，则异步准备读取数据
//                 println!("!!!!!!readed, read ready start, len: 0");
//                 ready_len = match handle.read_ready(&stream_id, 0) {
//                     Err(len) => len,
//                     Ok(value) => {
//                         println!("!!!!!!wait read_ready");
//                         let r = value.await;
//                         println!("!!!!!!wakeup read_ready, len: {}", r);
//                         r
//                     },
//                 };

//                 if ready_len == 0 {
//                     //当前连接已关闭，则立即退出
//                     return;
//                 }
//             }

//             if let Some(buf) = handle.get_read_buffer(&stream_id).as_ref().unwrap().lock().as_mut() {
//                 let bin = b"Hello World!";
//                 let _ = handle.write_ready(stream_id, bin);
//             }
//         }.boxed_local()
//     }

//     fn handle_writed(&self,
//                      handle: SocketHandle,
//                      stream_id: StreamId,
//                      result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             if let Err(e) = result {
//                 println!("===> Socket write failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                          handle.get_uid(),
//                          handle.get_remote(),
//                          handle.get_local(),
//                          stream_id,
//                          e);
//                 return;
//             }
//         }.boxed_local()
//     }

//     fn handle_closed(&self,
//                      handle: SocketHandle,
//                      stream_id: Option<StreamId>,
//                      code: u32,
//                      result: Result<()>) -> LocalBoxFuture<'static, ()> {
//         async move {
//             println!("===> Socket closed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, code: {:?}, reason: {:?}",
//                      handle.get_uid(),
//                      handle.get_remote(),
//                      handle.get_local(),
//                      stream_id,
//                      code,
//                      result);
//         }.boxed_local()
//     }

//     /// 异步处理已超时
//     fn handle_timeouted(&self,
//                         handle: SocketHandle,
//                         result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()> {
//         async move {

//         }.boxed_local()
//     }
// }

// #[test]
// fn test_client_by_multi_stream() {
//     //启动日志系统
//     env_logger::builder().format_timestamp_millis().init();

//     let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_udp_rt"), None);
//     let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("server_quic_rt"), None);

//     let mut transport_config = TransportConfig::default();
//     transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//     let listener = QuicListener::new(vec![quic_rt],
//                                      "./tests/quic.com.crt",
//                                      "./tests/quic.com.key",
//                                      ClientCertVerifyLevel::Ignore,
//                                      Default::default(),
//                                      65535,
//                                      65535,
//                                      100000,
//                                      Some(Arc::new(transport_config)),
//                                      Arc::new(TestServiceByMultiStreams),
//                                      1)
//         .expect("Create quic listener failed");
//     let addrs = SocketAddr::new(IpAddr::from_str("0.0.0.0").unwrap(), 38080);

//     //用于Quic的udp连接监听器，有且只允许有一个运行时
//     match UdpTerminal::bind(addrs,
//                             udp_rt,
//                             8 * 1024 * 1024,
//                             8 * 1024 * 1024,
//                             0xffff,
//                             0xffff,
//                             Box::new(listener)) {
//         Err(e) => {
//             println!("!!!> Socket Listener Bind Ipv4 Address Error, reason: {:?}", e);
//         },
//         Ok(_) => {
//             println!("===> Socket Listener Bind Ipv4 Address Ok");

//             let udp_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_udp_rt"), None);
//             let quic_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_quic_rt"), None);
//             let client_rt = AsyncRuntimeBuilder::default_local_thread(Some("client_rt"), None);

//             let mut transport_config = TransportConfig::default();
//             transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
//             let client = QuicClient::new("127.0.0.1:5000".parse().unwrap(),
//                                          udp_rt,
//                                          vec![quic_rt],
//                                          ServerCertVerifyLevel::Custom(Arc::new(TestServerCertVerifier::new())),
//                                          EndpointConfig::default(),
//                                          65535,
//                                          65535,
//                                          8 * 1024 * 1024,
//                                          8 * 1024 * 1024,
//                                          65535,
//                                          65535,
//                                          Some(Arc::new(transport_config)),
//                                          None,
//                                          5000,
//                                          1)
//                 .unwrap();

//             let pem = parse(read("./tests/quic.com.pub").unwrap()).unwrap();
//             println!("!!!!!!public key: {:?}", parse_ber(pem.contents()));

//             client_rt.spawn(async move {
//                 let now = Instant::now();
//                 match client.connect("127.0.0.1:38080".parse().unwrap(),
//                                      "0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff.0f.16.58.ff.00.ab.08.ff",
//                                      None,
//                                      None).await {
//                     Err(e) => {
//                         println!("!!!!!!Quic client connect failed, reason: {:?}", e);
//                     },
//                     Ok(connection) => {
//                         println!("!!!!!!Quic client connect ok, uid: {:?}, remote: {:?}, local: {:?}, time: {:?}",
//                                  connection.get_uid(),
//                                  connection.get_remote(),
//                                  connection.get_local(),
//                                  now.elapsed());

//                         //创建多个扩展流
//                         for _ in 0..1023 {
//                             if let Err(e) = connection
//                                 .get_handle()
//                                 .open_expanding_stream(Dir::Bi)
//                                 .await {
//                                 panic!("!!!!!!Quic open expanding stream failed, reason: {:?}", e);
//                             }
//                         }

//                         //每个流进行通讯
//                         for _ in 0..3 {
//                             for stream_id in connection.get_handle().get_stream_ids() {
//                                 let now = Instant::now();
//                                 if let Err(e) = connection.write(stream_id, [b"Hello World ", stream_id.to_string().as_bytes()].concat()) {
//                                     println!("!!!!!!Quic client send failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
//                                              connection.get_uid(),
//                                              connection.get_remote(),
//                                              connection.get_local(),
//                                              stream_id,
//                                              e);
//                                     break;
//                                 }

//                                 if let Some(resp) = connection.read(&stream_id).await {
//                                     let time = now.elapsed();
//                                     if time >= Duration::new(0, 50000000) {
//                                         println!("!!!!!!Quic client receive ok, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, time: {:?}, bin: {:?}",
//                                                  connection.get_uid(),
//                                                  connection.get_remote(),
//                                                  connection.get_local(),
//                                                  stream_id,
//                                                  time,
//                                                  String::from_utf8(resp.to_vec()));
//                                     }
//                                 }
//                             }
//                             thread::sleep(Duration::from_millis(1000));
//                         }

//                         let uid = connection.get_uid();
//                         let remote = connection.get_remote();
//                         let local = connection.get_local();
//                         let latency = connection.get_latency();
//                         let result = connection
//                             .close(10000,
//                                    Err(Error::new(ErrorKind::Other, "Normal"))).await;
//                         println!("Quic client close, uid: {:?}, remote: {:?}, local: {:?}, latency: {:?}, result: {:?}",
//                                  uid,
//                                  remote,
//                                  local,
//                                  latency,
//                                  result);

//                         thread::sleep(Duration::from_millis(1000000000));
//                     }
//                 }
//             });

//             thread::sleep(Duration::from_millis(10000000));
//         }
//     }
// }

// pub struct TestServerCertVerifier;

// impl rustls::client::ServerCertVerifier for TestServerCertVerifier {
//     fn verify_server_cert(&self,
//                           end_entity: &rustls::Certificate,
//                           intermediates: &[rustls::Certificate],
//                           server_name: &rustls::ServerName,
//                           scts: &mut dyn Iterator<Item=&[u8]>,
//                           ocsp_response: &[u8],
//                           now: SystemTime) -> std::result::Result<rustls::client::ServerCertVerified, rustls::Error> {
//         use std::fs::read;
//         println!("!!!!!!end_entity: {:?}", end_entity.0.as_slice());
//         let mut pem = Pem {
//             label: "".to_string(),
//             contents: end_entity.0.clone(),
//         };
//         println!("!!!!!!cert public key: {:?}", pem.parse_x509().unwrap().public_key().parsed());
//         println!("!!!!!!server_name: {:?}", server_name);

//         Ok(rustls::client::ServerCertVerified::assertion())
//     }
// }

// impl TestServerCertVerifier {
//     pub fn new() -> Self {
//         TestServerCertVerifier
//     }
// }




