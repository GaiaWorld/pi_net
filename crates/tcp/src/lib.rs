#![allow(warnings)]
#![feature(is_some_and)]
#![feature(io_slice_advance)]

use std::ptr;
use std::rc::Rc;
use std::task::Waker;
use std::str::FromStr;
use std::future::Future;
use std::cell::UnsafeCell;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::net::{SocketAddr, IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::AtomicUsize;

use futures::future::LocalBoxFuture;
use crossbeam_channel::Sender;
use mio::{Token, Interest, Poll,
          net::TcpStream};
use bytes::BytesMut;

use pi_async_rt::rt::{serial::AsyncValueNonBlocking,
                      serial_local_thread::LocalTaskRuntime};
use pi_hash::XHashMap;

#[macro_use]
extern crate lazy_static;

pub mod acceptor;
pub mod connect;
pub mod tls_connect;
pub mod connect_pool;
pub mod server;
pub mod utils;

use utils::{TlsConfig, SocketContext, Hibernate, Ready};

///
/// 默认的ipv4地址
///
pub const DEFAULT_TCP_IP_V4: &str = "0.0.0.0";

///
/// 默认的ipv6地址
///
pub const DEFAULT_TCP_IP_V6: &str = "::";

///
/// 默认的Tcp端口
///
pub const DEFAULT_TCP_PORT: u16 = 38080;

///
/// 默认缓冲区大小，16KB
///
pub const DEFAULT_BUFFER_SIZE: usize = 16384;

///
/// Tcp连接事件适配器
///
pub trait SocketAdapter: Send + Sync + 'static {
    type Connect: Socket;   //保证外部只可以操作Socket，而无法操作Stream

    /// 已连接
    fn connected(&self,
                 result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;

    /// 已读
    fn readed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;

    /// 已写
    fn writed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;

    /// 已关闭
    fn closed(&self,
              result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>) -> LocalBoxFuture<'static, ()>;

    /// 已超时
    fn timeouted(&self,
                 handle: SocketHandle<Self::Connect>,
                 event: SocketEvent) -> LocalBoxFuture<'static, ()>;
}

///
/// Tcp连接适配器工厂
///
pub trait SocketAdapterFactory {
    type Connect: Socket;
    type Adapter: SocketAdapter<Connect = Self::Connect>;

    //获取Tcp连接适配器实例
    fn get_instance(&self) -> Self::Adapter;
}

/*
* Tcp连接通用选项
*/
#[derive(Clone)]
pub struct SocketOption {
    pub recv_buffer_size:       usize,  //Socket接收缓冲大小，单位字节
    pub send_buffer_size:       usize,  //Socket发送缓冲大小，单位字节
    pub read_buffer_capacity:   usize,  //Socket读缓冲容量，单位字节
    pub write_buffer_capacity:  usize,  //Socket写缓冲容量，单位次
}

impl Default for SocketOption {
    fn default() -> Self {
        SocketOption {
            recv_buffer_size:       DEFAULT_BUFFER_SIZE, //默认的Socket接收缓冲大小，16KB
            send_buffer_size:       DEFAULT_BUFFER_SIZE, //默认的Socket发送缓冲大小，16KB
            read_buffer_capacity:   DEFAULT_BUFFER_SIZE, //默认的Socket读缓冲容量，16KB
            write_buffer_capacity:  16,                  //默认的Socket写缓冲次数，16次
        }
    }
}

///
/// Tcp连接配置
///
#[derive(Clone)]
pub enum SocketConfig {
    Raw(Vec<u16>, SocketOption),                            //非安全的tcp连接共享配置，可以同时在ipv4和ipv6上，绑定相同port
    Tls(Vec<(u16, TlsConfig)>, SocketOption),               //安全的tcp连接共享配置，可以同时在ipv4和ipv6上，绑定相同的port
    RawIpv4(IpAddr, Vec<u16>, SocketOption),                //非安全的tcp连接兼容配置，兼容ipv4和ipv4映射的ipv6
    TlsIpv4(IpAddr, Vec<(u16, TlsConfig)>, SocketOption),   //安全的tcp连接兼容配置，兼容ipv4和ipv4映射的ipv6
    RawIpv6(Ipv6Addr, Vec<u16>, SocketOption),              //非安全的tcp连接ipv6独占配置，可以与兼容的ipv4配置，绑定相同port
    TlsIpv6(Ipv6Addr, Vec<(u16, TlsConfig)>, SocketOption), //安全的tcp连接ipv6独占配置，可以与兼容的ipv4配置，绑定相同port
}

impl Default for SocketConfig {
    fn default() -> Self {
        SocketConfig::new(DEFAULT_TCP_IP_V4, &[DEFAULT_TCP_PORT])
    }
}

impl SocketConfig {
    /// 建一个指定ip和端口的Tcp连接兼容配置
    pub fn new(ip: &str, port: &[u16]) -> Self {
        if ip == DEFAULT_TCP_IP_V6 {
            //在本地ipv4和ipv6地址上绑定相同的port
            SocketConfig::Raw(port.to_vec(), SocketOption::default())
        } else {
            //在指定的本地地址上绑定port
            let addr: IpAddr;
            if let Ok(r) = Ipv4Addr::from_str(ip) {
                addr = IpAddr::V4(r);
            } else {
                if let Ok(r) = Ipv6Addr::from_str(ip) {
                    addr = IpAddr::V6(r);
                } else {
                    panic!("invalid ip");
                }
            }

            SocketConfig::RawIpv4(addr, port.to_vec(), SocketOption::default())
        }
    }

    /// 构建一个指定ip、端口和TLS配置的Tcp连接兼容配置
    pub fn with_tls(ip: &str, port: &[(u16, TlsConfig)]) -> Self {
        if ip == DEFAULT_TCP_IP_V6 {
            //在本地ipv4和ipv6地址上绑定相同的port
            SocketConfig::Tls(port.to_vec(), SocketOption::default())
        } else {
            //在指定的本地地址上绑定port
            let addr: IpAddr;
            if let Ok(r) = Ipv4Addr::from_str(ip) {
                addr = IpAddr::V4(r);
            } else {
                if let Ok(r) = Ipv6Addr::from_str(ip) {
                    addr = IpAddr::V6(r);
                } else {
                    panic!("invalid ip");
                }
            }

            SocketConfig::TlsIpv4(addr, port.to_vec(), SocketOption::default())
        }
    }

    /// 将地址转换为ipv6
    pub fn into_ipv6(self) -> Self {
        match self {
            SocketConfig::RawIpv4(ip, ports, option) => {
                //非安全的兼容配置，则转换为非安全的ipv6独占配置
                match ip {
                    IpAddr::V4(addr) => {
                        SocketConfig::RawIpv6(addr.to_ipv6_mapped(), ports, option)
                    },
                    IpAddr::V6(addr) => {
                        SocketConfig::RawIpv6(addr, ports, option)
                    },
                }
            },
            SocketConfig::TlsIpv4(ip, ports, option) => {
                //安全的兼容配置，则转换为安全的ipv6独占配置
                match ip {
                    IpAddr::V4(addr) => {
                        SocketConfig::TlsIpv6(addr.to_ipv6_mapped(), ports, option)
                    },
                    IpAddr::V6(addr) => {
                        SocketConfig::TlsIpv6(addr, ports, option)
                    },
                }
            },
            config => {
                //已经是ipv6独占或共享配置，则忽略
                config
            }
        }
    }

    /// 获取配置的连接通用选项
    pub fn option(&self) -> SocketOption {
        match self {
            SocketConfig::Raw(_ports, option) => option.clone(),
            SocketConfig::Tls(_ports, option) => option.clone(),
            SocketConfig::RawIpv4(_ip, _ports, option) => option.clone(),
            SocketConfig::TlsIpv4(_ip, _ports, option) => option.clone(),
            SocketConfig::RawIpv6(_ip, _ports, option) => option.clone(),
            SocketConfig::TlsIpv6(_ip, _ports, option) => option.clone(),
        }
    }

    /// 设置配置的连接通用选项
    pub fn set_option(&mut self,
                      recv_buffer_size: usize,
                      send_buffer_size: usize,
                      read_buffer_capacity: usize,
                      write_buffer_capacity: usize) {
        let option = match self {
            SocketConfig::Raw(_ports, option) => {
                option
            },
            SocketConfig::Tls(_ports, option) => {
                option
            },
            SocketConfig::RawIpv4(_ip, _ports, option) => {
                option
            },
            SocketConfig::TlsIpv4(_ip, _ports, option) => {
                option
            },
            SocketConfig::RawIpv6(_ip, _ports, option) => {
                option
            },
            SocketConfig::TlsIpv6(_ip, _ports, option) => {
                option
            },
        };

        option.recv_buffer_size = recv_buffer_size;
        option.send_buffer_size = send_buffer_size;
        option.read_buffer_capacity = read_buffer_capacity;
        option.write_buffer_capacity = write_buffer_capacity;
    }

    /// 获取配置的地址列表
    pub fn addrs(&self) -> Vec<(SocketAddr, TlsConfig)> {
        let mut addrs = Vec::with_capacity(1);

        match self {
            SocketConfig::Raw(ports, _option) => {
                for port in ports {
                    //同时插入默认的ipv4和ipv6地址
                    addrs.push((SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()), TlsConfig::empty()));
                    addrs.push((SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()), TlsConfig::empty()));
                }
            },
            SocketConfig::Tls(ports, _option) => {
                for (port, tls_cfg) in ports {
                    //同时插入默认的ipv4和ipv6地址
                    addrs.push((SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()), tls_cfg.clone()));
                    addrs.push((SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()), tls_cfg.clone()));
                }
            },
            SocketConfig::RawIpv4(ip, ports, _option) => {
                for port in ports {
                    addrs.push((SocketAddr::new(ip.clone(), port.clone()), TlsConfig::empty()))
                }
            },
            SocketConfig::TlsIpv4(ip, ports, _option) => {
                for (port, tls_cfg) in ports {
                    addrs.push((SocketAddr::new(ip.clone(), port.clone()), tls_cfg.clone()))
                }
            },
            SocketConfig::RawIpv6(ip, ports, _option) => {
                for port in ports {
                    addrs.push((SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()), TlsConfig::empty()))
                }
            },
            SocketConfig::TlsIpv6(ip, ports, _option) => {
                for (port, tls_cfg) in ports {
                    addrs.push((SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()), tls_cfg.clone()))
                }
            },
        }

        addrs
    }

    /// 获取配置列表
    pub fn configs(&self) -> Vec<(SocketAddr, SocketOption, TlsConfig)> {
        let mut configs = Vec::with_capacity(1);

        match self {
            SocketConfig::Raw(ports, option) => {
                for port in ports {
                    //同时插入默认的ipv4和ipv6配置
                    configs.push((SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()), option.clone(), TlsConfig::empty()));
                    configs.push((SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()), option.clone(), TlsConfig::empty()));
                }
            },
            SocketConfig::Tls(ports, option) => {
                for (port, tls_cfg) in ports {
                    //同时插入默认的ipv4和ipv6的安全配置
                    configs.push((SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()), option.clone(), tls_cfg.clone()));
                    configs.push((SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()), option.clone(), tls_cfg.clone()));
                }
            },
            SocketConfig::RawIpv4(ip, ports, option) => {
                for port in ports {
                    configs.push((SocketAddr::new(ip.clone(), port.clone()), option.clone(), TlsConfig::empty()))
                }
            },
            SocketConfig::TlsIpv4(ip, ports, option) => {
                for (port, tls_cfg) in ports {
                    configs.push((SocketAddr::new(ip.clone(), port.clone()), option.clone(), tls_cfg.clone()))
                }
            },
            SocketConfig::RawIpv6(ip, ports, option) => {
                for port in ports {
                    configs.push((SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()), option.clone(), TlsConfig::empty()))
                }
            },
            SocketConfig::TlsIpv6(ip, ports, option) => {
                for (port, tls_cfg) in ports {
                    configs.push((SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()), option.clone(), tls_cfg.clone()))
                }
            },
        }

        configs
    }
}

