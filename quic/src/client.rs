use std::fs;
use std::iter;
use std::rc::Rc;
use std::sync::Arc;
use std::net::SocketAddr;
use std::cell::UnsafeCell;
use std::convert::TryInto;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use std::io::{BufRead, Result, Error, ErrorKind, Cursor};

use futures::future::{FutureExt, LocalBoxFuture};
use parking_lot::RwLock;
use quinn_proto::{crypto, Endpoint, EndpointConfig, ClientConfig, TransportConfig, EndpointEvent, DatagramEvent, ConnectionEvent, ConnectionHandle, Transmit, Connection, StreamId, Dir};
use crossbeam_channel::{Sender, Receiver, unbounded};
use flume::{Sender as AsyncSender, Receiver as AsyncReceiver, bounded};
use dashmap::DashMap;
use bytes::{Buf, Bytes, BytesMut};
use futures::AsyncReadExt;
use futures::task::SpawnExt;
use rustls;
use log::{debug, error};

use pi_hash::XHashMap;
use pi_cancel_timer::Timer;
use pi_async::{lock::spin_lock::SpinLock,
               rt::{AsyncRuntime, serial::AsyncValueNonBlocking,
                    serial_local_thread::LocalTaskRuntime}};
use udp::{Socket, AsyncService, SocketHandle, TaskResult,
          terminal::UdpTerminal};

use crate::{AsyncService as QuicAsyncService, SocketHandle as QuicSocketHandle, SocketEvent,
            connect::QuicSocket,
            connect_pool::{EndPointPoller, QuicSocketPool},
            utils::{QuicSocketReady, QuicClientTimerEvent, load_certs_file, load_cert, load_key_file, load_key}, QuicEvent};


/// 默认的读取块大小，单位字节
const DEFAULT_READ_BLOCK_LEN: usize = 4096;

/// 默认的写入块大小，单位字节
const DEFAULT_WRITE_BLOCK_LEN: usize = 4096;

/// 最小的Quic已读读缓冲大小限制，单位字节
const MIN_READED_READ_BUF_SIZE_LIMIT: usize = DEFAULT_READ_BLOCK_LEN;

/// 最小的Quic已读写缓冲大小限制，单位字节
const MIN_READED_WRITE_BUF_SIZE_LIMIT: usize = DEFAULT_WRITE_BLOCK_LEN;

/// 默认的Quic已读读缓冲大小限制，单位字节
const DEAFULT_READED_READ_BUF_SIZE_LIMIT: usize = 64 * 1024;

/// 默认的Quic已读写缓冲大小限制，单位字节
const DEAFULT_READED_WRITE_BUF_SIZE_LIMIT: usize = 64 * 1024;

///
/// Quic客户端
///
pub struct QuicClient(Arc<InnerQuicClient>);

unsafe impl Send for QuicClient {}
unsafe impl Sync for QuicClient {}

impl Clone for QuicClient {
    fn clone(&self) -> Self {
        QuicClient(self.0.clone())
    }
}

impl AsyncService for QuicClient {
    fn bind_runtime(&mut self, _rt: LocalTaskRuntime<()>) {

    }

