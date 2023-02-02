use std::fs;
use std::rc::Rc;
use std::sync::Arc;
use std::path::Path;
use std::time::Instant;
use std::net::SocketAddr;
use std::cell::UnsafeCell;
use std::io::{Error, Result, ErrorKind};

use futures::future::{FutureExt, LocalBoxFuture};
use quinn_proto::{crypto, Endpoint, EndpointConfig, ClientConfig, EndpointEvent, DatagramEvent, ConnectionEvent, ConnectionHandle, Transmit, Connection, StreamId};
use crossbeam_channel::{Sender, Receiver, unbounded};
use flume::{Sender as AsyncSender, Receiver as AsyncReceiver, bounded};
use dashmap::DashMap;
use bytes::{Buf, Bytes, BytesMut};
use rustls;
use log::error;
use pi_async::prelude::SpinLock;

use pi_hash::XHashMap;
use pi_cancel_timer::Timer;
use pi_async::rt::{AsyncRuntime, serial::AsyncValue,
                   serial_local_thread::LocalTaskRuntime};
use udp::{Socket, AsyncService, SocketHandle,
          connect::UdpSocket,
          client::UdpClient,
          utils::AsyncServiceContext};

use crate::{AsyncService as QuicAsyncService, SocketHandle as QuicSocketHandle, SocketEvent,
            connect::QuicSocket,
            connect_pool::QuicSocketPool,
            utils::{QuicSocketReady, QuicClientTimerEvent}};


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
pub struct QuicClient<S: Socket = UdpSocket>(Arc<InnerQuicClient<S>>);

unsafe impl<S: Socket> Send for QuicClient<S> {}
unsafe impl<S: Socket> Sync for QuicClient<S> {}

impl<S: Socket> Clone for QuicClient<S> {
    fn clone(&self) -> Self {
        QuicClient(self.0.clone())
    }
}

impl<S: Socket> AsyncService<S> for QuicClient<S> {
    fn get_context(&self) -> Option<&AsyncServiceContext<S>> {
        //忽略获取Udp连接上下文
        None
    }

    fn set_context(&mut self, _context: AsyncServiceContext<S>) {
        //忽略设置Udp连接上下文
    }

    fn bind_runtime(&mut self, rt: LocalTaskRuntime<()>) {
        //运行Quic客户端事件循环
        let rt_copy = rt.clone();
        rt.spawn(event_loop(rt_copy, self.clone()));
    }