///
/// Tcp连接状态
///
#[derive(Debug)]
pub enum SocketStatus {
    Connected(Result<()>),  //已连接
    Readed(Result<()>),     //已读
    Writed(Result<()>),     //已写
    Closed(Result<()>),     //已关闭
    Timeout(SocketEvent),   //已超时
}

///
/// Tcp连接的事件
///
#[derive(Debug)]
pub struct SocketEvent {
    inner: *mut (), //内部事件
}

unsafe impl Send for SocketEvent {}

impl SocketEvent {
    /// 创建空的事件
    pub fn empty() -> Self {
        SocketEvent {
            inner: ptr::null_mut(),
        }
    }

    /// 判断事件是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.is_null()
    }

    /// 获取事件
    pub fn get<T: 'static>(&self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { *Box::from_raw(self.inner as *mut T) })
    }

    /// 设置事件，如果当前事件不为空，则设置失败
    pub fn set<T: 'static>(&mut self, event: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner = Box::into_raw(Box::new(event)) as *mut ();
        true
    }

    /// 移除事件，如果当前有事件，则返回被移除的事件
    pub fn remove<T: 'static>(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        let result = self.get();
        self.inner = ptr::null_mut();
        result
    }
}

///
/// Tcp流
///
pub trait Stream: Sized + 'static {
    /// 构建Tcp流
    fn new(local: &SocketAddr,
           remote: &SocketAddr,
           token: Option<Token>,
           stream: TcpStream,
           recv_frame_buf_size: usize,
           readed_read_size_limit: usize,
           readed_write_size_limit: usize,
           tls_cfg: TlsConfig) -> Self;

    /// 设置连接所在运行时
    fn set_runtime(&mut self, rt: LocalTaskRuntime<()>);

    /// 设置Tcp流句柄
    fn set_handle(&mut self, shared: &Arc<UnsafeCell<Self>>);

    /// 获取连接流
    fn get_stream_ref(&self) -> &TcpStream;

    /// 获取连接流
    fn get_stream_mut(&mut self) -> &mut TcpStream;

    /// 设置连接令牌，返回上个连接令牌
    fn set_token(&mut self, token: Option<Token>) -> Option<Token>;

    /// 设置连接唯一id，返回上个连接唯一id
    fn set_uid(&mut self, uid: usize) -> Option<usize>;

    /// 获取当前流对哪类事件感兴趣，如果当前事件与上次事件相同，则返回None
    fn get_interest(&self) -> Option<Interest>;

    /// 设置当前流对哪类事件感兴趣
    fn set_interest(&self, ready: Interest);

    /// 获取读取的块大小
    fn get_read_block_len(&self) -> usize;

    /// 设置读取的块大小
    fn set_read_block_len(&self, len: usize);

    /// 获取写入的块大小
    fn get_write_block_len(&self) -> usize;

    /// 设置写入的块大小
    fn set_write_block_len(&self, len: usize);

    /// 设置连接绑定的轮询器
    fn set_poll(&mut self, poll: Rc<UnsafeCell<Poll>>);

    /// 设置写事件监听器
    fn set_write_listener(&mut self, listener: Option<Sender<(Token, Vec<u8>)>>);

    /// 设置关闭事件监听器
    fn set_close_listener(&mut self, listener: Option<Sender<(Token, Result<()>)>>);

    /// 设置定时器监听器
    fn set_timer_listener(&mut self,
                          listener: Option<Sender<(Token, Option<(usize, SocketEvent)>)>>);

    /// 设置定时器句柄，返回上个定时器句柄
    fn set_timer_handle(&mut self, timer: usize) -> Option<usize>;

    /// 取消定时器句柄，返回定时器句柄
    fn unset_timer_handle(&mut self) -> Option<usize>;

    /// 是否需要继续从连接流中接收数据
    fn is_require_recv(&self) -> bool;

    /// 接收流中的数据，返回成功，则表示本次接收了需要的字节数，并返回本次接收的字节数，否则返回接收错误
    fn recv(&mut self) -> Result<usize>;

    /// 发送数据到流，返回成功，则返回本次发送了多少字节数，否则返回发送错误原因
    fn send(&mut self) -> Result<usize>;
}

