use std::future::Future;
use std::net::SocketAddr;
use std::collections::VecDeque;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

use mio::{Token,
          net::UdpSocket};
use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded};
use flume::{Sender as AsyncSender, Receiver as AsyncReceiver, bounded};
use dashmap::DashMap;
use fnv::FnvBuildHasher;
use bytes::Buf;
use log::warn;

use pi_async::rt::{serial::{AsyncRuntime, AsyncValue},
                   serial_local_thread::LocalTaskRuntime};

use crate::{Socket, SocketAdapterFactory, AsyncService, SocketAdapter, SocketHandle,
            connect_pool::UdpSocketPool, SocketDriver,
            server::PortsAdapter};
use crate::utils::{SocketContext, UdpMultiInterface, UdpSocketStatus, AsyncServiceContextBuilder};

///
/// Udp客户端适配器
///
pub struct ClientAdapter<S: Socket>(Arc<DashMap<u16, Box<dyn AsyncService<S>>, FnvBuildHasher>>);

unsafe impl<S: Socket> Send for ClientAdapter<S> {}
unsafe impl<S: Socket> Sync for ClientAdapter<S> {}

impl<S: Socket> Clone for ClientAdapter<S> {
    fn clone(&self) -> Self {
        ClientAdapter(self.0.clone())
    }
}

impl<S: Socket> SocketAdapter for ClientAdapter<S> {
    type Connect = S;

    fn bind_runtime(&mut self, rt: &LocalTaskRuntime<()>) {

    }

    fn binded(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_binded(handle,
                                                   Err(e)))
                    } else {
                        None
                    }
                },
                Ok(handle) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_binded(handle,
                                                   Ok(())))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed_local()
    }

    fn readed(&self,
              result: GenResult<(SocketHandle<Self::Connect>, Vec<u8>, Option<SocketAddr>), (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_readed(handle,
                                                   Err(e)))
                    } else {
                        None
                    }
                },
                Ok((handle, packet, peer)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_readed(handle,
                                                   Ok((packet, peer))))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed_local()
    }

    fn writed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_writed(handle,
                                                   Err(e)))
                    } else {
                        None
                    }
                },
                Ok(handle) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_writed(handle,
                                                   Ok(())))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed_local()
    }

    fn closed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_closed(handle,
                                                   Err(e)))
                    } else {
                        None
                    }
                },
                Ok(handle) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_closed(handle,
                                                   Ok(())))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed_local()
    }
}

impl<S: Socket> ClientAdapter<S> {
    /// 构建一个Udp客户端适配器
    pub fn new() -> Self {
        ClientAdapter(Arc::new(DashMap::with_hasher(FnvBuildHasher::default())))
    }

    /// 获取所有端口
    pub fn ports(&self) -> Vec<u16> {
        self
            .0
            .iter()
            .map(|e| {
                e.key().clone()
            })
            .collect::<Vec<u16>>()
    }

    /// 设置指定端口的异步事件服务
    pub fn set_service(&self,
                       port: u16,
                       service: Box<dyn AsyncService<S>>) {
        self.0.insert(port, service);
    }

    /// 移除指定端口的异步事件服务
    pub fn unset_service(&self, port: &u16) -> Option<Box<dyn AsyncService<S>>> {
        if let Some((_key, value)) = self.0.remove(port) {
            Some(value)
        } else {
            None
        }
    }
}

///
/// Udp客户端异步服务工厂
///
pub struct ClientAdapterFactory<S: Socket> {
    inner: ClientAdapter<S>, //Udp客户端适配器
}

impl<S: Socket> SocketAdapterFactory for ClientAdapterFactory<S> {
    type Connect = S;
    type Adapter = ClientAdapter<S>;

    fn get_instance(&self) -> Self::Adapter {
        self.inner.clone()
    }
}

impl<S: Socket> ClientAdapterFactory<S> {
    /// 构建Udp端口异步服务工厂
    pub fn with_adapter(adapter: <Self as SocketAdapterFactory>::Adapter) -> Self {
        ClientAdapterFactory {
            inner: adapter,
        }
    }

    /// 获取所有绑定的端口
    pub fn ports(&self) -> Vec<u16> {
        self.inner.ports()
    }
}

///
/// Udp客户端
///
pub struct UdpClient<S: Socket>(Arc<InnerUdpClient<S>>);

unsafe impl<S: Socket> Send for UdpClient<S> {}
unsafe impl<S: Socket> Sync for UdpClient<S> {}