    fn handle_binded(&self,
                     udp_handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        if let Ok(_) = result {
            //建立Udp连接成功
            if let Some((udp_connect_uid, (remote, hostname, client_config, timeout, now, value))) = self.0.wait_udp_connects.remove(&udp_handle.get_uid()) {
                //有Quic连接正在等待Udp连接建立
                let config = if let Some(cfg) = client_config {
                    cfg
                } else {
                    self.0.default_config.clone()
                };

                //同步建立指定地址的Quic连接
                match unsafe { (&mut *self.0.end_point.get()).connect(config,
                                                                      remote,
                                                                      hostname.as_str()) } {
                    Err(e) => {
                        //建立Quic连接失败，则立即关闭当前Udp连接
                        udp_handle.close(Err(Error::new(ErrorKind::Other,
                                                        format!("Create quic connection failed, hostname: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                                hostname,
                                                                udp_handle.get_remote(),
                                                                udp_handle.get_local(),
                                                                e))));

                        //立即返回连接错误原因
                        value.set(Err(Error::new(ErrorKind::Other,
                                                 format!("Create quic connection failed, hostname: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                         hostname,
                                                         udp_handle.get_remote(),
                                                         udp_handle.get_local(),
                                                         e))));
                    },
                    Ok((connection_handle, connection)) => {
                        //建立Quic连接成功
                        let quic_connect_timeout = timeout.checked_sub(now.elapsed().as_millis() as usize).unwrap_or(0);
                        if quic_connect_timeout == 0 {
                            //没有剩余连接时长，则立即关闭当前Udp连接，并立即返回连接错误原因
                            udp_handle.close(Err(Error::new(ErrorKind::Other,
                                                            format!("Create quic connection failed, hostname: {:?}, remote: {:?}, local: {:?}, reason: Connect quic timeout",
                                                                    hostname,
                                                                    udp_handle.get_remote(),
                                                                    udp_handle.get_local()))));

                            value.set(Err(Error::new(ErrorKind::TimedOut, "Connect quic timeout")));
                        } else {
                            //还有剩余连接时长，则设置Quic连接的超时时长
                            self.0.timer.lock().push(quic_connect_timeout, QuicClientTimerEvent::QuicConnect(connection_handle.0));

                            //注册Quic连接事件表
                            self.0.connect_events.insert(connection_handle.0, (udp_connect_uid, value));

                            //将Quic Socket路由到连接池
                            let sender = &self.0.router[connection_handle.0 % self.0.router.len()];
                            sender.send(QuicSocket::new(udp_handle,
                                                        connection_handle,
                                                        connection,
                                                        self.0.readed_read_size_limit,
                                                        self.0.readed_write_size_limit,
                                                        self.0.clock));
                        }
                    },
                }
            }
        }

        async move {

        }.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()> {
        let quic_client = self.clone();
        async move {
            match result {
                Err(e) => {
                    //Udp读数据失败
                    handle.close(Err(Error::new(ErrorKind::Other,
                                                format!("Read udp failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                        handle.get_token(),
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
                                event_sent.send((connection_handle, event));
                            }
                        }
                    }
                },
            }
        }.boxed_local()
    }

    fn handle_writed(&self,
                     _handle: SocketHandle<S>,
                     _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_closed(&self,
                     _handle: SocketHandle<S>,
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

// Quic客户端端点事件循环
fn event_loop<S>(rt: LocalTaskRuntime<()>,
                 quic_client: QuicClient<S>) -> LocalBoxFuture<'static, ()>
    where S: Socket {
    async move {
        handle_endpoint_events(&quic_client);

        rt.spawn(event_loop(rt.clone(), quic_client));
    }.boxed_local()
}

// 处理Quic客户端端点事件
fn handle_endpoint_events<S>(quic_client: &QuicClient<S>)
    where S: Socket {
    //处理Quic客户端连接的端点事件
    for (connection_handle, endpoint_event) in quic_client.0.endpoint_event_recv.try_iter().collect::<Vec<(ConnectionHandle, EndpointEvent)>>() {
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
                    sender.send((connection_handle, event));
                }
            }

            if let Some(sender) = quic_client.0.write_sents.get(&index) {
                while let Some(transmit) = end_point.poll_transmit() {
                    //向连接所在连接池发送Socket发送事件
                    sender.send((connection_handle, transmit));
                }
            }
        }
    }
}

/*
* Quic客户端同步方法
*/
impl<S: Socket> QuicClient<S> {
    /// 构建一个Quic客户端
    pub fn new(udp_client_runtime: LocalTaskRuntime<()>,
               runtimes: Vec<LocalTaskRuntime<()>>,
               root_cert: Vec<u8>,
               config: EndpointConfig,
               readed_read_size_limit: usize,
               readed_write_size_limit: usize,
               udp_read_packet_size: usize,
               udp_write_packet_size: usize,
               connect_timeout: usize,
               udp_timeout: Option<usize>,
               quic_timeout: Option<usize>) -> Result<Self> {
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

        //启动Udp客户端，有且只允许有一个运行时
        let udp_client = UdpClient::new(vec![udp_client_runtime.clone()],
                                        1024,
                                        1024,
                                        udp_read_packet_size,
                                        udp_write_packet_size,
                                        udp_timeout)?;

        //加载客户端使用的根证书
        let mut roots = rustls::RootCertStore::empty();
        if let Err(e) = roots.add(&rustls::Certificate(root_cert)) {
            //增加根证书失败，则立即返回错误原因
            return Err(Error::new(ErrorKind::Other,
                                  format!("Create quic client failed, reason: {:?}",
                                          e)));
        }

        //配置默认客户端安全参数，并构建默认客户端配置
        let mut client_crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let default_config = ClientConfig::new(Arc::new(client_crypto));

        //构建客户端连接池
        let mut pool_id = 0;
        let clock = Instant::now();
        let mut router = Vec::with_capacity(runtimes.len());
        let (endpoint_event_sent, endpoint_event_recv) = unbounded();
        let mut event_sents = XHashMap::default();
        let mut write_sents = XHashMap::default();
        let service = Arc::new(QuicClientService {
            client: Arc::new(UnsafeCell::new(None)),
        });
        for rt in runtimes.clone() {
            let (sender, socket_recv) = unbounded();
            router.push(sender);
            let (sender, read_recv) = unbounded();
            event_sents.insert(pool_id, sender);
            let (write_sent, write_recv) = unbounded();
            write_sents.insert(pool_id, write_sent.clone());

            //创建并运行连接池
            let connect_pool = QuicSocketPool::new(pool_id,
                                                   endpoint_event_sent.clone(),
                                                   socket_recv,
                                                   read_recv,
                                                   write_sent,
                                                   write_recv,
                                                   service.clone(),
                                                   clock,
                                                   quic_timeout);
            connect_pool.run(rt);
            pool_id += 1; //更新连接池唯一id
        }

        //创建Quic状态机
        let end_point = Rc::new(UnsafeCell::new(Endpoint::new(Arc::new(config), None)));
        let wait_udp_connects = DashMap::new();
        let connections = DashMap::new();
        let connect_events = DashMap::new();
        let close_events = DashMap::new();
        let timer = SpinLock::new(Timer::<QuicClientTimerEvent, 100, 60, 3>::default());

        let inner = InnerQuicClient {
            udp_client_runtime: udp_client_runtime.clone(),
            udp_client,
            runtimes,
            endpoint_event_recv,
            event_sents,
            write_sents,
            readed_read_size_limit,
            readed_write_size_limit,
            end_point,
            default_config,
            clock,
            connections,
            service,
            router,
            wait_udp_connects,
            connect_events,
            close_events,
            timer,
            connect_timeout,
        };
        let client = QuicClient(Arc::new(inner));
        unsafe {
            *client.0.service.client.get() = Some(client.clone());
        }

        //启动客户端定时器
        let client_copy = client.clone();
        udp_client_runtime.spawn(poll_timer(client_copy));

        Ok(client)
    }

    /// 用指定路径的证书文件，构建一个Quic客户端
    pub fn with_cert_file<P: AsRef<Path>>(udp_client_runtime: LocalTaskRuntime<()>,
                                          runtimes: Vec<LocalTaskRuntime<()>>,
                                          root_cert_path: P,
                                          config: EndpointConfig,
                                          readed_read_size_limit: usize,
                                          readed_write_size_limit: usize,
                                          udp_read_packet_size: usize,
                                          udp_write_packet_size: usize,
                                          connect_timeout: usize,
                                          udp_timeout: Option<usize>,
                                          quic_timeout: Option<usize>) -> Result<Self> {
        let root_cert = fs::read(root_cert_path)?;
        Self::new(udp_client_runtime,
                  runtimes,
                  root_cert,
                  config,
                  readed_read_size_limit,
                  readed_write_size_limit,
                  udp_read_packet_size,
                  udp_write_packet_size,
                  connect_timeout,
                  udp_timeout,
                  quic_timeout)
    }

    /// 判断指定令牌的连接是否已关闭
    pub fn is_closed(&self, uid: &usize) -> bool {
        self.0.connections.contains_key(uid)
    }
}

/*
* Quic客户端异步方法
*/
impl<S: Socket> QuicClient<S> {
    /// 异步连接到指定地址和主机名的服务端
    pub async fn connect(&self,
                         local: SocketAddr,
                         remote: SocketAddr,
                         hostname: &str,
                         config: Option<ClientConfig>,
                         timeout: Option<usize>) -> Result<QuicClientConnection<S>> {
        let udp_connect_uid = self.0.udp_client.alloc_connection_uid();
        let connect_value = AsyncValue::new();
        let connect_value_copy = connect_value.clone();

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

        //注册等待Udp连接的Quic连接
        self
            .0
            .wait_udp_connects
            .insert(udp_connect_uid,
                    (remote, hostname.to_string(), config, connect_timeout, Instant::now(), connect_value_copy));

        let quic_client = self.clone();
        if let Err(e) = self.0.udp_client.connect_with_uid(udp_connect_uid,
                                                           local,
                                                           remote,
                                                           None,
                                                           None,
                                                           quic_client) {
            //创建Udp连接失败，则立即返回错误原因
            self.0.wait_udp_connects.remove(&udp_connect_uid); //注销等待Udp连接的Quic连接
            return Err(e);
        }

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
            let value = AsyncValue::new();
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

// 在客户端所在的Udp客户端运行时中，异步推动定时器
fn poll_timer<S: Socket>(client: QuicClient<S>) -> LocalBoxFuture<'static, ()> {
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
                    QuicClientTimerEvent::UdpConnect(udp_connect_uid) => {
                        //Udp连接超时事件
                        if let Some((_, (_, _, _, _, _, connect_value))) = client.0.wait_udp_connects.remove(&udp_connect_uid) {
                            //指定的Udp连接超时，则立即返回错误原因
                            connect_value.set(Err(Error::new(ErrorKind::TimedOut, "Connect udp timeout")));
                        }
                    },
                    QuicClientTimerEvent::QuicConnect(quic_connect_uid) => {
                        //Quic连接超时事件
                        if let Some((_, (udp_connect_uid, connect_value))) = client.0.connect_events.remove(&quic_connect_uid) {
                            //指定的Udp连接超时，则立即关闭对应的Udp连接，并立即返回错误原因
                            client.0.udp_client.close_connection(udp_connect_uid, Err(Error::new(ErrorKind::TimedOut,
                                                                                                 "Connect quic timeout")));

                            connect_value.set(Err(Error::new(ErrorKind::TimedOut, "Connect quic timeout")));
                        }
                    },
                }
            }

        }