    fn handle_binded(&self,
                     _udp_handle: SocketHandle,
                     _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_received(&self,
                       handle: SocketHandle,
                       result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()> {
        let quic_client = self.clone();
        async move {
            match result {
                Err(e) => {
                    //Udp读数据失败
                    handle.close(Err(Error::new(ErrorKind::Other,
                                                format!("Read udp failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                        handle.get_uid(),
                                                        handle.get_remote(),
                                                        handle.get_local(),
                                                        e))));
                },
                Ok((bin, active_peer)) => {
                    //Udp读数据成功
                    unsafe {
                        let bytes: BytesMut = bin.as_slice().into();
                        if let Some((connection_handle, DatagramEvent::ConnectionEvent(event))) =
                            (&mut *quic_client.0.end_point.get()).handle(quic_client.0.clock,
                                                                         peer(active_peer, handle.get_remote()),
                                                                         Some(handle.get_local().ip()),
                                                                         None,
                                                                         bytes) {
                            //只处理Socket事件
                            if let Some(event_sent) = &quic_client
                                .0
                                .event_sents.get(&(connection_handle.0 % quic_client.0.event_sents.len())) {
                                //向连接所在连接池发送Socket事件
                                event_sent
                                    .send(QuicEvent::ConnectionReceived(connection_handle, event));
                            }
                        }
                    }
                },
            }
        }.boxed_local()
    }

    fn handle_sended(&self,
                     _handle: SocketHandle,
                     _result: Result<usize>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_closed(&self,
                     _handle: SocketHandle,
                     _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }
}

// 获取对端地址
#[inline]
fn peer(active_peer: Option<SocketAddr>,
        binded_peer: Option<&SocketAddr>) -> SocketAddr {
    if let Some(peer) = active_peer {
        peer
    } else {
        if let Some(peer) = binded_peer {
            peer.clone()
        } else {
            panic!("Take peer address failed, reason: invalid peer");
        }
    }
}

impl EndPointPoller for QuicClient {
    fn poll(&self,
            socket: &QuicSocket,
            handle: ConnectionHandle,
            events: VecDeque<EndpointEvent>) {
        let client = self.clone();
        socket
            .get_udp_handle()
            .spawn(async move {
                handle_endpoint_events(client,
                                       handle,
                                       events);
                TaskResult::Continue
            }.boxed_local());
    }
}

// 处理Quic客户端端点事件
fn handle_endpoint_events(quic_client: QuicClient,
                          connection_handle: ConnectionHandle,
                          endpoint_events: VecDeque<EndpointEvent>) {
    //处理Quic客户端连接的端点事件
    for endpoint_event in endpoint_events {
        if endpoint_event.is_drained() {
            //TODO 当前连接已被清理...
            continue;
        }

        unsafe {
            let end_point = &mut *quic_client.0.end_point.get();
            let index = connection_handle.0 % quic_client.0.event_sents.len(); //路由到指定连接池
            if let Some(event) = end_point.handle_event(connection_handle, endpoint_event) {
                if let Some(sender) = quic_client.0.event_sents.get(&index) {
                    //向连接所在连接池发送Socket事件
                    sender
                        .send(QuicEvent::ConnectionReceived(connection_handle, event));

                    while let Some(transmit) = end_point.poll_transmit() {
                        //向连接所在连接池发送Socket发送事件
                        sender
                            .send(QuicEvent::ConnectionSend(connection_handle, transmit));
                    }
                }
            }
        }
    }
}

/*
* Quic客户端同步方法
*/
impl QuicClient {
    /// 构建一个不授信的Quic客户端
    pub fn new(client_address: SocketAddr,
               udp_client_runtime: LocalTaskRuntime<()>,
               runtimes: Vec<LocalTaskRuntime<()>>,
               verify_level: ServerCertVerifyLevel,
               config: EndpointConfig,
               readed_read_size_limit: usize,
               readed_write_size_limit: usize,
               udp_recv_buffer_size: usize,
               udp_send_buffer_size: usize,
               udp_read_packet_size: usize,
               udp_write_packet_size: usize,
               transport_config: Option<Arc<TransportConfig>>,
               listener: Option<Arc<dyn QuicAsyncService>>,
               connect_timeout: usize,
               timeout: u64) -> Result<Self> {
        Self::with_cert_and_key(client_address,
                                udp_client_runtime,
                                runtimes,
                                ClientCreditLevel::UnCredit,
                                verify_level,
                                config,
                                readed_read_size_limit,
                                readed_write_size_limit,
                                udp_recv_buffer_size,
                                udp_send_buffer_size,
                                udp_read_packet_size,
                                udp_write_packet_size,
                                transport_config,
                                listener,
                                connect_timeout,
                                timeout)
    }

    /// 构建一个指定授信级别的Quic客户端
    pub fn with_cert_and_key(client_address: SocketAddr,
                             udp_client_runtime: LocalTaskRuntime<()>,
                             runtimes: Vec<LocalTaskRuntime<()>>,
                             credit_level: ClientCreditLevel,
                             verify_level: ServerCertVerifyLevel,
                             config: EndpointConfig,
                             readed_read_size_limit: usize,
                             readed_write_size_limit: usize,
                             udp_recv_buffer_size: usize,
                             udp_send_buffer_size: usize,
                             udp_read_packet_size: usize,
                             udp_write_packet_size: usize,
                             transport_config: Option<Arc<TransportConfig>>,
                             listener: Option<Arc<dyn QuicAsyncService>>,
                             connect_timeout: usize,
                             timeout: u64) -> Result<Self> {
        if runtimes.is_empty() {
            //连接运行时为空，则立即返回错误原因
            return Err(Error::new(ErrorKind::Other,
                                  format!("Create quic client failed, reason: invalid runtimes")));
        }

        let readed_read_size_limit = if readed_read_size_limit < MIN_READED_READ_BUF_SIZE_LIMIT {
            //Quic已读读缓冲大小限制过小
            DEAFULT_READED_READ_BUF_SIZE_LIMIT
        } else {
            readed_read_size_limit
        };

        let readed_write_size_limit = if readed_write_size_limit < MIN_READED_WRITE_BUF_SIZE_LIMIT {
            //Quic已读写缓冲大小限制过小
            DEAFULT_READED_WRITE_BUF_SIZE_LIMIT
        } else {
            readed_write_size_limit
        };

        //启动Udp终端，有且只允许有一个运行时
        let (udp_terminal, init_udp_terminal) = UdpTerminal::bind_without_service(client_address,
                                                                                  udp_client_runtime.clone(),
                                                                                  udp_recv_buffer_size,
                                                                                  udp_send_buffer_size,
                                                                                  udp_read_packet_size,
                                                                                  udp_write_packet_size)?;

        let client_auth = match credit_level {
            ClientCreditLevel::UnCredit => {
                //不需要授信
                None
            },
            credit => {
                //需要授信
                match credit {
                    ClientCreditLevel::CreditFile(client_cert_path, client_key_path) => {
                        //使用指定的客户端证书路径和客户端密钥路径
                        Some((load_certs_file(client_cert_path)?,
                              load_key_file(client_key_path)?))
                    },
                    ClientCreditLevel::CreditBytes(client_cert_bytes, client_key_bytes) => {
                        //使用指定的客户端证书数据和客户端密钥数据
                        Some((vec![load_cert(client_cert_bytes)],
                              load_key(client_key_bytes)))
                    },
                    _ => unimplemented!(),
                }
            },
        };

        //构建客户端配置
        let builder = rustls::ClientConfig::builder()
            .with_safe_defaults();
        let client_crypto = match verify_level {
            ServerCertVerifyLevel::Ignore => {
                //不需要严格验证服务端证书
                match client_auth {
                    None => {
                        //不需要授信客户端
                        builder
                            .with_custom_certificate_verifier(Arc::new(InsecureServerCertVerifier))
                            .with_no_client_auth()
                    },
                    Some((client_certs, client_key)) => {
                        //需要授信客户端
                        match builder
                            .with_custom_certificate_verifier(Arc::new(InsecureServerCertVerifier))
                            .with_single_cert(client_certs, client_key) {
                            Err(e) => {
                                return Err(Error::new(ErrorKind::Other,
                                                      format!("Create quic client failed, reason: {:?}",
                                                              e)));
                            },
                            Ok(crypto) => {
                                crypto
                            },
                        }
                    },
                }
            },
            ServerCertVerifyLevel::Custom(verifier) => {
                //需要验证服务端证书，使用指定的自定义验证器
                match client_auth {
                    None => {
                        //不需要授信客户端
                        builder
                            .with_custom_certificate_verifier(verifier)
                            .with_no_client_auth()
                    },
                    Some((client_certs, client_key)) => {
                        //需要授信客户端
                        match builder
                            .with_custom_certificate_verifier(verifier)
                            .with_single_cert(client_certs, client_key) {
                            Err(e) => {
                                return Err(Error::new(ErrorKind::Other,
                                                      format!("Create quic client failed, reason: {:?}",
                                                              e)));
                            },
                            Ok(crypto) => {
                                crypto
                            },
                        }
                    },
                }
            },
            ca_cert => {
                //需要严格验证服务端证书
                let bytes = match ca_cert {
                    ServerCertVerifyLevel::CaCertFile(path) => {
                        //使用指定的CA证书文件
                        fs::read(path)?
                    },
                    ServerCertVerifyLevel::CaCertBytes(bin) => {
                        //使用指定的CA证书数据
                        bin
                    },
                    _ => unimplemented!(),
                };

                let mut roots = rustls::RootCertStore::empty();
                if let Err(e) = roots.add(&rustls::Certificate(bytes)) {
                    //增加根证书失败，则立即返回错误原因
                    return Err(Error::new(ErrorKind::Other,
                                          format!("Create quic client failed, reason: {:?}",
                                                  e)));
                }

                //配置默认客户端安全参数，并构建默认客户端配置
                match client_auth {
                    None => {
                        //不需要授信客户端
                        builder
                            .with_root_certificates(roots)
                            .with_no_client_auth()
                    },
                    Some((client_certs, client_key)) => {
                        //需要授信客户端
                        match builder
                            .with_root_certificates(roots)
                            .with_single_cert(client_certs, client_key) {
                            Err(e) => {
                                return Err(Error::new(ErrorKind::Other,
                                                      format!("Create quic client failed, reason: {:?}",
                                                              e)));
                            },
                            Ok(crypto) => {
                                crypto
                            },
                        }
                    },
                }
            },
        };
        let mut default_config = ClientConfig::new(Arc::new(client_crypto));
        if let Some(transport) = transport_config {
            //使用外部自定义传输配置
            default_config.transport = transport;
        }

        //创建Quic状态机
        let end_point = Rc::new(UnsafeCell::new(Endpoint::new(Arc::new(config), None)));
        let connections = DashMap::new();
        let connect_events = DashMap::new();
        let close_events = DashMap::new();
        let timer = SpinLock::new(Timer::<QuicClientTimerEvent, 100, 60, 3>::default());

        //构建Quic客户端
        let clock = Instant::now();
        let runtimes_len = runtimes.len();
        let mut event_sents = XHashMap::default();
        let mut event_pairs = VecDeque::with_capacity(runtimes_len);
        let mut router = Vec::with_capacity(runtimes_len);
        for pool_id in 0..runtimes_len {
            let (event_send, event_recv) = unbounded();
            router.push(event_send.clone());
            event_sents.insert(pool_id, event_send.clone());
            event_pairs.push_back((event_send, event_recv));
        }
        let service = Arc::new(QuicClientService {
            client: Arc::new(UnsafeCell::new(None)),
        });
        let inner = InnerQuicClient {
            udp_terminal_runtime: udp_client_runtime.clone(),
            udp_terminal: RwLock::new(Some(udp_terminal)),
            runtimes: runtimes.clone(),
            event_sents,
            readed_read_size_limit,
            readed_write_size_limit,
            end_point,
            default_config,
            clock,
            connections,
            service: service.clone(),
            router,
            connect_events,
            close_events,
            listener,
            timer,
            connect_timeout,
        };
        let client = QuicClient(Arc::new(inner));
        unsafe {
            *client.0.service.client.get() = Some(client.clone());
        }

        //初始化Udp终端
        init_udp_terminal(Box::new(client.clone()));

        //启动客户端定时器
        udp_client_runtime.spawn(poll_timer(client.clone()));

        //构建并启动客户端连接池
        let mut pool_id = 0;
        for rt in runtimes {
            let (event_send, event_recv) = event_pairs.pop_front().unwrap();

            //创建并运行连接池
            let connect_pool = QuicSocketPool::new(pool_id,
                                                   client.clone(),
                                                   event_send,
                                                   event_recv,
                                                   service.clone(),
                                                   clock,
                                                   timeout);
            connect_pool.run(rt);
            pool_id += 1; //更新连接池唯一id
        }

        Ok(client)
    }

    /// 重新绑定当前Quic客户端的Udp终端到指定的本地地址
    pub fn rebind(&self,
                  local_udp_addr: SocketAddr,
                  udp_recv_buffer_size: usize,
                  udp_send_buffer_size: usize,
                  udp_read_packet_size: usize,
                  udp_write_packet_size: usize,
                  reason: Result<()>) -> Result<()> {
        //启动新的Udp终端，复用当前Udp终端的运行时
        let (new_udp_terminal, init_udp_terminal) = UdpTerminal::bind_without_service(local_udp_addr,
                                                                                      self.0.udp_terminal_runtime.clone(),
                                                                                      udp_recv_buffer_size,
                                                                                      udp_send_buffer_size,
                                                                                      udp_read_packet_size,
                                                                                      udp_write_packet_size)?;
        init_udp_terminal(Box::new(self.clone())); //初始化新的Udp终端
        self
            .0
            .udp_terminal_runtime
            .spawn(poll_timer(self.clone())); //在新的Udp终端上启动客户端定时器

        //获取新的Udp终端的连接句柄
        let udp_handle = new_udp_terminal.reconnect_to_peer()?;

        //替换Udp终端
        if let Some(old_udp_terminal) = self.0.udp_terminal.write().replace(new_udp_terminal) {
            //立即关闭Quic绑定的旧Udp终端
            old_udp_terminal.close(reason)?;

            for (_index, event_send) in self.0.event_sents.iter() {
                event_send.send(QuicEvent::RebindUdp(udp_handle.clone()));
            }
        }

        Ok(())
    }

    /// 判断指定令牌的连接是否已关闭
    pub fn is_closed(&self, uid: &usize) -> bool {
        self.0.connections.contains_key(uid)
    }
}

/*
* Quic客户端异步方法
*/
impl QuicClient {
    /// 异步连接到指定地址和主机名的服务端
    pub async fn connect(&self,
                         remote: SocketAddr,
                         hostname: &str,
                         config: Option<ClientConfig>,
                         timeout: Option<usize>) -> Result<QuicClientConnection> {
        let udp_connect_uid = self
            .0
            .udp_terminal
            .read()
            .as_ref()
            .unwrap()
            .get_uid();
        let connect_timeout = if let Some(connect_timeout) = timeout {
            //设置指定的连接超时时长
            if connect_timeout == 0 {
                //指定的连接超时时长过短，则使用默认的连接超时时长
                self.0.timer.lock().push(self.0.connect_timeout, QuicClientTimerEvent::UdpConnect(udp_connect_uid));
                self.0.connect_timeout
            } else {
                self.0.timer.lock().push(connect_timeout, QuicClientTimerEvent::UdpConnect(udp_connect_uid));
                connect_timeout
            }
        } else {
            //没有指定的连接超时时长，则使用默认的连接超时时长
            self.0.timer.lock().push(self.0.connect_timeout, QuicClientTimerEvent::UdpConnect(udp_connect_uid));
            self.0.connect_timeout
        };

        //连接指定的对端Udp
        let udp_handle = match self.0.udp_terminal.read().as_ref().unwrap().connect_to_peer(remote) {
            Err(e) => {
                //创建Udp连接失败，则立即返回错误原因
                return Err(e);
            },
            Ok(handle) => handle,
        };

        //连接指定的对端Udp成功
        let config = if let Some(cfg) = config {
            cfg
        } else {
            self.0.default_config.clone()
        };

        //异步建立指定地址的Quic连接，避免外部使用非serial运行时，出现Send约束错误
        let now = Instant::now();
        let client = self.clone();
        let hostname = hostname.to_string();
        let connect_value = AsyncValueNonBlocking::new();
        let connect_value_copy = connect_value.clone();
        self
            .0
            .udp_terminal
            .read()
            .as_ref()
            .unwrap()
            .spawn(async move {
                match unsafe { (&mut *client.0.end_point.get()).connect(config,
                                                                        remote,
                                                                        hostname.as_str()) } {
                    Err(e) => {
                        //建立Quic连接失败，则立即返回连接错误原因
                        connect_value_copy.set(Err(Error::new(ErrorKind::Other,
                                                              format!("Create quic connection failed, hostname: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                                      hostname,
                                                                      udp_handle.get_remote(),
                                                                      udp_handle.get_local(),
                                                                      e))));
                    },
                    Ok((connection_handle, connection)) => {
                        //建立Quic连接成功
                        let quic_connect_timeout = connect_timeout
                            .checked_sub(now.elapsed().as_millis() as usize)
                            .unwrap_or(0);
                        if quic_connect_timeout == 0 {
                            //没有剩余连接时长，则立即关闭当前Udp连接，并立即返回连接错误原因
                            connect_value_copy.set(Err(Error::new(ErrorKind::TimedOut, "Connect quic timeout")));
                        } else {
                            //还有剩余连接时长，则设置Quic连接的超时时长
                            client.0.timer.lock().push(quic_connect_timeout, QuicClientTimerEvent::QuicConnect(connection_handle.0));

                            //注册Quic连接事件表
                            client.0.connect_events.insert(connection_handle.0, (udp_connect_uid, connect_value_copy));

                            //将Quic Socket路由到连接池
                            let quic_socket = QuicSocket::new(udp_handle,
                                                              connection_handle,
                                                              connection,
                                                              client.0.readed_read_size_limit,
                                                              client.0.readed_write_size_limit,
                                                              client.0.clock);
                            quic_socket.enable_client(); //设置为客户端
                            let sender = &client.0.router[connection_handle.0 % client.0.router.len()];
                            sender.send(QuicEvent::Accepted(quic_socket));
                        }
                    },
                }

                TaskResult::Continue
            }.boxed_local());

        match connect_value.await {
            Err(e) => {
                //建立Udp连接失败，则立即返回错误原因
                Err(e)
            },
            Ok(socket_handle) => {
                //建立Udp连接成功，并返回Quic客户端连接
                let inner = InnerQuicClientConnection {
                    client: self.clone(),
                    handle: socket_handle,
                };

                let connection = QuicClientConnection(Arc::new(inner));
                self
                    .0
                    .connections
                    .insert(connection.get_connection_handle().0,
                            connection.clone());

                Ok(connection)
            },
        }
    }

    /// 用本地异步运行时，异步关闭指定内部连接句柄的Quic连接
    pub async fn close_connection(&self,
                                  connection_handle: ConnectionHandle,
                                  code: u32,
                                  reason: Result<()>) -> Result<()> {
        let uid = connection_handle.0;
        if let Some((_uid, connection)) = self.0.connections.remove(&uid) {
            //指定唯一id的Udp连接存在，则开始关闭
            let value = AsyncValueNonBlocking::new();
            self.0.close_events.insert(uid, value.clone()); //注册指定连接的关闭事件监听器

            if let Err(e) = connection.0.handle.close(code, reason) {
                //关闭连接失败，则立即返回错误原因
                self.0.close_events.remove(&uid); //注销指定连接的关闭事件监听器
                return Err(e);
            }

            //返回关闭结果
            value.await
        } else {
            //指定唯一id的Udp连接不存在，则忽略
            Ok(())
        }
    }
}

// 在客户端所在的Udp终端运行时中，异步推动定时器
fn poll_timer(client: QuicClient) -> LocalBoxFuture<'static, ()> {
    async move {
        let current_time = client.0.clock.elapsed().as_millis() as u64;
        if client.0.timer.lock().is_ok(current_time) {
            //需要继续获取超时的Udp连接唯一id
            loop {
                let event = if let Some((_key, event)) = client.0.timer.lock().pop_kv(current_time) {
                    event
                } else {
                    break;
                };

                match event {
                    QuicClientTimerEvent::UdpConnect(_udp_connect_uid) => {
                        //Udp连接超时事件
                        unimplemented!();
                    },
                    QuicClientTimerEvent::QuicConnect(quic_connect_uid) => {
                        //Quic连接超时事件
                        if let Some((_, (_udp_connect_uid, connect_value))) = client.0.connect_events.remove(&quic_connect_uid) {
                            //指定的Udp连接超时，则立即返回错误原因
                            connect_value.set(Err(Error::new(ErrorKind::TimedOut, "Connect quic timeout")));
                        }
                    },
                }
            }

        }

        client.0.udp_terminal_runtime.spawn(poll_timer(client.clone()));
    }.boxed_local()
}

//内部Quic客户端
struct InnerQuicClient {
    //Udp终端运行时
    udp_terminal_runtime:       LocalTaskRuntime<()>,
    //Udp终端
    udp_terminal:               RwLock<Option<UdpTerminal>>,
    //连接运行时
    runtimes:                   Vec<LocalTaskRuntime<()>>,
    //Quic事件发送器表
    event_sents:                XHashMap<usize, Sender<QuicEvent>>,
    //连接已读读缓冲大小限制
    readed_read_size_limit:     usize,
    //连接已读写缓冲大小限制
    readed_write_size_limit:    usize,
    //Quic端点
    end_point:                  Rc<UnsafeCell<Endpoint>>,
    //默认的客户端配置
    default_config:             ClientConfig,
    //内部时钟
    clock:                      Instant,
    //Quic连接表
    connections:                DashMap<usize, QuicClientConnection>,
    //Quic客户端服务
    service:                    Arc<QuicClientService>,
    //Quic连接路由器
    router:                     Vec<Sender<QuicEvent>>,
    //Quic连接事件表
    connect_events:             DashMap<usize, (usize, AsyncValueNonBlocking<Result<QuicSocketHandle>>)>,
    //Quic关闭事件表
    close_events:               DashMap<usize, AsyncValueNonBlocking<Result<()>>>,
    //Quic客户端事件监听器
    listener:                   Option<Arc<dyn QuicAsyncService>>,
    //定时器
    timer:                      SpinLock<Timer<QuicClientTimerEvent, 100, 60, 3>>,
    //默认的连接超时时长
    connect_timeout:            usize,
}

unsafe impl Send for InnerQuicClient {}
unsafe impl Sync for InnerQuicClient {}

// Quic客户端服务
#[derive(Clone)]
struct QuicClientService {
    client: Arc<UnsafeCell<Option<QuicClient>>>,  //客户端
}

unsafe impl Send for QuicClientService {}
unsafe impl Sync for QuicClientService {}

impl QuicAsyncService for QuicClientService {
    /// 异步处理已连接
    fn handle_connected(&self,
                        handle: QuicSocketHandle,
                        result: Result<()>) -> LocalBoxFuture<'static, ()> {
        unsafe {
            if let Some(client) = &*self.client.get() {
                if let Some((_quic_connect_uid, (_udp_connect_uid, value))) = client.0.connect_events.remove(&handle.get_connection_handle().0) {
                    //正在等待Quic连接的结果
                    if let Err(e) = result {
                        //Quic连接失败，则立即返回错误原因
                        value.set(Err(e));
                    } else {
                        //Quic连接成功，则创建双向流，并返回Quic连接句柄
                        if let Err(e) = handle.open_main_streams() {
                            //打开连接的主流失败，则立即返回错误原因
                            value.set(Err(e));
                        } else {
                            //打开连接的主流成功
                            handle
                                .set_ready(handle.get_main_stream_id().unwrap().clone(),
                                           QuicSocketReady::Readable); //开始首次读
                            value.set(Ok(handle));
                        }
                    }
                }
            }
        }

        async move {

        }.boxed_local()
    }

    /// 异步处理已打开扩展流
    fn handle_opened_expanding_stream(&self,
                                      handle: QuicSocketHandle,
                                      stream_id: StreamId,
                                      stream_type: Dir,
                                      result: Result<()>) -> LocalBoxFuture<'static, ()> {
        unsafe {
            if let Err(e) = result {
                //打开连接的扩展流失败
                error!("Open expanding stream failed, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, stream_type: {:?}, reason: {:?}",
                    handle.get_uid(),
                    handle.get_remote(),
                    handle.get_local(),
                    stream_id,
                    stream_type,
                    e);
            } else {
                //打开连接的扩展流成功
                match stream_type {
                    Dir::Uni => {
                        //TODO
                    },
                    Dir::Bi => {
                        handle
                            .set_ready(stream_id,
                                       QuicSocketReady::Readable); //开始首次读
                    },
                }
            }
        }

        async move {

        }.boxed_local()
    }