impl<S: Socket> Clone for UdpClient<S> {
    fn clone(&self) -> Self {
        UdpClient(self.0.clone())
    }
}

/*
* Udp客户端同步方法
*/
impl<S: Socket> UdpClient<S> {
    /// 构建一个Udp客户端
    pub fn new(mut runtimes: Vec<LocalTaskRuntime<()>>,
               init_cap: usize,            //连接池初始容量
               event_size: usize,          //同时处理的事件数
               read_packet_size: usize,    //读报文大小
               write_packet_size: usize,   //写报文大小
               timeout: Option<usize>,     //事件轮询超时时长
    ) -> Result<Self> {
        if runtimes.is_empty() {
            //至少需要一个工作者
            return Err(Error::new(ErrorKind::Other,
                                  format!("Bind listener failed, reason: require runtime")));
        }

        let rt_size = runtimes.len(); //获取工作者数量
        let mut senders = Vec::with_capacity(rt_size);
        let mut receivers = Vec::with_capacity(rt_size);
        for _ in 0..rt_size {
            let (sender, receiver) = unbounded();
            senders.push(sender);
            receivers.push(receiver);
        }
        let mut pools = Vec::with_capacity(rt_size);
        let mut driver = SocketDriver::new(&[], senders);

        //创建工作者数量的连接池，共用一个写缓冲池
        let mut index = 0;
        for receiver in receivers {
            let name = index.to_string() + " Udp Pool"; //获取连接池名称
            match UdpSocketPool::with_capacity(index as u8,
                                               name,
                                               receiver,
                                               init_cap) {
                Err(e) => {
                    return Err(e);
                },
                Ok(mut pool) => {
                    pools.push(pool);
                    index += 1;
                },
            }
        }

        //为所有连接池，设置不同端口适配器的连接驱动，并启动所有连接池
        let adapter = ClientAdapter::new();
        let factory = ClientAdapterFactory::with_adapter(adapter.clone());
        let mut index = 0;
        for pool in pools {
            let mut driver_clone = driver.clone();
            let mut instance = factory.get_instance();
            instance.bind_runtime(&runtimes[index]);
            driver_clone.set_adapter(instance); //设置连接驱动的端口适配器

            if let Err(e) = pool.run((&runtimes[index]).clone(),
                                     driver_clone,
                                     event_size,
                                     timeout) {
                //启动连接池失败
                return Err(e);
            }

            index += 1;
        }

        let index = AtomicUsize::new(0);
        let connections = DashMap::new();
        let (connect_event_sent, connect_event_recv) = unbounded();
        let connect_events = DashMap::new();
        let (close_event_sent, close_event_recv) = unbounded();
        let close_events = DashMap::new();
        let inner = InnerUdpClient {
            runtimes,
            adapter,
            driver,
            index,
            read_packet_size,
            write_packet_size,
            connections,
            connect_event_sent,
            connect_event_recv,
            connect_events,
            close_event_sent,
            close_event_recv,
            close_events,
        };

        Ok(UdpClient(Arc::new(inner)))
    }

    /// 分配Udp连接唯一id
    pub fn alloc_connection_uid(&self) -> usize {
        self.0.index.fetch_add(1, Ordering::Relaxed)
    }

    /// 用指定的连接唯一id，同步非阻塞连接到指定的服务端
    pub fn connect_with_uid<T>(&self,
                               uid: usize,
                               local: SocketAddr,
                               remote: SocketAddr,
                               read_packet_size: Option<usize>,
                               write_packet_size: Option<usize>,
                               mut service: T) -> Result<()>
        where T: AsyncService<S> {
        let read_packet_size = if let Some(size) = read_packet_size {
            size
        } else {
            self.0.read_packet_size
        };

        let write_packet_size = if let Some(size) = write_packet_size {
            size
        } else {
            self.0.write_packet_size
        };

        //绑定到指定的本地地址
        let socket = UdpSocket::bind(local)?;

        //注册指定端口的服务
        self.0.adapter.set_service(socket.local_addr().unwrap().port(),
                                   Box::new(service));

        let rt = &self.0.runtimes[uid % self.0.runtimes.len()];
        if let Err(e) = socket.connect(remote) {
            //连接远端地址失败，则立即返回错误原因
            return Err(e);
        }

        //连接远端地址成功，则路由到连接池
        if let Err(e) = self.0.driver.route(S::new(uid,
                                                     local,
                                                     Some(remote),
                                                     Some(Token(uid)),
                                                     socket,
                                                     read_packet_size,
                                                     write_packet_size)) {
            //路由失败，则立即返回错误原因
            return Err(e);
        }

        //同步非阻塞连接成功
        Ok(())
    }