///
/// Tcp连接
///
pub trait Socket: Sized + 'static {
    /// 线程安全的判断是否已关闭Tcp连接
    fn is_closed(&self) -> bool;

    /// 线程安全的判断是否写后立即刷新连接
    fn is_flush(&self) -> bool;

    /// 线程安全的设置是否写后立即刷新连接
    fn set_flush(&self, flush: bool);

    /// 获取当前Tcp连接的句柄
    fn get_handle(&self) -> SocketHandle<Self>;

    /// 移除当前Tcp连接的句柄
    fn remove_handle(&mut self) -> Option<SocketHandle<Self>>;

    /// 获取连接本地地址
    fn get_local(&self) -> &SocketAddr;

    /// 获取连接远端地址
    fn get_remote(&self) -> &SocketAddr;

    /// 获取连接令牌
    fn get_token(&self) -> Option<&Token>;

    /// 获取连接唯一id
    fn get_uid(&self) -> Option<&usize>;

    //获取连接上下文只读引用
    fn get_context(&self) -> Rc<UnsafeCell<SocketContext>>;

    /// 设置连接的超时定时器，同时只允许设置一个定时器，新的定时器会覆盖未超时的旧定时器
    fn set_timeout(&self, timeout: usize, event: SocketEvent);

    /// 取消连接的未超时超时定时器
    fn unset_timeout(&self);

    /// 是否是安全的连接
    fn is_security(&self) -> bool;

    /// 通知连接读就绪，可以开始接收指定字节数的数据，如果当前需要等待接收则返回AsyncValueNonBlocking, 否则返回接收缓冲区中已有数据的字节数
    /// 设置准备读取的字节大小为0，则表示准备接收任意数量的字节，直到当前连接的流没有可接收的数据
    /// 设置准备读取的字节大小大于0，则表示至少需要接收指定数量的字节，如果还未接收到指定数量的字节，则继续从流中接收
    /// 异步阻塞读取读缓冲前应该保证调用此函数对读缓冲进行填充，避免异步读取被异步阻塞
    /// 返回0长度，表示当前连接已关闭，继续操作将是未定义行为
    /// 注意调用此方法，在保持连接的前提下，必须保证后续一定还可以接收到数据，否则会导致无法唤醒当前异步准备读取器
    fn read_ready(&mut self, adjust: usize) -> GenResult<AsyncValueNonBlocking<usize>, usize>;

    /// 判断当前连接是否有异步准备读取器
    fn is_wait_wakeup_read_ready(&self) -> bool;

    /// 唤醒当前连接异步阻塞的读就绪，如果当前连接没有异步准备读取器则忽略
    fn wakeup_read_ready(&mut self);

    /// 获取连接的输入缓冲区的只读引用
    fn get_read_buffer(&self) -> Rc<UnsafeCell<Option<BytesMut>>>;

    /// 获取连接的输出缓冲区的可写引用
    fn get_write_buffer(&mut self) -> Option<&mut BytesMut>;

    /// 通知连接写就绪，可以开始发送指定的数据
    fn write_ready<B>(&mut self, buf: B) -> Result<()>
        where B: AsRef<[u8]> + 'static;

    /// 重新注册当前流感兴趣的事件
    fn reregister_interest(&mut self, ready: Ready) -> Result<()>;

    /// 判断当前连接是否已休眠
    fn is_hibernated(&self) -> bool;

    /// 当前连接已休眠，则新的任务必须加入等待任务队列中
    fn push_hibernated_task<F>(&self,
                               task: F)
        where F: Future<Output = ()> + 'static;

    /// 开始执行连接休眠时加入的任务，当前任务执行完成后自动执行下一个任务，直到任务队列为空
    fn run_hibernated_tasks(&self);

    /// 获取当前连接的休眠对象，返回空表示连接已关闭
    fn hibernate(&self,
                 handle: SocketHandle<Self>,
                 ready: Ready) -> Option<Hibernate<Self>>;

    /// 设置当前连接的休眠对象，设置成功返回真
    fn set_hibernate(&self, hibernate: Hibernate<Self>) -> bool;

    /// 设置当前连接在休眠时挂起的其它休眠对象的唤醒器
    fn set_hibernate_wakers(&self, waker: Waker);

    /// 非阻塞的唤醒被休眠的当前连接，如果当前连接未被休眠，则忽略
    /// 还会唤醒当前连接正在休眠时，当前连接的所有其它休眠对象的唤醒器
    /// 唤醒过程可能会被阻塞，这不会导致线程阻塞而是返回假，调用者可以继续尝试唤醒，直到返回真
    fn wakeup(&mut self, result: Result<()>) -> bool;

    /// 线程安全的关闭Tcp连接
    fn close(&mut self, reason: Result<()>) -> Result<()>;
}

