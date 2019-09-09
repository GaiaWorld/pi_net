use std::mem;
use std::task::Waker;
use std::cell::RefCell;
use std::rc::{Weak, Rc};
use std::future::Future;
use std::net::SocketAddr;
use std::any::{Any, TypeId};
use std::io::{Result, Error};
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::collections::{hash_map::Entry,  HashMap};

use mio::Token;
use fnv::FnvBuildHasher;
use crossbeam_channel::{Sender, unbounded};
use futures::future::BoxFuture;

use apm::common::SysStat;
use r#async::{AsyncSpawner, AsyncExecutor,
              local_queue::{LocalQueueSpawner, LocalQueue}, task::LocalTask};

use crate::acceptor::Acceptor;
use crate::connect_pool::TcpSocketPool;
use crate::buffer_pool::WriteBufferPool;
use crate::driver::{Socket, Stream, SocketAdapter, SocketAdapterFactory, AsyncIOWait, AsyncService, SocketStatus, SocketHandle, SocketConfig, SocketDriver, AsyncServiceFactory};

/*
* Tcp异步任务等待表
*/
type AsyncWaits = Rc<RefCell<HashMap<usize, Waker, FnvBuildHasher>>>;

/*
* Tcp异步任务等待表句柄
*/
pub struct AsyncWaitsHandle(Weak<RefCell<HashMap<usize, Waker, FnvBuildHasher>>>);

unsafe impl Send for AsyncWaitsHandle {}
unsafe impl Sync for AsyncWaitsHandle {}

impl Clone for AsyncWaitsHandle {
    fn clone(&self) -> Self {
        AsyncWaitsHandle(self.0.clone())
    }
}

impl AsyncIOWait for AsyncWaitsHandle {
    fn io_wait(&self, token: &Token, waker: Waker) {
        if let Some(waits) = self.0.upgrade() {
            //获取到异步任务等待表，则将待唤醒的异步任务加入等待表
            waits.borrow_mut().insert(token.0, waker);
        }
    }
}

/*
* Tcp异步处理适配器
*/
pub struct AsyncAdapter<S, O>
    where S: Socket,
          O: 'static, {
    waits:      AsyncWaits,                                                                         //Tcp连接待处理表
    tasks:      RefCell<LocalQueue<LocalTask<()>, ()>>,                                             //Tcp连接任务队列
    spawner:    Rc<LocalQueueSpawner<LocalTask<()>, ()>>,                                           //Tcp连接任务派发器
    service:    Box<AsyncService<S, AsyncWaitsHandle, Out = O, Future = BoxFuture<'static, O>>>,    //Tcp连接异步服务
    marker:     PhantomData<S>,
}

impl<S, O> SocketAdapter for AsyncAdapter<S, O>
    where S: Socket,
          O: 'static, {
    type Connect = S;

    fn connected(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        let mut r = Ok(());
        let handle = match result {
            Err((s, e)) => {
                r = Err(e);
                s
            },
            Ok(s) => {
                s
            }
        };

        async_run::<S, O>(&self.waits, &self.tasks, &self.spawner, &self.service, handle, SocketStatus::Connected(r));
    }

    fn readed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        let mut r = Ok(());
        let handle = match result {
            Err((s, e)) => {
                r = Err(e);
                s
            },
            Ok(s) => {
                s
            }
        };

        async_run::<S, O>(&self.waits, &self.tasks, &self.spawner, &self.service, handle, SocketStatus::Readed(r));
    }

    fn writed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        let mut r = Ok(());
        let handle = match result {
            Err((s, e)) => {
                r = Err(e);
                s
            },
            Ok(s) => {
                s
            }
        };

        async_run::<S, O>(&self.waits, &self.tasks, &self.spawner, &self.service, handle, SocketStatus::Writed(r));
    }

    fn closed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        let mut r = Ok(());
        let handle = match result {
            Err((s, e)) => {
                r = Err(e);
                s
            },
            Ok(s) => {
                s
            }
        };

        async_run::<S, O>(&self.waits, &self.tasks, &self.spawner, &self.service, handle, SocketStatus::Closed(r));
    }
}