    /// 同步推动Udp客户端运行，用于收集连接和关闭事件
    pub fn poll_event(&self) {
        for (uid, connect_result) in self.0.connect_event_recv.try_iter().collect::<Vec<(usize, Result<SocketHandle<S>>)>>() {
            if let Some((_uid, value)) = self.0.connect_events.remove(&uid) {
                //指定客户端连接正在等待建立连接的结果
                value.set(connect_result);
            }
        }

        for (uid, close_result) in self.0.close_event_recv.try_iter().collect::<Vec<(usize, Result<()>)>>() {
            if let Some((_uid, value)) = self.0.close_events.remove(&uid) {
                //指定客户端连接正在等待关闭连接的结果
                value.set(close_result);
            }
        }
    }

    /// 判断指定令牌的连接是否已关闭
    pub fn is_closed(&self, uid: &usize) -> bool {
        self.0.connections.contains_key(uid)
    }
}

/*
* Udp客户端异步方法
*/
impl<S: Socket> UdpClient<S> {
    /// 异步连接到指定的服务端
    pub async fn connect<T>(&self,
                            local: SocketAddr,
                            remote: SocketAddr,
                            read_packet_size: Option<usize>,
                            write_packet_size: Option<usize>,
                            mut service: T) -> Result<UdpClientConnection<S>>
        where T: AsyncService<S> {
        let read_packet_size = if let Some(size) = read_packet_size {
            size
        } else {
            self.0.read_packet_size
        };

        let write_packet_size = if let Some(size) = write_packet_size {
            size
        } else {
            self.0.write_packet_size
        };

        //绑定到指定的本地地址
        let socket = UdpSocket::bind(local)?;

        //初始化服务的上下文，并注册指定端口的服务
        let (receive_event_sent, receive_event_recv) = bounded(8);
        let mut service_context_builder = AsyncServiceContextBuilder::new();
        service_context_builder = service_context_builder
            .set_connect_notifier(self.0.connect_event_sent.clone());
        service_context_builder = service_context_builder.
            set_receive_notifier(receive_event_sent);
        service_context_builder = service_context_builder
            .set_close_notifier(self.0.close_event_sent.clone());
        let service_context = service_context_builder.build();
        service.set_context(service_context);
        self.0.adapter.set_service(socket.local_addr().unwrap().port(),
                                   Box::new(service));

        let uid = self.0.index.fetch_add(1, Ordering::Relaxed);
        let rt = &self.0.runtimes[uid % self.0.runtimes.len()];
        let value = AsyncValue::new();
        let value_copy = value.clone();
        let client = self.clone();
        rt.spawn(async move {
            if let Err(e) = socket.connect(remote) {
                //连接远端地址失败，则立即返回错误原因
                value_copy.set(Err(e));
                return;
            }

            //注册指定连接的连接事件监听器
            let connect_result = AsyncValue::new();
            client.0.connect_events.insert(uid, connect_result.clone());

            //连接远端地址成功，则路由到连接池
            if let Err(e) = client.0.driver.route(S::new(uid,
                                                         local,
                                                         Some(remote),
                                                         Some(Token(uid)),
                                                         socket,
                                                         read_packet_size,
                                                         write_packet_size)) {
                //路由失败，则立即返回错误原因
                client.0.connect_events.remove(&uid); //注销指定连接的连接事件监听器
                value_copy.set(Err(e));
                return;
            }

            //返回连接结果
            value_copy.set(connect_result.await);
        });

        match value.await {
            Err(e) => {
                //异步连接失败，则立即返回错误原因
                Err(e)
            },
            Ok(handle) => {
                //异步连接成功
                let inner = InnerUdpClientConnection {
                    client: self.clone(),
                    handle,
                    receive_event_recv,
                };

                //注册Udp客户端连接
                let connection = UdpClientConnection(Arc::new(inner));
                self
                    .0
                    .connections
                    .insert(connection.get_uid(), connection.clone());

                Ok(connection)
            },
        }
    }