    /// 异步处理已读
    fn handle_readed(&self,
                     handle: QuicSocketHandle,
                     stream_id: StreamId,
                     result: Result<usize>) -> LocalBoxFuture<'static, ()> {
        let client = self.client.clone();
        async move {
            unsafe {
                if let Some(client) = (&*client.get()).as_ref() {
                    if let Some(listener) = &client.0.listener {
                        //客户端注册了事件监听器
                        listener
                            .handle_readed(handle, stream_id, result)
                            .await;
                    }
                }
            }
        }.boxed_local()
    }

    /// 异步处理已写
    fn handle_writed(&self,
                     _handle: QuicSocketHandle,
                     _stream_id: StreamId,
                     _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    /// 异步处理已关闭
    fn handle_closed(&self,
                     handle: QuicSocketHandle,
                     stream_id: Option<StreamId>,
                     code: u32,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        let client = self.client.clone();
        async move {
            //Quic连接已关闭
            unsafe {
                if let Some(client) = &*client.get() {
                    if let Some((_uid, value)) = client.0.close_events.remove(&handle.get_connection_handle().0) {
                        //正在等待Quic关闭连接的结果，则立即返回关闭结果
                        let r = if let Err(e) = &result {
                            Err(Error::new(ErrorKind::Other, format!("{:?}", e)))
                        } else {
                            Ok(())
                        };
                        value.set(r);
                    }
                }

                if let Some(client) = (&*client.get()).as_ref() {
                    if let Some(listener) = &client.0.listener {
                        //客户端注册了事件监听器
                        listener
                            .handle_closed(handle, stream_id, code, result)
                            .await;
                    }
                }
            }
        }.boxed_local()
    }

    /// 异步处理已超时
    fn handle_timeouted(&self,
                        _handle: QuicSocketHandle,
                        _result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }
}

