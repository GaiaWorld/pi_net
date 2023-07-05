use std::fs;
use std::rc::Rc;
use std::sync::Arc;
use std::net::SocketAddr;
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use std::io::{Error, Result, ErrorKind};

use futures::future::{FutureExt, BoxFuture, LocalBoxFuture};
use quinn_proto::{EndpointConfig, ServerConfig, Endpoint, EndpointEvent, ConnectionEvent, ConnectionHandle, Transmit, TransportConfig};
use crossbeam_channel::{Receiver, Sender, unbounded};
use futures::task::SpawnExt;
use rustls;

use pi_async::rt::serial_local_thread::LocalTaskRuntime;
use pi_hash::XHashMap;
use udp::{AsyncService, SocketHandle, TaskResult};

use crate::{AsyncService as QuicAsyncService, QuicEvent,
            acceptor::QuicAcceptor,
            connect_pool::{EndPointPoller, QuicSocketPool},
            utils::{load_certs_file, load_key_file}};
use crate::connect::QuicSocket;

///
/// Quic连接监听器
///
pub struct QuicListener(Arc<InnerQuicListener>);

unsafe impl Send for QuicListener {}
unsafe impl Sync for QuicListener {}

impl Clone for QuicListener {
    fn clone(&self) -> Self {
        QuicListener(self.0.clone())
    }
}

impl AsyncService for QuicListener {
    fn bind_runtime(&mut self, _rt: LocalTaskRuntime<()>) {

    }