    /// 异步关闭指定唯一id的Udp连接
    pub async fn close_connection(&self,
                                  uid: usize,
                                  reason: Result<()>) -> Result<()> {
        if let Some((_uid, connection)) = self.0.connections.remove(&uid) {
            //指定唯一id的Udp连接存在，则开始关闭
            let rt = &self.0.runtimes[uid % self.0.runtimes.len()];
            let value = AsyncValue::new();
            let value_copy = value.clone();
            let client = self.clone();
            rt.spawn(async move {
                //注册指定连接的关闭事件监听器
                let close_reason = AsyncValue::new();
                client.0.close_events.insert(uid, close_reason.clone());

                if let Err(e) = connection.0.handle.close(reason) {
                    //关闭连接失败，则立即返回错误原因
                    client.0.close_events.remove(&uid); //注销指定连接的关闭事件监听器
                    value_copy.set(Err(e));
                    return;
                }

                //返回关闭结果
                let _ = close_reason.await;
                value_copy.set(Ok(()));
            });

            value.await
        } else {
            //指定唯一id的Udp连接不存在，则忽略
            Ok(())
        }
    }
}

// 内部Udp客户端
struct InnerUdpClient<S: Socket> {
    runtimes:           Vec<LocalTaskRuntime<()>>,                              //运行时列表
    adapter:            ClientAdapter<S>,                                       //客户端适配器
    driver:             SocketDriver<S, ClientAdapter<S>>,                      //Udp连接驱动器
    index:              AtomicUsize,                                            //客户端连接序号
    read_packet_size:   usize,                                                  //默认读报文大小
    write_packet_size:  usize,                                                  //默认写报文大小
    connections:        DashMap<usize, UdpClientConnection<S>>,                 //连接表
    connect_event_sent: Sender<(usize, Result<SocketHandle<S>>)>,               //连接事件发送器
    connect_event_recv: Receiver<(usize, Result<SocketHandle<S>>)>,             //连接事件接收器
    connect_events:     DashMap<usize, AsyncValue<Result<SocketHandle<S>>>>,    //连接事件表
    close_event_sent:   Sender<(usize, Result<()>)>,                            //关闭事件发送器
    close_event_recv:   Receiver<(usize, Result<()>)>,                          //关闭事件接收器
    close_events:       DashMap<usize, AsyncValue<Result<()>>>,                 //关闭事件表
}

///
/// Udp客户端连接
///
pub struct UdpClientConnection<S: Socket>(Arc<InnerUdpClientConnection<S>>);

unsafe impl<S: Socket> Send for UdpClientConnection<S> {}
unsafe impl<S: Socket> Sync for UdpClientConnection<S> {}

impl<S: Socket> Clone for UdpClientConnection<S> {
    fn clone(&self) -> Self {
        UdpClientConnection(self.0.clone())
    }
}

/*
* Udp客户端连接同步方法
*/
impl<S: Socket> UdpClientConnection<S> {
    /// 线程安全的判断连接是否关闭
    pub fn is_closed(&self) -> bool {
        self
            .0
            .handle
            .is_closed()
    }

    /// 线程安全的获取连接令牌
    pub fn get_token(&self) -> &Token {
        self
            .0
            .handle
            .get_token()
    }

    /// 线程安全的获取连接唯一id
    pub fn get_uid(&self) -> usize {
        self
            .0
            .handle
            .get_uid()
    }

    /// 线程安全的获取连接本地地址
    pub fn get_local(&self) -> &SocketAddr {
        self
            .0
            .handle
            .get_local()
    }

    /// 线程安全的获取连接远端地址
    pub fn get_remote(&self) -> Option<&SocketAddr> {
        self
            .0
            .handle
            .get_remote()
    }

    /// 线程安全的设置非多播报文的生存周期
    pub fn set_ttl(&self, ttl: u32) -> Result<u32> {
        self
            .0
            .handle
            .set_ttl(ttl)
    }

    /// 线程安全的写
    pub fn write<B>(&self,
                    buf: B) -> Result<()>
        where B: Buf + Send + 'static {
        self
            .0
            .handle
            .write_ready(buf, None)
    }
}

/*
* Udp客户端连接异步方法
*/
impl<S: Socket> UdpClientConnection<S> {
    /// 线程安全的读取下个数据报文
    pub async fn read(&self) -> Option<Result<Vec<u8>>> {
        match self.0.receive_event_recv.recv_async().await {
            Err(e) => {
                //异步通道已关闭
                None
            },
            Ok(result) => {
                Some(result)
            },
        }
    }

    /// 线程安全的异步关闭连接
    pub async fn close(self, reason: Result<()>) -> Result<()> {
        self
            .0
            .client
            .close_connection(self.get_uid(), reason).await
    }
}

// 内部Udp客户端连接
struct InnerUdpClientConnection<S: Socket> {
    client:             UdpClient<S>,                   //客户端
    handle:             SocketHandle<S>,                //连接句柄
    receive_event_recv: AsyncReceiver<Result<Vec<u8>>>, //接收事件接收器
}