///
/// Quic客户端连接
///
pub struct QuicClientConnection(Arc<InnerQuicClientConnection>);

unsafe impl Send for QuicClientConnection {}
unsafe impl Sync for QuicClientConnection {}

impl Clone for QuicClientConnection {
    fn clone(&self) -> Self {
        QuicClientConnection(self.0.clone())
    }
}

/*
* Quic客户端连接同步方法
*/
impl QuicClientConnection {
    /// 线程安全的判断连接是否关闭
    pub fn is_closed(&self) -> bool {
        self
            .0
            .handle
            .is_closed()
    }

    /// 线程安全的获取连接唯一id
    pub fn get_uid(&self) -> usize {
        self
            .0
            .handle
            .get_uid()
    }

    /// 线程安全的获取连接本地地址
    pub fn get_local(&self) -> SocketAddr {
        self
            .0
            .handle
            .get_local()
    }

    /// 线程安全的获取连接远端地址
    pub fn get_remote(&self) -> SocketAddr {
        self
            .0
            .handle
            .get_remote()
    }

    /// 获取Quic连接句柄
    pub fn get_handle(&self) -> &QuicSocketHandle {
        &self.0.handle
    }

    /// 获取内部连接句柄
    pub fn get_connection_handle(&self) -> &ConnectionHandle {
        self
            .0
            .handle
            .get_connection_handle()
    }

