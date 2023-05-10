#![allow(unused)]
#![feature(is_some_and)]

use std::rc::Rc;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use mio::{Token, Interest, Poll,
          net::UdpSocket};
use futures::future::LocalBoxFuture;
use crossbeam_channel::Sender;
use bytes::Buf;

#[macro_use]
extern crate lazy_static;

use pi_async::{lock::spin_lock::SpinLock,
               rt::serial_local_thread::LocalTaskRuntime};
use pi_hash::XHashMap;

pub mod utils;
pub mod connect;
pub mod connect_pool;
pub mod server;
pub mod client;

use utils::{UdpMultiInterface, UdpSocketStatus, SocketContext, AsyncServiceContext};

///
/// Udp连接
///
pub trait Socket: Sized + Send + 'static {
    /// 构建一个Udp连接
    fn new(uid: usize,
           local: SocketAddr,
           remote: Option<SocketAddr>,
           token: Option<Token>,
           socket: UdpSocket,
           read_packet_size: usize,
           write_packet_size: usize) -> Self;

    /// 线程安全的判断是否已关闭Udp连接
    fn is_closed(&self) -> bool;

    /// 获取连接所在运行时
    fn get_runtime(&self) -> Option<&LocalTaskRuntime<()>>;

    /// 设置连接所在运行时
    fn set_runtime(&mut self, rt: LocalTaskRuntime<()>);

    /// 获取当前Udp连接的句柄
    fn get_handle(&self) -> SocketHandle<Self>;

    /// 设置Udp连接句柄
    fn set_handle(&mut self, shared: &Arc<RefCell<Self>>);

    /// 获取内部连接的只读引用
    fn get_socket(&self) -> &SpinLock<UdpSocket>;

    /// 获取连接唯一id
    fn get_uid(&self) -> usize;

    /// 获取连接本地地址
    fn get_local(&self) -> &SocketAddr;

    /// 获取连接远端地址
    fn get_remote(&self) -> Option<&SocketAddr>;

    /// 获取连接令牌
    fn get_token(&self) -> Option<&Token>;

    /// 设置连接令牌，返回上个连接令牌
    fn set_token(&mut self, token: Option<Token>) -> Option<Token>;

    /// 获取连接状态
    fn get_status(&self) -> &UdpSocketStatus;

    /// 单播绑定指定的对端地址，参数为空则表示不绑定指定的对端地址
    fn bind(&mut self, peer: Option<SocketAddr>) -> Result<()>;

    /// 设置非多播报文的生存周期
    fn set_ttl(&self, ttl: u32) -> Result<u32>;

    /// 加入指定的多播组
    /// 指定多播报文的生存周期，默认为1
    /// 指定多播消息是否会发回源地址，
    fn join_multicast_group(&mut self,
                            address: SocketAddr,
                            interface: UdpMultiInterface,
                            ttl: u32,
                            is_loop: bool) -> Result<()>;

    /// 离开指定的多播组
    fn leave_multicast_group(&mut self,
                             address: SocketAddr,
                             interface: UdpMultiInterface) -> Result<()>;

    /// 允许当前连接在局域网内广播
    fn enable_broadcast(&mut self) -> Result<()>;

    /// 禁止当前连接在局域网内广播
    fn disable_boradcast(&mut self) -> Result<()>;

    //获取连接上下文只读引用
    fn get_context(&self) -> &SocketContext;

    //获取连接上下文的可写引用
    fn get_context_mut(&mut self) -> &mut SocketContext;

    /// 获取当前流对哪类事件感兴趣，如果当前事件与上次事件相同，则返回None
    fn get_interest(&self) -> Option<Interest>;

    /// 设置当前流对哪类事件感兴趣
    fn set_interest(&self, ready: Interest);

    /// 获取读取的块大小
    fn get_read_block_len(&self) -> usize;

    /// 设置读取的块大小
    fn set_read_block_len(&mut self, len: usize);

    /// 获取写入的块大小
    fn get_write_block_len(&self) -> usize;

    /// 设置写入的块大小
    fn set_write_block_len(&mut self, len: usize);

    /// 设置连接绑定的轮询器
    fn set_poll(&mut self, poll: Arc<SpinLock<Poll>>);

    /// 设置关闭事件监听器
    fn set_close_listener(&mut self, listener: Option<Sender<(Token, Result<()>)>>);

    /// 通知连接写就绪，可以开始发送指定的数据
    fn write_ready<B>(&mut self,
                      buf: B,
                      peer: Option<SocketAddr>) -> Result<()>
        where B: Buf + Send + 'static;

    /// 接收流中的数据，返回成功，则表示本次接收了需要的字节数，并返回本次接收的字节数，否则返回接收错误
    fn recv(&mut self) -> Result<(Vec<u8>, Option<SocketAddr>)>;

    /// 发送数据到流，返回成功，则返回本次发送了多少字节数，否则返回发送错误原因
    fn send(&mut self) -> Result<usize>;

    /// 线程安全的关闭Udp连接
    fn close(&mut self, reason: Result<()>) -> Result<()>;
}