//运行异步任务
fn async_run<S, O>(waits: &Rc<RefCell<HashMap<usize, Waker, FnvBuildHasher>>>,
                   tasks: &RefCell<LocalQueue<LocalTask<()>, ()>>,
                   spawner: &Rc<LocalQueueSpawner<LocalTask<()>, ()>>,
                   service: &Box<AsyncService<S, AsyncWaitsHandle, Out = O, Future = BoxFuture<'static, O>>>,
                   handle: SocketHandle<S>,
                   status: SocketStatus)
    where S: Socket,
          O: 'static, {
    if let Some(socket) = handle.as_handle() {
        let id = socket.borrow().get_token().unwrap().0;
        //有效的Tcp连接
        let mut w = waits.borrow_mut();
        match w.entry(id) {
            Entry::Vacant(_v) => {
                //创建新的异步任务
                let waits = AsyncWaitsHandle(Rc::downgrade(waits));
                let future = match &status {
                    SocketStatus::Connected(_) => service.handle_connected(handle, waits, status),
                    SocketStatus::Readed(_) => service.handle_readed(handle, waits, status),
                    SocketStatus::Writed(_) => service.handle_writed(handle, waits, status),
                    SocketStatus::Closed(_) => service.handle_closed(handle, waits, status),
                };
                mem::drop(w); //因为后续操作在异步等待队列引用的作用域内，所以必须显示释放异步等待队列引用，以保证后续可以继续借用异步等待队列
                let task = LocalTask::new(spawner.clone(), async move {
                    future.await;
                });

                if let Err(e) = spawner.spawn(task) {
                    panic!("run async task failed, reason: {:?}", e);
                }
            },
            Entry::Occupied(o) => {
                //唤醒待完成异步任务
                let waker = o.remove();
                mem::drop(w); //因为后续操作在异步等待队列引用的作用域内，所以必须显示释放异步等待队列引用，以保证后续可以继续借用异步等待队列
                waker.wake();
            },
        }

        //执行异步任务
        tasks.borrow_mut().run_once();
    } else {
        panic!("run async task failed, reason: invalid socket");
    }
}

impl<S, O> AsyncAdapter<S, O>
    where S: Socket,
          O: 'static, {
    //构建一个指定异步服务的Tcp异步处理适配器
    pub fn with_service(service: Box<AsyncService<S, AsyncWaitsHandle, Out = O, Future = BoxFuture<'static, O>>>) -> Self {
        let queue = LocalQueue::with_capacity(256);
        let spawner = queue.get_spawner();

        AsyncAdapter {
            waits: Rc::new(RefCell::new(HashMap::with_hasher(FnvBuildHasher::default()))),
            tasks: RefCell::new(queue),
            spawner: Rc::new(spawner),
            service,
            marker: PhantomData,
        }
    }
}

/*
* Tcp端口适配器
*/
pub struct PortsAdapter<S: Socket> {
    ports:  HashMap<u16, Box<dyn SocketAdapter<Connect = S>>, FnvBuildHasher>,  //端口适配器表
}

impl<S: Socket> SocketAdapter for PortsAdapter<S> {
    type Connect = S;

    fn connected(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        match result {
            Err((socket_ref, e)) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.connected(Err((socket_ref, e)));
                    }
                }
            },
            Ok(socket_ref) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.connected(Ok(socket_ref));
                    }
                }
            },
        }
    }

    fn readed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        match result {
            Err((socket_ref, e)) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.readed(Err((socket_ref, e)));
                    }
                }
            },
            Ok(socket_ref) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.readed(Ok(socket_ref));
                    }
                }
            },
        }
    }

    fn writed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        match result {
            Err((socket_ref, e)) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.writed(Err((socket_ref, e)));
                    }
                }
            },
            Ok(socket_ref) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.writed(Ok(socket_ref));
                    }
                }
            },
        }
    }

    fn closed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) {
        match result {
            Err((socket_ref, e)) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.closed(Err((socket_ref, e)));
                    }
                }
            },
            Ok(socket_ref) => {
                if let Some(socket) = socket_ref.as_handle() {
                    let port = socket.borrow().get_local().port();
                    if let Some(adapter) = self.ports.get(&port) {
                        adapter.closed(Ok(socket_ref));
                    }
                }
            },
        }
    }
}

impl<S: Socket> PortsAdapter<S> {
    //构建一个Tcp端口适配器
    pub fn new() -> Self {
        PortsAdapter {
            ports: HashMap::with_hasher(FnvBuildHasher::default()),
        }
    }

    //获取所有端口
    pub fn ports(&self) -> Vec<u16> {
        self.ports.keys().map(|port| {
            port.clone()
        }).collect::<Vec<u16>>()
    }