        client.0.udp_client_runtime.spawn(poll_timer(client.clone()));
    }.boxed_local()
}

//内部Quic客户端
struct InnerQuicClient<S: Socket> {
    udp_client_runtime:         LocalTaskRuntime<()>,                                                                                               //Udp客户端运行时
    udp_client:                 UdpClient<S>,                                                                                                       //Udp客户端
    runtimes:                   Vec<LocalTaskRuntime<()>>,                                                                                          //连接运行时
    endpoint_event_recv:        Receiver<(ConnectionHandle, EndpointEvent)>,                                                                        //Quic客户端端点事件接收器
    event_sents:                XHashMap<usize, Sender<(ConnectionHandle, ConnectionEvent)>>,                                                   //Socket事件发送器表
    write_sents:                XHashMap<usize, Sender<(ConnectionHandle, Transmit)>>,                                                          //Socket发送事件发送器表
    readed_read_size_limit:     usize,                                                                                                              //连接已读读缓冲大小限制
    readed_write_size_limit:    usize,                                                                                                              //连接已读写缓冲大小限制
    end_point:                  Rc<UnsafeCell<Endpoint>>,                                                                                           //Quic端点
    default_config:             ClientConfig,                                                                                                       //默认的客户端配置
    clock:                      Instant,                                                                                                            //内部时钟
    connections:                DashMap<usize, QuicClientConnection<S>>,                                                                            //Quic连接表
    service:                    Arc<QuicClientService<S>>,                                                                                          //Quic客户端服务
    router:                     Vec<Sender<QuicSocket<S>>>,                                                                                         //Quic连接路由器
    wait_udp_connects:          DashMap<usize, (SocketAddr, String, Option<ClientConfig>, usize, Instant, AsyncValue<Result<QuicSocketHandle<S>>>)>,   //等待Udp连接表
    connect_events:             DashMap<usize, (usize, AsyncValue<Result<QuicSocketHandle<S>>>)>,                                                            //Quic连接事件表
    close_events:               DashMap<usize, AsyncValue<Result<()>>>,                                                                             //Quic关闭事件表
    timer:                      SpinLock<Timer<QuicClientTimerEvent, 100, 60, 3>>,                                                                  //定时器
    connect_timeout:            usize,                                                                                                              //默认的连接超时时长
}