    /// 获取当前连接的延迟估计
    pub fn get_latency(&self) -> Duration {
        self
            .0
            .handle
            .get_latency()
    }

    /// 尝试同步非阻塞得读取指定流当前接收到的所有数据
    pub fn try_read(&self, stream_id: &StreamId) -> Option<Bytes> {
        self.try_read_with_len(stream_id, 0)
    }

    /// 尝试同步非阻塞得读取指定流的指定长度的数据，如果len为0，则表示读取任意长度的数据，如果确实读取到数据，则保证读取到的数据长度>0
    /// 如果len>0，则表示最多只读取指定数量的数据，如果确实读取到数据，则保证读取到的数据长度==len
    pub fn try_read_with_len(&self,
                             stream_id: &StreamId,
                             len: usize) -> Option<Bytes> {
        if let Some(buffer) = self.0.handle.get_read_buffer(stream_id) {
            if let Some(buf) = buffer.lock().as_mut() {
                //当前连接有读缓冲
                let remaining = buf.remaining();
                if (len > 0) && (remaining < len) {
                    //当前读缓冲中没有足够的数据
                    return None;
                }

                if (len == 0) && (remaining > 0) {
                    //用户需要读取任意长度的数据，且当前读缓冲区有足够的数据
                    Some(buf.copy_to_bytes(remaining))
                } else if (len > 0) && (remaining >= len) {
                    //用户需要读取指定长度的数据，且当前读缓冲区有足够的数据
                    Some(buf.copy_to_bytes(len))
                } else {
                    //用户需要读取任意长度的数据，且当前缓冲区没有足够的数据
                    None
                }
            } else {
                //当前连接没有读缓冲
                None
            }
        } else {
            //当前连接没有读缓冲
            None
        }
    }