///
/// Udp连接事件适配器
///
pub trait SocketAdapter: Send + Sync + 'static {
    type Connect: Socket;

    /// 绑定连接事件适配器所在运行时
    fn bind_runtime(&mut self, rt: &LocalTaskRuntime<()>);

    /// 已绑定本地端口的Udp连接
    fn binded(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;

    /// 已读
    fn readed(&self,
              result: GenResult<(SocketHandle<Self::Connect>, Vec<u8>, Option<SocketAddr>), (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;

    /// 已写
    fn writed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;

    /// 已关闭
    fn closed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;
}

///
/// Udp连接异步服务
///
pub trait AsyncService<S: Socket>: Send + Sync + 'static {
    /// 绑定异步服务所在的运行时
    fn bind_runtime(&mut self, rt: LocalTaskRuntime<()>);

    /// 获取Udp连接异步服务上下文的只读引用
    fn get_context(&self) -> Option<&AsyncServiceContext<S>>;

    /// 设置Udp连接异步服务上下文件
    fn set_context(&mut self, context: AsyncServiceContext<S>);

    /// 异步处理已绑定本地端口的Udp连接
    fn handle_binded(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已读
    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已写
    fn handle_writed(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已关闭
    fn handle_closed(&self,
                     handle: SocketHandle<S>,
                     result: Result<()>) -> LocalBoxFuture<'static, ()>;
}

///
/// Udp连接适配器工厂
///
pub trait SocketAdapterFactory {
    type Connect: Socket;
    type Adapter: SocketAdapter<Connect = Self::Connect>;

    //获取Udp连接适配器实例
    fn get_instance(&self) -> Self::Adapter;
}

///
/// Udp连接句柄
///
pub struct SocketHandle<S: Socket>(Arc<SocketImage<S>>);

unsafe impl<S: Socket> Send for SocketHandle<S> {}
unsafe impl<S: Socket> Sync for SocketHandle<S> {}

impl<S: Socket> Clone for SocketHandle<S> {
    fn clone(&self) -> Self {
        SocketHandle(self.0.clone())
    }
}

impl<S: Socket> SocketHandle<S> {
    /// 构建Udp连接句柄
    pub fn new(image: SocketImage<S>) -> Self {
        SocketHandle(Arc::new(image))
    }

    /// 线程安全的判断连接是否关闭
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    /// 线程安全的获取连接令牌
    pub fn get_token(&self) -> &Token {
        &self.0.token
    }

    /// 线程安全的获取连接唯一id
    pub fn get_uid(&self) -> usize {
        self.0.uid
    }

    /// 线程安全的获取连接本地地址
    pub fn get_local(&self) -> &SocketAddr {
        &self.0.local
    }

    /// 线程安全的获取连接远端地址
    pub fn get_remote(&self) -> Option<&SocketAddr> {
        unsafe {
            (&*self.0.inner).get_remote()
        }
    }

    /// 非线程安全的获取连接状态
    pub fn get_status(&self) -> &UdpSocketStatus {
        unsafe {
            (&*self.0.inner).get_status()
        }
    }

    /// 非线程安全的单播绑定指定的对端地址，参数为空则表示不绑定指定的对端地址
    pub fn bind(&self, peer: Option<SocketAddr>) -> Result<()> {
        unsafe {
            (&mut *(self.0.inner as *mut S)).bind(peer)
        }
    }

    /// 线程安全的设置非多播报文的生存周期
    pub fn set_ttl(&self, ttl: u32) -> Result<u32> {
        unsafe {
            (&*self.0.inner).set_ttl(ttl)
        }
    }

    /// 非线程安全的加入指定的多播组
    /// 指定多播报文的生存周期，默认为1
    /// 指定多播消息是否会发回源地址，
    pub fn join_multicast_group(&self,
                                address: SocketAddr,
                                interface: UdpMultiInterface,
                                ttl: u32,
                                is_loop: bool) -> Result<()> {
        unsafe {
            (&mut *(self.0.inner as *mut S)).join_multicast_group(address,
                                                                  interface,
                                                                  ttl,
                                                                  is_loop)
        }
    }

    /// 非线程安全的离开指定的多播组
    pub fn leave_multicast_group(&self,
                                 address: SocketAddr,
                                 interface: UdpMultiInterface) -> Result<()> {
        unsafe {
            (&mut *(self.0.inner as *mut S)).leave_multicast_group(address,
                                                                   interface)
        }
    }

    /// 非线程安全的允许当前连接在局域网内广播
    pub fn enable_broadcast(&self) -> Result<()> {
        unsafe {
            (&mut *(self.0.inner as *mut S)).enable_broadcast()
        }
    }

    /// 非线程安全的禁止当前连接在局域网内广播
    pub fn disable_boradcast(&self) -> Result<()> {
        unsafe {
            (&mut *(self.0.inner as *mut S)).disable_boradcast()
        }
    }

    /// 非线程安全的获取Udp连接上下文的只读引用
    pub fn get_context(&self) -> &SocketContext {
        unsafe {
            (&*self.0.inner).get_context()
        }
    }

    /// 非线程安全的获取Udp连接上下文的可写引用
    pub fn get_context_mut(&self) -> &mut SocketContext {
        unsafe {
            (&mut *(self.0.inner as *mut S)).get_context_mut()
        }
    }

    /// 线程安全的写
    pub fn write_ready<B>(&self,
                          buf: B,
                          peer: Option<SocketAddr>) -> Result<()>
        where B: Buf + Send + 'static {
        unsafe {
            (&mut *(self.0.inner as *mut S)).write_ready(buf, peer)
        }
    }

    /// 线程安全的关闭连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        unsafe {
            (&mut *(self.0.inner as *mut S)).close(reason)
        }
    }
}