unsafe impl<S: Socket> Send for InnerQuicClient<S> {}
unsafe impl<S: Socket> Sync for InnerQuicClient<S> {}

// Quic客户端服务
#[derive(Clone)]
struct QuicClientService<S: Socket = UdpSocket> {
    client: Arc<UnsafeCell<Option<QuicClient<S>>>>,  //客户端
}

unsafe impl<S: Socket> Send for QuicClientService<S> {}
unsafe impl<S: Socket> Sync for QuicClientService<S> {}

impl<S: Socket> QuicAsyncService<S> for QuicClientService<S> {
    /// 异步处理已连接
    fn handle_connected(&self,
                        handle: QuicSocketHandle<S>,
                        result: Result<()>) -> LocalBoxFuture<'static, ()> {
        unsafe {
            if let Some(client) = &*self.client.get() {
                if let Some((_quic_connect_uid, (_udp_connect_uid, value))) = client.0.connect_events.remove(&handle.get_connection_handle().0) {
                    //正在等待Quic连接的结果
                    if let Err(e) = result {
                        //Quic连接失败，则关闭Udp连接，并立即返回错误原因
                        let _ = client
                            .0
                            .udp_client
                            .close_connection(handle.get_udp_uid(),
                                              Err(Error::new(e.kind(), format!("{:?}", e))));
                        value.set(Err(e));
                    } else {
                        //Quic连接成功，则创建双向流，并返回Quic连接句柄
                        if let Err(e) = handle.open_main_streams() {
                            //打开连接的主流失败，则关闭Udp连接，并立即返回错误原因
                            let _ = client
                                .0
                                .udp_client
                                .close_connection(handle.get_udp_uid(),
                                                  Err(Error::new(e.kind(), format!("{:?}", e))));
                            value.set(Err(e));
                        } else {
                            //打开连接的主流成功
                            handle.set_ready(QuicSocketReady::Readable); //开始首次读
                            value.set(Ok(handle));
                        }
                    }
                }
            }
        }