    //设置指定端口的事件适配器
    pub fn set_adapter(&mut self, port: u16, adapter: Box<dyn SocketAdapter<Connect = S>>) {
        self.ports.insert(port, adapter);
    }
}

/*
* Tcp端口异步服务工厂
*/
pub struct AsyncPortsFactory<S: Socket> {
    ports_factory: HashMap<u16, Box<dyn AsyncServiceFactory<Connect = S, Waits = AsyncWaitsHandle, Out = (), Future = BoxFuture<'static, ()>>>, FnvBuildHasher>,   //端口异步服务工厂表
}

impl<S: Socket> SocketAdapterFactory for AsyncPortsFactory<S> {
    type Connect = S;
    type Adapter = PortsAdapter<S>;

    fn get_instance(&self) -> Self::Adapter {
        let mut ports_adapter = Self::Adapter::new();

        for (port, factory) in &self.ports_factory {
            let service = factory.new_service();
            let async_adapter = Box::new(AsyncAdapter::<S, ()>::with_service(service));
            ports_adapter.set_adapter(port.clone(), async_adapter);
        }

        ports_adapter
    }
}

impl<S: Socket> AsyncPortsFactory<S> {
    //构建Tcp端口异步服务工厂
    pub fn new() -> Self {
        AsyncPortsFactory {
            ports_factory: HashMap::with_hasher(FnvBuildHasher::default()),
        }
    }

    //获取绑定端口的数量
    pub fn size(&self) -> usize {
        self.ports_factory.len()
    }

    //是否绑定了指定端口的异步服务工厂
    pub fn is_bind(&self, port: u16) -> bool {
        self.ports_factory.contains_key(&port)
    }

    //为指定端口绑定指定的异步服务工厂，返回上次绑定的异步服务工厂
    pub fn bind(&mut self, port: u16, factory: Box<dyn AsyncServiceFactory<Connect = S, Waits = AsyncWaitsHandle, Out = (), Future = BoxFuture<'static, ()>>>) -> Option<Box<dyn AsyncServiceFactory<Connect = S, Waits = AsyncWaitsHandle, Out = (), Future = BoxFuture<'static, ()>>>> {
        self.ports_factory.insert(port, factory)
    }

    //为指定端口解绑定指定的异步服务工厂
    pub fn unbind(&mut self, port: u16) -> Option<Box<dyn AsyncServiceFactory<Connect = S, Waits = AsyncWaitsHandle, Out = (), Future = BoxFuture<'static, ()>>>> {
        self.ports_factory.remove(&port)
    }
}

/*
* Tcp连接监听器
*/
pub struct SocketListener<S: Socket + Stream, F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>> {
    marker: PhantomData<(S, F)>,
}

impl<S, F> SocketListener<S, F>
    where S: Socket + Stream,
          F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>, {
    //绑定指定配置的Tcp连接监听器
    pub fn bind(factory: F,                 //Tcp端口适配器工厂
                buffer: WriteBufferPool,    //写缓冲池
                config: SocketConfig,       //连接配置
                init_cap: usize,            //连接池初始容量
                stack_size: usize,          //线程堆栈大小
                event_size: usize,          //同时处理的事件数
                timeout: Option<usize>      //事件轮询超时时长
    ) -> Result<SocketDriver<S, PortsAdapter<S>>> {
        let addrs = config.addrs();
        let (sender, receiver) = unbounded();
        let binds: Vec<(SocketAddr, Sender<S>)> = addrs.iter().map(|addr| {
            (addr.clone(), sender.clone())
        }).collect();

        let acceptor;
        let sys = SysStat::new();
        let processor = sys.processor_count();
        let mut pools = Vec::with_capacity(processor);
        let mut driver = SocketDriver::new(&binds[..]);
        match Acceptor::bind(&addrs[..], &driver) {
            Err(e) => {
                return Err(e);
            },
            Ok(a) => {
                //创建当前系统cpu核心数的连接池，共用一个写缓冲池
                acceptor = a;
                for index in 0..processor {
                    match TcpSocketPool::with_capacity(index as u8,
                                                       acceptor.get_name(),
                                                       receiver.clone(),
                                                       config.clone(),
                                                       buffer.clone(),
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
            if let Err(e) = pool.run(driver_clone, stack_size, event_size, timeout) {
                //启动连接池失败
                return Err(e);
            }
        }

        //启动接受器的监听
        if let Err(e) = acceptor.listen(stack_size, event_size, timeout) {
            //启动接受器失败
            return Err(e);
        }

        Ok(driver)
    }
}