    /// 线程安全的写
    pub fn write<B>(&self,
                    stream_id: StreamId,
                    buf: B) -> Result<()>
        where B: AsRef<[u8]> + 'static {
        self
            .0
            .handle
            .write_ready(stream_id, buf)
    }
}

/*
* Quic客户端连接异步方法
*/
impl QuicClientConnection {
    /// 线程安全的异步读取指定流当前接收到的所有数据
    pub async fn read(&self, stream_id: &StreamId) -> Option<Bytes> {
        self.read_with_len(stream_id, 0).await
    }

    /// 线程安全的异步读取指定流的指定长度的数据，如果len为0，则表示读取任意长度的数据，如果确实读取到数据，则保证读取到的数据长度>0
    /// 如果len>0，则表示最多只读取指定数量的数据，如果确实读取到数据，则保证读取到的数据长度==len
    pub async fn read_with_len(&self,
                               stream_id: &StreamId,
                               mut len: usize) -> Option<Bytes> {
        let remaining = if let Some(len) = self.0.handle.read_buffer_remaining(stream_id) {
            //当前连接有读缓冲
            len
        } else {
            //当前连接没有读缓冲
            return None;
        };

        let mut readed_len = 0;
        if remaining == 0 {
            //当前连接的读缓冲中没有数据，则异步准备读取数据
            readed_len = match self.0.handle.read_ready(stream_id, len) {
                Err(r) => r,
                Ok(value) => {
                    value.await
                },
            };

            if readed_len == 0 {
                //当前连接已关闭，则立即退出
                return None;
            }
        } else {
            //当前连接的读缓冲中有数据
            readed_len = remaining;
        }

        if let Some(buffer) =  self.0.handle.get_read_buffer(stream_id) {
            if let Some(buf) = buffer.lock().as_mut() {
                //当前连接有读缓冲
                if len == 0 {
                    //用户需要读取任意长度的数据
                    Some(buf.copy_to_bytes(readed_len))
                } else {
                    //用户需要读取指定长度的数据
                    Some(buf.copy_to_bytes(len))
                }
            } else {
                //当前连接没有读缓冲
                None
            }
        } else {
            //当前连接没有读缓冲
            None
        }
    }