///
/// Udp连接镜像
///
pub struct SocketImage<S: Socket> {
    inner:          *const S,                                       //Udp连接指针
    uid:            usize,                                          //Udp连接唯一id
    local:          SocketAddr,                                     //Udp连接本地地址
    token:          Token,                                          //Udp连接令牌
    closed:         Rc<AtomicBool>,                                 //Udp连接关闭状态
    close_listener: Sender<(Token, Result<()>)>,                    //关闭事件监听器
}

unsafe impl<S: Socket> Send for SocketImage<S> {}
unsafe impl<S: Socket> Sync for SocketImage<S> {}

impl<S: Socket> SocketImage<S> {
    /// 构建Udp连接镜像
    pub fn new(shared: &Arc<RefCell<S>>,
               uid: usize,
               local: SocketAddr,
               token: Token,
               closed: Rc<AtomicBool>,
               close_listener: Sender<(Token, Result<()>)>) -> Self {
        SocketImage {
            inner: shared.as_ptr() as *const S,
            uid,
            local,
            token,
            closed,
            close_listener,
        }
    }
}

///
/// Udp连接驱动器，用于处理接收和发送的二进制数据
///
pub struct SocketDriver<S: Socket, A: SocketAdapter<Connect = S>> {
    addrs:      Rc<XHashMap<SocketAddr, usize>>,                                //驱动器绑定的地址
    router:     Rc<Vec<Sender<S>>>,                                             //连接路由表
    adapter:    Option<Arc<A>>,                                                 //连接协议适配器
}

unsafe impl<S: Socket, A: SocketAdapter<Connect = S>> Send for SocketDriver<S, A> {}
unsafe impl<S: Socket, A: SocketAdapter<Connect = S>> Sync for SocketDriver<S, A> {}

impl<S: Socket, A: SocketAdapter<Connect = S>> Clone for SocketDriver<S, A> {
    fn clone(&self) -> Self {
        SocketDriver {
            addrs: self.addrs.clone(),
            router: self.router.clone(),
            adapter: self.adapter.clone(),
        }
    }
}

impl<S: Socket, A: SocketAdapter<Connect = S>> SocketDriver<S, A> {
    /// 构建一个Udp连接驱动器
    pub fn new(bind: &[SocketAddr],
               router: Vec<Sender<S>>) -> Self {
        let size = bind.len();
        let mut map = XHashMap::default();

        let mut index: usize = 0;
        for addr in bind {
            map.insert(addr.clone(), index);
            index += 1;
        }

        SocketDriver {
            addrs: Rc::new(map),
            router: Rc::new(router),
            adapter: None,
        }
    }

    /// 获取Udp连接驱动器绑定的地址
    pub fn get_addrs(&self) -> Vec<SocketAddr> {
        self.addrs.keys().map(|addr| {
            addr.clone()
        }).collect::<Vec<SocketAddr>>()
    }

    /// 将连接路由到对应的连接池中等待处理
    pub fn route(&self, mut socket: S) -> Result<()> {
        if let Some(Token(id)) = socket.set_token(None) {
            let router = &self.router[id % self.router.len()];

            match router.try_send(socket) {
                Err(e) => {
                    Err(Error::new(ErrorKind::BrokenPipe,
                                   format!("tcp socket route failed, e: {:?}",
                                           e)))
                },
                Ok(_) => Ok(()),
            }
        } else {
            Err(Error::new(ErrorKind::Interrupted,
                           format!("tcp socket route failed, e: invalid accept token")))
        }
    }

    /// 获取连接适配器
    pub fn get_adapter(&self) -> &A {
        self.adapter.as_ref().unwrap()
    }

    /// 复制连接适配器
    pub fn clone_adapter(&self) -> Arc<A> {
        self
            .adapter
            .as_ref()
            .unwrap()
            .clone()
    }

    /// 设置连接适配器
    pub fn set_adapter(&mut self, adapter: A) {
        self.adapter = Some(Arc::new(adapter));
    }
}

