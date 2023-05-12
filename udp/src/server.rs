use std::sync::Arc;
use std::net::SocketAddr;
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};

use mio::{Token, net::UdpSocket};
use futures::future::{FutureExt, LocalBoxFuture};
use dashmap::DashMap;
use crossbeam_channel::{Sender, unbounded};
use fnv::FnvBuildHasher;
use log::warn;

use pi_async::rt::serial_local_thread::LocalTaskRuntime;

use crate::{Socket, SocketAdapter, AsyncService, SocketAdapterFactory, SocketHandle, SocketDriver,
            connect_pool::UdpSocketPool};

///
/// Udp端口适配器
///
pub struct PortsAdapter<S: Socket>(Arc<DashMap<u16, Box<dyn AsyncService<S>>, FnvBuildHasher>>);

unsafe impl<S: Socket> Send for PortsAdapter<S> {}
unsafe impl<S: Socket> Sync for PortsAdapter<S> {}

impl<S: Socket> Clone for PortsAdapter<S> {
    fn clone(&self) -> Self {
        PortsAdapter(self.0.clone())
    }
}

impl<S: Socket> SocketAdapter for PortsAdapter<S> {
    type Connect = S;

    fn bind_runtime(&mut self, rt: &LocalTaskRuntime<()>) {
        let mut iter = self.0.iter_mut();
        while let Some(mut service) = iter.next() {
            service.bind_runtime(rt.clone());
        }
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

impl<S: Socket> PortsAdapter<S> {
    /// 构建一个Udp端口适配器
    pub fn new() -> Self {
        PortsAdapter(Arc::new(DashMap::with_hasher(FnvBuildHasher::default())))
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
/// Udp端口异步服务工厂
///
pub struct PortsAdapterFactory<S: Socket> {
    inner: PortsAdapter<S>, //Udp端口适配器
}

impl<S: Socket> SocketAdapterFactory for PortsAdapterFactory<S> {
    type Connect = S;
    type Adapter = PortsAdapter<S>;

    fn get_instance(&self) -> Self::Adapter {
        self.inner.clone()
    }
}

impl<S: Socket> PortsAdapterFactory<S> {
    /// 构建Udp端口异步服务工厂
    pub fn new() -> Self {
        let inner = PortsAdapter::new();
        PortsAdapterFactory {
            inner,
        }
    }

    /// 绑定指定端口的异步事件服务
    pub fn bind(&mut self,
                port: u16,
                service: Box<dyn AsyncService<S>>) {
        self.inner.set_service(port, service);
    }

    /// 获取所有绑定的端口
    pub fn ports(&self) -> Vec<u16> {
        self.inner.ports()
    }
}

///
/// Udp连接绑定器
///
pub struct SocketListener<
    S: Socket,
    F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>,
> {
    marker: PhantomData<(S, F)>,
}

impl<S, F> SocketListener<S, F>
    where S: Socket,
          F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>, {
    /// 绑定指定配置的Udp连接绑定器
    pub fn bind(mut runtimes: Vec<LocalTaskRuntime<()>>,
                factory: F,                 //Udp端口适配器工厂
                addrs: Vec<SocketAddr>,     //待绑定的Udp地址列表
                init_cap: usize,            //连接池初始容量
                event_size: usize,          //同时处理的事件数
                read_packet_size: usize,    //读报文大小
                write_packet_size: usize,   //写报文大小
                timeout: Option<usize>,     //事件轮询超时时长
    ) -> Result<()> {
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
        let mut driver = SocketDriver::new(&addrs[..], senders);

        //创建工作者数量的连接池，共用一个写缓冲池
        let mut index = 0;
        for receiver in receivers {
            let name = addrs
                .iter()
                .map(|addr| {
                    addr.port().to_string()
                })
                .collect::<Vec<String>>()
                .join(","); //获取连接池名称

            match UdpSocketPool::with_capacity(index as u8,
                                               name,
                                               receiver,
                                               init_cap) {
                Err(e) => {
                    return Err(e);
                },
                Ok(pool) => {
                    pools.push(pool);
                    index += 1;
                },
            }
        }

        //为所有连接池，设置不同端口适配器的连接驱动，并启动所有连接池
        for pool in pools {
            if let Some(rt) = runtimes.pop() {
                let mut driver_clone = driver.clone();
                let mut instance = factory.get_instance();
                instance.bind_runtime(&rt);
                driver_clone.set_adapter(instance); //设置连接驱动的端口适配器

                if let Err(e) = pool.run(rt,
                                         driver_clone,
                                         event_size,
                                         timeout) {
                    //启动连接池失败
                    return Err(e);
                }
            }
        }

        //绑定所有地址
        let mut index = 0;
        for addr in addrs {
            match UdpSocket::bind(addr.clone()) {
                Err(e) => {
                    //绑定指定地址失败，则立即抛出异常
                    panic!("Udp bind address failed, addr: {:?}, reason: {:?}",
                        addr,
                        e);
                },
                Ok(mut socket) => {
                    //绑定指定地址成功，则路由到连接池
                    driver.route(S::new(index,
                                        addr,
                                        None,
                                        Some(Token(index)),
                                        socket,
                                        read_packet_size,
                                        write_packet_size));
                    index += 1;
                }
            }
        }

        Ok(())
    }
}