///
/// 接收器指令
///
#[derive(Clone)]
pub enum AcceptorCmd {
    Continue,       //继续
    Pause(usize),   //暂停，指定的暂停时间，单位ms
    Close(String),  //关闭，需要指定关闭原因
}

///
/// Tcp连接驱动器，用于处理接收和发送的二进制数据
///
pub struct SocketDriver<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    addrs:      Rc<XHashMap<SocketAddr, usize>>,                                //驱动器绑定的地址
    controller: Option<Rc<Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>>>,    //连接接受器的控制器
    count:      Rc<AtomicUsize>,                                                //内部连接计数器
    router:     Rc<Vec<Sender<S>>>,                                             //连接路由表
    adapter:    Option<Arc<A>>,                                                 //连接协议适配器
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for SocketDriver<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for SocketDriver<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Clone for SocketDriver<S, A> {
    fn clone(&self) -> Self {
        SocketDriver {
            addrs: self.addrs.clone(),
            controller: self.controller.clone(),
            count: self.count.clone(),
            router: self.router.clone(),
            adapter: self.adapter.clone(),
        }
    }
}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> SocketDriver<S, A> {
    /// 构建一个Tcp连接驱动器
    pub fn new(bind: &[(SocketAddr, Sender<S>)]) -> Self {
        let size = bind.len();
        let mut map = XHashMap::default();
        let mut vec = Vec::with_capacity(size);

        let mut index: usize = 0;
        for (addr, sender) in bind {
            map.insert(addr.clone(), index);
            vec.push(sender.clone());
            index += 1;
        }
        let count = AtomicUsize::new(0);

        SocketDriver {
            addrs: Rc::new(map),
            controller: None,
            count: Rc::new(count),
            router: Rc::new(vec),
            adapter: None,
        }
    }

    /// 获取Tcp连接驱动器绑定的地址
    pub fn get_addrs(&self) -> Vec<SocketAddr> {
        self.addrs.keys().map(|addr| {
            addr.clone()
        }).collect::<Vec<SocketAddr>>()
    }

    /// 获取连接驱动的控制器
    pub fn get_controller(&self) -> Option<&Rc<Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>>> {
        self.controller.as_ref()
    }

    /// 设置连接驱动的控制器
    pub fn set_controller(&mut self, controller: Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>) {
        self.controller = Some(Rc::new(controller));
    }

    /// 将连接路由到对应的连接池中等待处理
    pub fn route(&self, mut socket: S) -> Result<()> {
        if let Some(_token) = socket.set_token(None) {
            let router = &self.router[self.count.fetch_add(1, Ordering::Relaxed) % self.router.len()];

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

///
/// Tcp连接句柄
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
    /// 构建Tcp连接句柄
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

    /// 线程安全的获取连接令牌
    pub fn get_uid(&self) -> usize {
        self.0.uid
    }

    /// 线程安全的获取连接本地地址
    pub fn get_local(&self) -> &SocketAddr {
        &self.0.local
    }

    /// 线程安全的获取连接远端地址
    pub fn get_remote(&self) -> &SocketAddr {
        &self.0.remote
    }

    /// 线程安全的设置超时定时器
    pub fn set_timeout(&self, timeout: usize, event: SocketEvent) {
        let _ = self
            .0
            .timer_listener
            .send((self.0.token, Some((timeout, event))));
    }

    /// 线程安全的取消超时定时器
    pub fn unset_timeout(&self) {
        let _ = self
            .0
            .timer_listener
            .send((self.0.token, None));
    }

    /// 线程安全的判断是否是安全连接
    pub fn is_security(&self) -> bool {
        self.0.security
    }

    /// 线程安全的获取Tcp连接上下文的只读引用
    pub fn get_context(&self) -> Rc<UnsafeCell<SocketContext>> {
        unsafe {
            (&*self.0.inner.get()).get_context()
        }
    }

    /// 线程安全的准备读
    pub fn read_ready(&self, size: usize) -> GenResult<AsyncValueNonBlocking<usize>, usize> {
        unsafe {
            (&mut *self.0.inner.get()).read_ready(size)
        }
    }

    /// 线程安全的获取连接的接收缓冲区的引用
    pub fn get_read_buffer(&self) -> Rc<UnsafeCell<Option<BytesMut>>> {
        unsafe {
            (&*self.0.inner.get()).get_read_buffer()
        }
    }

    /// 线程安全的写
    pub fn write_ready<B>(&self, buf: B) -> Result<()>
        where B: AsRef<[u8]> + 'static {
        unsafe {
            (&mut *self.0.inner.get()).write_ready(buf)
        }
    }

    /// 重新注册当前流感兴趣的事件
    pub fn reregister_interest(&self, ready: Ready) -> Result<()> {
        unsafe {
            (&mut *self.0.inner.get()).reregister_interest(ready)
        }
    }

    /// 线程安全的开始执行连接休眠时加入的任务，当前任务执行完成后自动执行下一个任务，直到任务队列为空
    pub fn run_hibernated_tasks(&self) {
        unsafe {
            (&*self.0.inner.get()).run_hibernated_tasks();
        }
    }

    /// 线程安全的异步休眠当前连接，直到被唤醒
    pub fn hibernate(&self,
                     handle: SocketHandle<S>,
                     ready: Ready) -> Option<Hibernate<S>> {
        unsafe {
            (&*self.0.inner.get()).hibernate(handle, ready)
        }
    }

    /// 线程安全的设置当前连接的休眠对象
    pub fn set_hibernate(&self, hibernate: Hibernate<S>) -> bool {
        unsafe {
            (&*self.0.inner.get()).set_hibernate(hibernate)
        }
    }

    /// 线程安全的设置当前连接在休眠时挂起的其它休眠对象的唤醒器
    fn set_hibernate_wakers(&self, waker: Waker) {
        unsafe {
            (&*self.0.inner.get()).set_hibernate_wakers(waker);
        }
    }

    /// 线程安全的非阻塞的唤醒被休眠的当前连接，如果当前连接未被休眠，则忽略
    /// 还会唤醒当前连接正在休眠时，当前连接的所有其它休眠对象的唤醒器
    /// 唤醒过程可能会被阻塞，这不会导致线程阻塞而是返回假，调用者可以继续尝试唤醒，直到返回真
    pub fn wakeup(&self, result: Result<()>) -> bool {
        unsafe {
            (&mut *self.0.inner.get()).wakeup(result)
        }
    }

    /// 线程安全的关闭连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        unsafe {
            (&mut *self.0.inner.get()).close(reason)
        }
    }
}