    fn handle_binded(&self,
                     _handle: SocketHandle,
                     _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_received(&self,
                       handle: SocketHandle,
                       result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()> {
        let listener = self.clone();
        async move {
            match result {
                Err(e) => {
                    //Udp接收数据失败
                    handle.close(Err(Error::new(ErrorKind::Other,
                                                    format!("Receive udp failed, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                        handle.get_uid(),
                                                        handle.get_remote(),
                                                        handle.get_local(),
                                                        e))));
                },
                Ok((bin, active_peer)) => {
                    //Udp接收数据成功
                    if let Some((connection_handle, event)) = listener
                        .0
                        .acceptor
                        .accept(handle,
                                bin,
                                active_peer,
                                listener.0.readed_read_size_limit,
                                listener.0.readed_write_size_limit) {
                        //处理Socket事件
                        if let Some(event_sent) = &listener
                            .0
                            .event_sents
                            .get(&(connection_handle.0 % listener.0.event_sents.len())) {
                            //向连接所在连接池发送Socket事件
                            event_sent
                                .send(QuicEvent::ConnectionReceived(connection_handle, event));
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

impl EndPointPoller for QuicListener {
    fn poll(&self,
            socket: &QuicSocket,
            handle: ConnectionHandle,
            events: VecDeque<EndpointEvent>) {
        let listener = self.clone();
        socket
            .get_udp_handle()
            .spawn(async move {
                handle_endpoint_events(listener,
                                       handle,
                                       events);
                TaskResult::Continue
            }.boxed_local());
    }
}

impl QuicListener {
    /// 构建一个Quic连接监听器
    /// timeout可以在运行时与udp不是同一个运行时设置，避免连接池事件循环空载
    pub fn new<P: AsRef<Path>>(runtimes: Vec<LocalTaskRuntime<()>>,
                               server_certs_path: P,
                               server_key_path: P,
                               verify_level: ClientCertVerifyLevel,
                               config: EndpointConfig,
                               readed_read_size_limit: usize,
                               readed_write_size_limit: usize,
                               concurrent_connections: u32,
                               transport_config: Option<Arc<TransportConfig>>,
                               service: Arc<dyn QuicAsyncService>,
                               timeout: u64) -> Result<Self> {
        match load_certs_file(server_certs_path) {
            Err(e) => {
                //加载指定的证书失败，则立即返回错误原因
                Err(e)
            },
            Ok(certs) => {
                //加载指定的证书成功
                match load_key_file(server_key_path) {
                    Err(e) => {
                        //加载指定的私钥失败，则立即返回错误原因
                        Err(e)
                    },
                    Ok(key) => {
                        //加载指定的私钥成功
                        let mut server_config = match verify_level {
                            ClientCertVerifyLevel::Ignore => {
                                //不需要严格验证客户端证书
                                match ServerConfig::with_single_cert(certs, key) {
                                    Err(e) => {
                                        //创建服务端配置失败，则立即返回错误原因
                                        return Err(Error::new(ErrorKind::Other,
                                                              format!("Create quic server failed, client_verify_level: ignore, reason: {:?}",
                                                                      e)))
                                    },
                                    Ok(config) => {
                                        config
                                    },
                                }
                            },
                            ClientCertVerifyLevel::Custom(verifier) => {
                                //需要验证客户端证书，使用指定的自定义验证器
                                match rustls::ServerConfig::builder()
                                    .with_safe_default_cipher_suites()
                                    .with_safe_default_kx_groups()
                                    .with_protocol_versions(&[&rustls::version::TLS13])
                                    .unwrap()
                                    .with_client_cert_verifier(verifier)
                                    .with_single_cert(certs, key) {
                                    Err(e) => {
                                        //创建服务端配置失败，则立即返回错误原因
                                        return Err(Error::new(ErrorKind::Other,
                                                              format!("Create quic server failed, client_verify_level: custom, reason: {:?}",
                                                                      e)))
                                    },
                                    Ok(config) => {
                                        ServerConfig::with_crypto(Arc::new(config))
                                    }
                                }
                            },
                        };

                        //创建Quic状态机
                        server_config.concurrent_connections(concurrent_connections); //设置最大并发连接数
                        if let Some(transport) = transport_config {
                            //使用外部自定义传输配置
                            server_config.transport = transport;
                        }
                        let end_point = Rc::new(UnsafeCell::new(Endpoint::new(Arc::new(config), Some(Arc::new(server_config)))));

                        //创建Quic连接接受器
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
                        let acceptor
                            = QuicAcceptor::new(end_point.clone(), router, clock);

                        let inner = InnerQuicListener {
                            rt: None,
                            acceptor,
                            event_sents,
                            end_point,
                            readed_read_size_limit,
                            readed_write_size_limit,
                            clock,
                        };
                        let listener = QuicListener(Arc::new(inner));

                        //创建服务端配置成功
                        let mut pool_id = 0;
                        for rt in runtimes {
                            //创建并运行连接池
                            let (event_send, event_recv) = event_pairs.pop_front().unwrap();
                            let connect_pool = QuicSocketPool::new(pool_id,
                                                                   listener.clone(),
                                                                   event_send,
                                                                   event_recv,
                                                                   service.clone(),
                                                                   clock,
                                                                   timeout);
                            connect_pool.run(rt);
                            pool_id += 1; //更新连接池唯一id
                        }

                        Ok(listener)
                    },
                }
            },
        }
    }
}

// 处理Quic连接监听器端点事件
fn handle_endpoint_events(listener: QuicListener,
                          connection_handle: ConnectionHandle,
                          endpoint_events: VecDeque<EndpointEvent>) {
    for endpoint_event in endpoint_events {
        if endpoint_event.is_drained() {
            //TODO 当前连接已被清理...
            continue;
        }

        unsafe {
            let end_point = &mut *listener.0.end_point.get();
            let index = connection_handle.0 % listener.0.event_sents.len(); //路由到指定连接池
            if let Some(event) = end_point.handle_event(connection_handle, endpoint_event) {
                if let Some(sender) = listener.0.event_sents.get(&index) {
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

// 内部Quic连接监听器
struct InnerQuicListener {
    rt:                         Option<LocalTaskRuntime<()>>,                                   //运行时
    acceptor:                   QuicAcceptor,                                                   //Quic连接接受器
    event_sents:                XHashMap<usize, Sender<QuicEvent>>,                             //Quic事件发送器表
    end_point:                  Rc<UnsafeCell<Endpoint>>,                                       //Quic端点
    readed_read_size_limit:     usize,                                                          //Quic已读读缓冲大小限制
    readed_write_size_limit:    usize,                                                          //Quic已读写缓冲大小限制
    clock:                      Instant,                                                        //内部时钟
}

///
/// 客户端证书验证级别
///
pub enum ClientCertVerifyLevel {
    Ignore,                                                 //忽略验证
    Custom(Arc<dyn rustls::server::ClientCertVerifier>),    //自定义验证
}

// 非可靠客户端证书验证器
struct InsecureClientCertVerifier;

impl rustls::server::ClientCertVerifier for InsecureClientCertVerifier {
    fn client_auth_root_subjects(&self) -> Option<rustls::DistinguishedNames> {
        None
    }

    fn verify_client_cert(&self,
                          _end_entity: &rustls::Certificate,
                          _intermediates: &[rustls::Certificate],
                          _now: SystemTime) -> std::result::Result<rustls::server::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::ClientCertVerified::assertion())
    }
}