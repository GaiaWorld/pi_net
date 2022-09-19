use std::mem;
use std::sync::Arc;
use std::task::Waker;
use std::cell::RefCell;
use std::rc::{Weak, Rc};
use std::future::Future;
use std::net::SocketAddr;
use std::any::{Any, TypeId};
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::collections::hash_map::Entry;
use std::io::{Result, Error, ErrorKind};

use mio::Token;
use fnv::FnvBuildHasher;
use crossbeam_channel::{Sender, unbounded};
use futures::future::{FutureExt, BoxFuture};
use dashmap::DashMap;
use num_cpus;

use pi_async::rt::worker_thread::WorkerRuntime;

use crate::{Socket, Stream, SocketAdapter, SocketAdapterFactory, SocketConfig, SocketEvent, SocketDriver, SocketHandle, AsyncService, acceptor::Acceptor, connect_pool::TcpSocketPool, SocketStatus};

///
/// Tcp端口适配器
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

    fn connected(&self,
                 result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> BoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_connected(handle,
                                                     SocketStatus::Connected(Err(e))))
                    } else {
                        None
                    }
                },
                Ok(handle) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_connected(handle,
                                                     SocketStatus::Connected(Ok(()))))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed()
    }

    fn readed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> BoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_readed(handle,
                                                   SocketStatus::Readed(Err(e))))
                    } else {
                        None
                    }
                },
                Ok(handle) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_readed(handle,
                                                   SocketStatus::Readed(Ok(()))))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed()
    }

    fn writed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> BoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_writed(handle,
                                                   SocketStatus::Writed(Err(e))))
                    } else {
                        None
                    }
                },
                Ok(handle) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_writed(handle,
                                                   SocketStatus::Writed(Ok(()))))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed()
    }

    fn closed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> BoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let task = match result {
                Err((handle, e)) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_closed(handle,
                                                   SocketStatus::Closed(Err(e))))
                    } else {
                        None
                    }
                },
                Ok(handle) => {
                    let port = handle.get_local().port();
                    if let Some(val) = adapter.0.get(&port) {
                        let service = val.value();
                        Some(service.handle_closed(handle,
                                                   SocketStatus::Closed(Ok(()))))
                    } else {
                        None
                    }
                },
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed()
    }

    fn timeouted(&self,
                 handle: SocketHandle<Self::Connect>,
                 event: SocketEvent) -> BoxFuture<'static, ()> {
        let adapter = self.clone();
        async move {
            let port = handle.get_local().port();
            let task = if let Some(val) = adapter.0.get(&port) {
                let service = val.value();
                Some(service.handle_timeouted(handle,
                                              SocketStatus::Timeout(event)))
            } else {
                None
            };

            if let Some(task) = task {
                task.await;
            }
        }.boxed()
    }
}

impl<S: Socket> PortsAdapter<S> {
    /// 构建一个Tcp端口适配器
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
/// Tcp端口异步服务工厂
///
pub struct PortsAdapterFactory<S: Socket> {
    inner: PortsAdapter<S>, //Tcp端口适配器
}

impl<S: Socket> SocketAdapterFactory for PortsAdapterFactory<S> {
    type Connect = S;
    type Adapter = PortsAdapter<S>;

    fn get_instance(&self) -> Self::Adapter {
        self.inner.clone()
    }
}

impl<S: Socket> PortsAdapterFactory<S> {
    /// 构建Tcp端口异步服务工厂
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
/// Tcp连接监听器
///
pub struct SocketListener<S: Socket + Stream, F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>> {
    marker: PhantomData<(S, F)>,
}

impl<S, F> SocketListener<S, F>
    where S: Socket + Stream,
          F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>, {
    /// 绑定指定配置的Tcp连接监听器
    pub fn bind(mut runtimes: Vec<WorkerRuntime<()>>,
                factory: F,                     //Tcp端口适配器工厂
                config: SocketConfig,           //连接配置
                init_cap: usize,                //连接池初始容量
                stack_size: usize,              //线程堆栈大小
                event_size: usize,              //同时处理的事件数
                recv_frame_buf_size: usize,     //接收帧缓冲数量
                readed_read_size_limit: usize,  //已读读缓冲大小限制
                readed_write_size_limit: usize, //已读写缓冲大小限制
                timeout: Option<usize>          //事件轮询超时时长
    ) -> Result<SocketDriver<S, PortsAdapter<S>>> {
        if runtimes.is_empty() {
            //至少需要一个工作者
            return Err(Error::new(ErrorKind::Other, format!("Bind listener failed, reason: require runtime")));
        }

        let addrs = config.addrs();
        let (sender, receiver) = unbounded();
        let binds: Vec<(SocketAddr, Sender<S>)> = addrs.iter().map(|(addr, _tls_cfg)| {
            (addr.clone(), sender.clone())
        }).collect();

        let acceptor;
        let rt_size = runtimes.len(); //获取工作者数量
        let mut pools = Vec::with_capacity(rt_size);
        let mut driver = SocketDriver::new(&binds[..]);
        match Acceptor::bind(&addrs[..], &driver) {
            Err(e) => {
                return Err(e);
            },
            Ok(a) => {
                //创建工作者数量的连接池，共用一个写缓冲池
                acceptor = a;
                for index in 0..rt_size {
                    match TcpSocketPool::with_capacity(index as u8,
                                                       acceptor.get_name(),
                                                       receiver.clone(),
                                                       config.clone(),
                                                       init_cap) {
                        Err(e) => {
                            return Err(e);
                        },
                        Ok(pool) => {
                            pools.push(pool);
                        },
                    }
                }
            },
        }

        driver.set_controller(acceptor.get_controller()); //设置连接驱动的控制器
        //为所有连接池，设置不同端口适配器的连接驱动，并启动所有连接池
        for pool in pools {
            let mut driver_clone = driver.clone();
            driver_clone.set_adapter(factory.get_instance()); //设置连接驱动的端口适配器
            if let Some(rt) = runtimes.pop() {
                if let Err(e) = pool.run(rt,
                                         driver_clone,
                                         event_size,
                                         timeout) {
                    //启动连接池失败
                    return Err(e);
                }
            }
        }

        //启动接受器的监听
        if let Err(e) = acceptor.listen(stack_size,
                                        event_size,
                                        recv_frame_buf_size,
                                        readed_read_size_limit,
                                        readed_write_size_limit,
                                        timeout) {
            //启动接受器失败
            return Err(e);
        }

        Ok(driver)
    }
}