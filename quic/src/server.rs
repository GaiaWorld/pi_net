use std::rc::Rc;
use std::sync::Arc;
use std::path::Path;
use std::time::Instant;
use std::net::SocketAddr;
use std::cell::UnsafeCell;
use std::io::{Error, Result, ErrorKind};

use futures::future::{FutureExt, LocalBoxFuture};
use quinn_proto::{EndpointConfig, ServerConfig, Endpoint, EndpointEvent, ConnectionEvent, ConnectionHandle, Transmit};
use crossbeam_channel::{Receiver, Sender, unbounded};

use pi_async::rt::serial_local_thread::LocalTaskRuntime;
use pi_hash::XHashMap;

use udp::{Socket, AsyncService, SocketHandle,
          connect::UdpSocket, utils::AsyncServiceContext};

use crate::{acceptor::QuicAcceptor,
            AsyncService as QuicAsyncService,
            connect_pool::QuicSocketPool,
            utils::{load_certs, load_private_key}};

///
/// Quic连接监听器
///
pub struct QuicListener<S: Socket = UdpSocket>(Rc<InnerQuicListener<S>>);

unsafe impl<S: Socket> Send for QuicListener<S> {}
unsafe impl<S: Socket> Sync for QuicListener<S> {}

impl<S: Socket> Clone for QuicListener<S> {
    fn clone(&self) -> Self {
        QuicListener(self.0.clone())
    }
}

impl<S: Socket> AsyncService<S> for QuicListener<S> {
    fn get_context(&self) -> Option<&AsyncServiceContext<S>> {
        None
    }

    fn set_context(&mut self, _context: AsyncServiceContext<S>) {

    }

    fn bind_runtime(&mut self, rt: LocalTaskRuntime<()>) {
        Rc::get_mut(&mut self.0).unwrap().rt = Some(rt.clone()); //在未复制前可以设置运行时
        let listener = self.clone();

        //运行Quic连接监听器事件循环
        let rt_copy = rt.clone();
        rt.spawn(event_loop(rt_copy, listener));
    }

    fn handle_binded(&self,
                     _handle: SocketHandle<S>,
                     _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()> {
        let listener = self.clone();
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
                            .event_sents.get(&(connection_handle.0 % listener.0.event_sents.len())) {
                            //向连接所在连接池发送Socket事件
                            event_sent.send((connection_handle, event));
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

impl<S: Socket> QuicListener<S> {
    /// 构建一个Quic连接监听器
    /// timeout可以在运行时与udp不是同一个运行时设置，避免连接池事件循环空载
    pub fn new<P: AsRef<Path>>(runtimes: Vec<LocalTaskRuntime<()>>,
                               server_certs_path: P,
                               server_key_path: P,
                               config: EndpointConfig,
                               readed_read_size_limit: usize,
                               readed_write_size_limit: usize,
                               service: Arc<dyn QuicAsyncService<S>>,
                               timeout: Option<usize>) -> Result<Self> {
        match load_certs(server_certs_path) {
            Err(e) => {
                //加载指定的证书失败，则立即返回错误原因
                Err(e)
            },
            Ok(certs) => {
                //加载指定的证书成功
                match load_private_key(server_key_path) {
                    Err(e) => {
                        //加载指定的私钥失败，则立即返回错误原因
                        Err(e)
                    },
                    Ok(key) => {
                        //加载指定的私钥成功
                        match ServerConfig::with_single_cert(certs, key) {
                            Err(e) => {
                                //创建服务端配置失败，则立即返回错误原因
                                Err(Error::new(ErrorKind::Other,
                                               format!("Creat server config failed, reason: {:?}",
                                                       e)))
                            },
                            Ok(server_config) => {
                                //创建服务端配置成功
                                let mut pool_id = 0;
                                let clock = Instant::now();
                                let mut router = Vec::with_capacity(runtimes.len());
                                let (endpoint_event_sent, endpoint_event_recv) = unbounded();
                                let mut event_sents = XHashMap::default();
                                let mut write_sents = XHashMap::default();
                                for rt in runtimes {
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
                                                                           timeout);
                                    connect_pool.run(rt);
                                    pool_id += 1; //更新连接池唯一id
                                }

                                //创建Quic状态机
                                let end_point = Rc::new(UnsafeCell::new(Endpoint::new(Arc::new(config), Some(Arc::new(server_config)))));

                                //创建Quic连接接受器
                                let acceptor = QuicAcceptor::new(end_point.clone(), router, clock);

                                let inner = InnerQuicListener {
                                    rt: None,
                                    acceptor,
                                    endpoint_event_recv,
                                    event_sents,
                                    write_sents,
                                    end_point,
                                    readed_read_size_limit,
                                    readed_write_size_limit,
                                    clock,
                                };

                                Ok(QuicListener(Rc::new(inner)))
                            },
                        }
                    },
                }
            },
        }
    }
}

// Quic连接监听器端点事件循环
fn event_loop<S>(rt: LocalTaskRuntime<()>,
                 listener: QuicListener<S>) -> LocalBoxFuture<'static, ()>
    where S: Socket {
    async move {
        handle_endpoint_events(&listener);

        rt.spawn(event_loop(rt.clone(), listener));
    }.boxed_local()
}

// 处理Quic连接监听器端点事件
fn handle_endpoint_events<S>(listener: &QuicListener<S>)
    where S: Socket {
    for (connection_handle, endpoint_event) in listener.0.endpoint_event_recv.try_iter().collect::<Vec<(ConnectionHandle, EndpointEvent)>>() {
        if endpoint_event.is_drained() {
            //TODO 当前连接已被清理...
            continue;
        }

        unsafe {
            let end_point = &mut *listener.0.end_point.get();
            let index = connection_handle.0 % listener.0.event_sents.len(); //路由到指定连接池
            if let Some(event) = end_point.handle_event(connection_handle, endpoint_event) {
                if let Some(sender) = listener.0.event_sents.get(&index) {
                    println!("!!!!!!connection_event: {:?}", event);
                    //向连接所在连接池发送Socket事件
                    sender.send((connection_handle, event));
                }
            }

            if let Some(sender) = listener.0.write_sents.get(&index) {
                while let Some(transmit) = end_point.poll_transmit() {
                    //向连接所在连接池发送Socket发送事件
                    sender.send((connection_handle, transmit));
                }
            }
        }
    }
}

// 内部Quic连接监听器
struct InnerQuicListener<S: Socket> {
    rt:                         Option<LocalTaskRuntime<()>>,                                   //运行时
    acceptor:                   QuicAcceptor<S>,                                                //Quic连接接受器
    endpoint_event_recv:        Receiver<(ConnectionHandle, EndpointEvent)>,                    //Quic连接监听器端点事件接收器
    event_sents:                XHashMap<usize, Sender<(ConnectionHandle, ConnectionEvent)>>,   //Socket事件发送器表
    write_sents:                XHashMap<usize, Sender<(ConnectionHandle, Transmit)>>,          //Socket发送事件发送器表
    end_point:                  Rc<UnsafeCell<Endpoint>>,                                       //Quic端点
    readed_read_size_limit:     usize,                                                          //Quic已读读缓冲大小限制
    readed_write_size_limit:    usize,                                                          //Quic已读写缓冲大小限制
    clock:                      Instant,                                                        //内部时钟
}