    /// 用本地异步运行时，线程安全的异步关闭连接
    pub async fn close(self,
                       code: u32,
                       reason: Result<()>) -> Result<()> {
        self
            .0
            .client
            .close_connection(self.get_connection_handle().clone(),
                              code,
                              reason).await
    }
}

// 内部Quic客户端连接
struct InnerQuicClientConnection {
    client:             QuicClient,             //客户端
    handle:             QuicSocketHandle,       //连接句柄
}

///
/// 客户端授信级别
///
pub enum ClientCreditLevel {
    UnCredit,                       //不授信
    CreditFile(PathBuf, PathBuf),   //授信文件路径
    CreditBytes(Vec<u8>, Vec<u8>),  //授信文件数据
}

///
/// 服务器证书验证级别
///
pub enum ServerCertVerifyLevel {
    Ignore,                                                 //忽略验证
    Custom(Arc<dyn rustls::client::ServerCertVerifier>),    //自定义验证
    CaCertFile(PathBuf),                                    //CA证书文件验证
    CaCertBytes(Vec<u8>),                                   //CA证书数据验证
}

// 非可靠服务端证书验证器
struct InsecureServerCertVerifier;

impl rustls::client::ServerCertVerifier for InsecureServerCertVerifier {
    fn verify_server_cert(&self,
                          _end_entity: &rustls::Certificate,
                          _intermediates: &[rustls::Certificate],
                          _server_name: &rustls::ServerName,
                          _scts: &mut dyn Iterator<Item=&[u8]>,
                          _ocsp_response: &[u8],
                          _now: SystemTime) -> std::result::Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}