        async move {

        }.boxed_local()
    }

    /// 异步处理已读
    fn handle_readed(&self,
                     handle: QuicSocketHandle<S>,
                     result: Result<usize>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    /// 异步处理已写
    fn handle_writed(&self,
                     handle: QuicSocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    /// 异步处理已关闭
    fn handle_closed(&self,
                     handle: QuicSocketHandle<S>,
                     _stream_id: Option<StreamId>,
                     _code: u32,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        let client = self.client.clone();
        async move {
            //Quic连接已关闭
            unsafe {
                if let Some(client) = &*client.get() {
                    //关闭对应的Udp连接
                    let result = client
                        .0
                        .udp_client
                        .close_connection(handle.get_udp_uid(),
                                          result)
                        .await;

                    if let Some((_uid, value)) = client.0.close_events.remove(&handle.get_connection_handle().0) {
                        //正在等待Quic关闭连接的结果，则立即返回关闭结果
                        value.set(result);
                    }
                }
            }
        }.boxed_local()
    }

    /// 异步处理已超时
    fn handle_timeouted(&self,
                        handle: QuicSocketHandle<S>,
                        result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }
}

///
/// Quic客户端连接
///
pub struct QuicClientConnection<S: Socket = UdpSocket>(Arc<InnerQuicClientConnection<S>>);

unsafe impl<S: Socket> Send for QuicClientConnection<S> {}
unsafe impl<S: Socket> Sync for QuicClientConnection<S> {}

impl<S: Socket> Clone for QuicClientConnection<S> {
    fn clone(&self) -> Self {
        QuicClientConnection(self.0.clone())
    }
}

/*
* Quic客户端连接同步方法
*/
impl<S: Socket> QuicClientConnection<S> {
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

    /// 获取内部连接句柄
    pub fn get_connection_handle(&self) -> &ConnectionHandle {
        self
            .0
            .handle
            .get_connection_handle()
    }

    /// 尝试同步非阻塞得读取当前接收到的所有数据
    pub fn try_read(&self) -> Option<Bytes> {
        self.try_read_with_len(0)
    }

    /// 尝试同步非阻塞得读取指定长度的数据，如果len为0，则表示读取任意长度的数据，如果确实读取到数据，则保证读取到的数据长度>0
    /// 如果len>0，则表示最多只读取指定数量的数据，如果确实读取到数据，则保证读取到的数据长度==len
    pub fn try_read_with_len(&self, len: usize) -> Option<Bytes> {
        if let Some(buf) = self.0.handle.get_read_buffer().lock().as_mut() {
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
    }

    /// 线程安全的写
    pub fn write<B>(&self,
                    buf: B) -> Result<()>
        where B: AsRef<[u8]> + 'static {
        self
            .0
            .handle
            .write_ready(buf)
    }
}

/*
* Quic客户端连接异步方法
*/
impl<S: Socket> QuicClientConnection<S> {
    /// 线程安全的异步读取当前接收到的所有数据
    pub async fn read(&self) -> Option<Bytes> {
        self.read_with_len(0).await
    }

    /// 线程安全的异步读取指定长度的数据，如果len为0，则表示读取任意长度的数据，如果确实读取到数据，则保证读取到的数据长度>0
    /// 如果len>0，则表示最多只读取指定数量的数据，如果确实读取到数据，则保证读取到的数据长度==len
    pub async fn read_with_len(&self, mut len: usize) -> Option<Bytes> {
        let remaining = if let Some(len) = self.0.handle.read_buffer_remaining() {
            //当前连接有读缓冲
            len
        } else {
            //当前连接没有读缓冲
            return None;
        };

        let mut readed_len = 0;
        if remaining == 0 {
            //当前连接的读缓冲中没有数据，则异步准备读取数据
            readed_len = match self.0.handle.read_ready(len) {
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
            readed_len = remaining;
        }

        if let Some(buf) = self.0.handle.get_read_buffer().lock().as_mut() {
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
struct InnerQuicClientConnection<S: Socket> {
    client:             QuicClient<S>,             //客户端
    handle:             QuicSocketHandle<S>,       //连接句柄
}