///
/// Tcp连接镜像
///
pub struct SocketImage<S: Socket> {
    inner:          Arc<UnsafeCell<S>>,                             //Tcp连接指针
    local:          SocketAddr,                                     //TCP连接本地地址
    remote:         SocketAddr,                                     //TCP连接远端地址
    token:          Token,                                          //Tcp连接令牌
    uid:            usize,                                          //Tcp连接唯一id
    security:       bool,                                           //Tcp连接是否安全
    closed:         Arc<AtomicBool>,                                //Tcp连接关闭状态
    close_listener: Sender<(Token, Result<()>)>,                    //关闭事件监听器
    timer_listener: Sender<(Token, Option<(usize, SocketEvent)>)>,  //定时事件监听器
}

unsafe impl<S: Socket> Send for SocketImage<S> {}
unsafe impl<S: Socket> Sync for SocketImage<S> {}

impl<S: Socket> SocketImage<S> {
    /// 构建Tcp连接镜像
    pub fn new(shared: &Arc<UnsafeCell<S>>,
               local: SocketAddr,
               remote: SocketAddr,
               token: Token,
               uid: usize,
               security: bool,
               closed: Arc<AtomicBool>,
               close_listener: Sender<(Token, Result<()>)>,
               timer_listener: Sender<(Token, Option<(usize, SocketEvent)>)>
    ) -> Self {
        SocketImage {
            inner: shared.clone(),
            local,
            remote,
            token,
            uid,
            security,
            closed,
            close_listener,
            timer_listener,
        }
    }
}

///
/// Tcp连接异步服务
///
pub trait AsyncService<S: Socket>: Send + Sync + 'static {
    /// 异步处理已连接
    fn handle_connected(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已读
    fn handle_readed(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已写
    fn handle_writed(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已关闭
    fn handle_closed(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已超时
    fn handle_timeouted(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()>;
}