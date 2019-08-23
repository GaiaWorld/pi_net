use std::pin::Pin;
use std::str::FromStr;
use std::cell::RefCell;
use std::future::Future;
use std::rc::{Weak, Rc};
use std::collections::HashMap;
use std::result::Result as GenResult;
use std::task::{Context, Poll, Waker};
use std::io::{Error, Result, ErrorKind};
use std::net::{Shutdown, SocketAddr, IpAddr, Ipv6Addr, Ipv4Addr};

use fnv::FnvBuildHasher;
use crossbeam_channel::Sender;
use mio::{
    PollOpt, Token, Ready,
    net::TcpStream
};

use atom::Atom;
use std::marker::PhantomData;

/*
* 默认的ipv4地址
*/
const DEFAULT_TCP_IP_V4: &str = "0.0.0.0";

/*
* 默认的ipv6地址
*/
const DEFAULT_TCP_IP_V6: &str = "::";

/*
* 默认的Tcp端口
*/
const DEFAULT_TCP_PORT: u16 = 38080;

/*
* 默认缓冲区大小，16KB
*/
const DEFAULT_BUFFER_SIZE: usize = 16384;

/*
* Tcp流
*/
pub trait Stream: Sized + Send + 'static {
    //构建Tcp流
    fn new(local: &SocketAddr, remote: &SocketAddr, token: Option<Token>, stream: TcpStream) -> Self;

    //设置Tcp流上下文集合
    fn set_handle(&mut self, handle: Weak<RefCell<Self>>);

    //获取连接流
    fn get_stream(&self) -> &TcpStream;

    //获取当前流事件准备状态
    fn get_ready(&self) -> &Ready;

    //设置当前流事件准备状态
    fn set_ready(&mut self, ready: Ready);

    //取消当前流事件准备状态
    fn unset_ready(&mut self, ready: Ready);

    //获取当前流事件轮询选项
    fn get_poll_opt(&self) -> &PollOpt;

    //设置当前流事件轮询选项
    fn set_poll_opt(&mut self, opt: PollOpt);

    //取消当前流事件轮询选项
    fn unset_poll_opt(&mut self, opt: PollOpt);

    //设置可读事件唤醒器，返回上个可读事件唤醒器
    fn set_readable_rouser(&mut self, rouser: Option<Sender<Token>>);

    //设置可写事件唤醒器，返回上个可写事件唤醒器
    fn set_writable_rouser(&mut self, rouser: Option<Sender<Token>>);

    //接收流中的数据，返回成功，则表示本次接收了需要的字节数，并返回本次接收的字节数，否则返回接收错误
    fn recv(&mut self) -> Result<usize>;

    //发送数据到流，返回成功，则返回本次发送了多少字节数，否则返回发送错误原因
    fn send(&mut self) -> Result<usize>;
}

/*
* Tcp连接
*/
pub trait Socket: Sized + Send + 'static {
    //是否已关闭Tcp连接
    fn is_closed(&self) -> bool;

    //是否写后立即刷新连接
    fn is_flush(&self) -> bool;

    //获取当前Tcp连接的句柄
    fn get_handle(&self) -> SocketHandle<Self>;

    //设置是否写后立即刷新连接
    fn set_flush(&mut self, flush: bool);

    //获取连接本地地址
    fn get_local(&self) -> &SocketAddr;

    //获取连接远端地址
    fn get_remote(&self) -> &SocketAddr;

    //获取连接令牌
    fn get_token(&self) -> Option<&Token>;

    //设置连接令牌，返回上个连接令牌
    fn set_token(&mut self, token: Option<Token>) -> Option<Token>;

    //设置连接读写缓冲区容量
    fn init_buffer_capacity(&mut self, read_size: usize, write_size: usize);

    //通知连接读就绪，可以开始接收指定字节数的数据，如果为0则表示读取任意字节数，不会从当前读缓冲区中返回任何数据
    fn read_ready(&mut self, size: usize) -> Result<()>;

    //读取指定字节数的数据，如果为0则表示读取任意字节数
    //返回当前缓冲中指定字节数的数据
    //返回None，则表示当前读缓冲里没有指定字节数的数据，等待指定字节数的数据准备好后，异步回调
    fn read(&mut self, size: usize) -> Result<Option<&[u8]>>;

    //通知连接写就绪，可以开始发送数据
    fn write_ready(&mut self) -> Result<()>;

    //写入指定的数据
    fn write(&mut self, bin: &[u8]) -> Result<()>;

    //关闭Tcp连接
    fn close(&self, how: Shutdown) -> Result<()>;
}

/*
* Tcp连接事件适配器
*/
pub trait SocketAdapter: 'static {
    type Connect: Socket;   //保证外部只可以操作Socket，而无法操作Stream

    //已连接
    fn connected(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>);

    //已读
    fn readed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>);

    //已写
    fn writed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>);

    //已关闭
    fn closed(&self, result: GenResult<SocketHandle<Self::Connect>, (SocketHandle<Self::Connect>, Error)>);
}

/*
* 异步任务IO等待
*/
pub trait AsyncIOWait: Clone + Send + 'static {
    //异步等待指定令牌的IO准备完成
    fn io_wait(&self, token: &Token, waker: Waker);
}

/*
* Tcp连接异步服务
*/
pub trait AsyncService<S: Socket, H: AsyncIOWait>: 'static {
    type Out;

    //使用关联类型，以保证可以延迟到实现时实例化类型，同时使用关联约束，以保证同时支持静态分派和动态分派
    type Future: Future<Output = Self::Out> + Send + 'static;

    //异步处理已连接
    fn handle_connected(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future;

    //异步处理已读
    fn handle_readed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future;

    //异步处理已写
    fn handle_writed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future;

    //异步处理已关闭
    fn handle_closed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future;
}

/*
* Tcp连接状态
*/
pub enum SocketStatus {
    Connected(Result<()>),  //已连接
    Readed(Result<()>),     //已读
    Writed(Result<()>),     //已写
    Closed(Result<()>),     //已关闭
}

/*
* Tcp连接句柄
*/
pub struct SocketHandle<S: Socket>(Weak<RefCell<S>>);

unsafe impl<S: Socket> Send for SocketHandle<S> {}
unsafe impl<S: Socket> Sync for SocketHandle<S> {}

impl<S: Socket> Clone for SocketHandle<S> {
    fn clone(&self) -> Self {
        SocketHandle(self.0.clone())
    }
}

impl<S: Socket> SocketHandle<S> {
    //构建Tcp连接引用
    pub fn new(weak: Weak<RefCell<S>>) -> Self {
        SocketHandle(weak)
    }

    //获取Tcp连接句柄
    pub fn as_handle(&self) -> Option<Rc<RefCell<S>>> {
        self.0.upgrade()
    }
}

/*
* 异步读任务
*/
pub struct AsyncReadTask<'a, S: Socket, W: AsyncIOWait> {
    inner:  *mut S,             //Tcp连接指针
    handle: SocketHandle<S>,    //Tcp连接句柄
    waits:  W,                  //异步任务等待队列
    size:   usize,              //本次需要异步读的字节数
    marker: PhantomData<&'a W>,
}

unsafe impl<'a, S: Socket, W: AsyncIOWait> Send for AsyncReadTask<'a, S, W> {}

impl<'a, S: Socket, W: AsyncIOWait> Future for AsyncReadTask<'a, S, W> {
    type Output = Result<&'a [u8]>; //没有读复制

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.handle.as_handle() {
            None => {
                //从Tcp句柄获取Tcp失败
                Poll::Ready(Err(Error::new(ErrorKind::Other, "async read failed, get socket error")))
            },
            Some(s) => {
                let socket = s.borrow_mut(); //通过Tcp连接句柄，获取可写引用，以保证安全的使用可写指针
                match unsafe { (*self.inner).read(self.size) } {
                    Err(e) => {
                        //读数据错误
                        Poll::Ready(Err(e))
                    },
                    Ok(None) => {
                        //读数据未准备好，则等待
                        if let Some(token) = socket.get_token() {
                            self.waits.io_wait(token, cx.waker().clone());
                        }

                        Poll::Pending
                    },
                    Ok(Some(bin)) => {
                        Poll::Ready(Ok(bin))
                    },
                }
            },
        }
    }
}

impl<'a, S: Socket, W: AsyncIOWait> AsyncReadTask<'a, S, W> {
    //异步读指定字节数的数据
    pub fn async_read(handle: SocketHandle<S>, waits: W, size: usize) -> Self {
        let inner = handle.as_handle().unwrap().as_ptr();
        AsyncReadTask {
            inner,
            handle,
            waits,
            size,
            marker: PhantomData,
        }
    }
}

/*
* 异步写任务
*/
pub struct AsyncWriteTask<'a, S: Socket, W: AsyncIOWait> {
    handle: SocketHandle<S>,    //Tcp连接句柄
    waits:  W,                  //异步任务等待队列
    buf:    &'a [u8],           //本次需要异步写的数据
}

unsafe impl<'a, S: Socket, W: AsyncIOWait> Send for AsyncWriteTask<'a, S, W> {}

impl<'a, S: Socket, W: AsyncIOWait> Future for AsyncWriteTask<'a, S, W> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.handle.as_handle() {
            None => {
                //从Tcp句柄获取Tcp失败
                Poll::Ready(Err(Error::new(ErrorKind::Other, "async write failed, get socket error")))
            },
            Some(s) => {
                let mut socket = s.borrow_mut();
                match socket.write(self.buf) {
                    Err(e) => {
                        //写数据错误
                        Poll::Ready(Err(e))
                    },
                    Ok(()) => {
                        //写数据完成
                        Poll::Ready(Ok(()))
                    },
                }
            },
        }
    }
}

impl<'a, S: Socket, W: AsyncIOWait> AsyncWriteTask<'a, S, W> {
    //异步读写指定的数据
    pub fn async_write(handle: SocketHandle<S>, waits: W, buf: &'a [u8]) -> Self {
        AsyncWriteTask {
            handle,
            waits,
            buf,
        }
    }
}

/*
* 接收器指令
*/
#[derive(Clone)]
pub enum AcceptorCmd {
    Continue,       //继续
    Pause(usize),   //暂停，指定的暂停时间，单位ms
    Close(String),  //关闭，需要指定关闭原因
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

/*
* Tcp连接配置
*/
#[derive(Clone)]
pub enum SocketConfig {
    Raw(Vec<u16>, SocketOption),                                            //非安全的tcp连接共享配置，可以同时在ipv4和ipv6上，绑定相同port
    Tls(Vec<u16>, Atom, Atom, Option<Atom>, SocketOption),                  //安全的tcp连接共享配置，可以同时在ipv4和ipv6上，绑定相同的port
    RawIpv4(IpAddr, Vec<u16>, SocketOption),                                //非安全的tcp连接兼容配置，兼容ipv4和ipv4映射的ipv6
    TlsIpv4(IpAddr, Vec<u16>, Atom, Atom, Option<Atom>, SocketOption),      //安全的tcp连接兼容配置，兼容ipv4和ipv4映射的ipv6
    RawIpv6(Ipv6Addr, Vec<u16>, SocketOption),                              //非安全的tcp连接ipv6独占配置，可以与兼容的ipv4配置，绑定相同port
    TlsIpv6(Ipv6Addr, Vec<u16>, Atom, Atom, Option<Atom>, SocketOption),    //安全的tcp连接ipv6独占配置，可以与兼容的ipv4配置，绑定相同port
}

impl Default for SocketConfig {
    fn default() -> Self {
        SocketConfig::new(DEFAULT_TCP_IP_V4, &[DEFAULT_TCP_PORT])
    }
}

impl SocketConfig {
    //构建一个指定ip和端口的Tcp连接兼容配置
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

    //将地址转换为ipv6
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
            SocketConfig::TlsIpv4(ip, ports, cert_file, key_file, cert_path, option) => {
                //安全的兼容配置，则转换为安全的ipv6独占配置
                match ip {
                    IpAddr::V4(addr) => {
                        SocketConfig::TlsIpv6(addr.to_ipv6_mapped(), ports, cert_file, key_file, cert_path, option)
                    },
                    IpAddr::V6(addr) => {
                        SocketConfig::TlsIpv6(addr, ports, cert_file, key_file, cert_path, option)
                    },
                }
            },
            config => {
                //已经是ipv6独占或共享配置，则忽略
                config
            }
        }
    }

    //设置为安全连接，允许提供tls连接或只允许证书认证通过的客户端建立连接
    pub fn security(self, cert_file: &str, key_file: &str, cert_path: Option<&str>) -> Self {
        let client_cert_path: Option<Atom>;
        if let Some(path) = cert_path {
            //设置了客户端证书路径
            client_cert_path = Some(Atom::from(path))
        } else {
            //未设置客户端证书路径
            client_cert_path = None
        }

        match self {
            SocketConfig::Raw(ports, option) => {
                //非安全的共享配置，则转换为安全的共享配置
                SocketConfig::Tls(ports, Atom::from(cert_file), Atom::from(key_file), client_cert_path, option)
            },
            SocketConfig::RawIpv4(ip, ports, option) => {
                //非安全的兼容配置，则转换为安全的兼容配置
                SocketConfig::TlsIpv4(ip, ports, Atom::from(cert_file), Atom::from(key_file), client_cert_path, option)
            },
            SocketConfig::RawIpv6(ip, ports, option) => {
                //非安全的ipv6独占配置，则转换为安全的ipv6独占配置
                SocketConfig::TlsIpv6(ip, ports, Atom::from(cert_file), Atom::from(key_file), client_cert_path, option)
            },
            config => {
                //已经是安全配置，则忽略
                config
            }
        }
    }

    //获取配置的连接通用选项
    pub fn option(&self) -> SocketOption {
        match self {
            SocketConfig::Raw(_ports, option) => option.clone(),
            SocketConfig::Tls(_ports, _cert_file, _key_file, _cert_path, option) => option.clone(),
            SocketConfig::RawIpv4(_ip, _ports, option) => option.clone(),
            SocketConfig::TlsIpv4(_ip, _ports, _cert_file, _key_file, _cert_path, option) => option.clone(),
            SocketConfig::RawIpv6(_ip, _ports, option) => option.clone(),
            SocketConfig::TlsIpv6(_ip, _ports, _cert_file, _key_file, _cert_path, option) => option.clone(),
        }
    }

    //获取配置的地址列表
    pub fn addrs(&self) -> Vec<SocketAddr> {
        let mut addrs = Vec::with_capacity(1);

        match self {
            SocketConfig::Raw(ports, _option) => {
                for port in ports {
                    //同时插入默认的ipv4和ipv6地址
                    addrs.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()));
                    addrs.push(SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()));
                }
            },
            SocketConfig::Tls(ports, _cert_file, _key_file, _cert_path, _option) => {
                for port in ports {
                    //同时插入默认的ipv4和ipv6地址
                    addrs.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()));
                    addrs.push(SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()));
                }
            },
            SocketConfig::RawIpv4(ip, ports, _option) => {
                for port in ports {
                    addrs.push(SocketAddr::new(ip.clone(), port.clone()))
                }
            },
            SocketConfig::TlsIpv4(ip, ports, _cert_file, _key_file, _cert_path, _option) => {
                for port in ports {
                    addrs.push(SocketAddr::new(ip.clone(), port.clone()))
                }
            },
            SocketConfig::RawIpv6(ip, ports, _option) => {
                for port in ports {
                    addrs.push(SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()))
                }
            },
            SocketConfig::TlsIpv6(ip, ports, _cert_file, _key_file, _cert_path, _option) => {
                for port in ports {
                    addrs.push(SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()))
                }
            },
        }

        addrs
    }

    //获取配置列表
    pub fn configs(&self) -> Vec<(SocketAddr, Option<Atom>, Option<Atom>, Option<Atom>, SocketOption)> {
        let mut configs = Vec::with_capacity(1);

        match self {
            SocketConfig::Raw(ports, option) => {
                for port in ports {
                    //同时插入默认的ipv4和ipv6配置
                    configs.push((SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()), None, None, None, option.clone()));
                    configs.push((SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()), None, None, None, option.clone()));
                }
            },
            SocketConfig::Tls(ports, cert_file, key_file, cert_path, option) => {
                for port in ports {
                    //同时插入默认的ipv4和ipv6的安全配置
                    configs.push((SocketAddr::new(IpAddr::V4(Ipv4Addr::from_str(DEFAULT_TCP_IP_V4).unwrap()), port.clone()), Some(cert_file.clone()), Some(key_file.clone()), cert_path.clone(), option.clone()));
                    configs.push((SocketAddr::new(IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).unwrap()), port.clone()), Some(cert_file.clone()), Some(key_file.clone()), cert_path.clone(), option.clone()));
                }
            },
            SocketConfig::RawIpv4(ip, ports, option) => {
                for port in ports {
                    configs.push((SocketAddr::new(ip.clone(), port.clone()), None, None, None, option.clone()))
                }
            },
            SocketConfig::TlsIpv4(ip, ports, cert_file, key_file, cert_path, option) => {
                for port in ports {
                    configs.push((SocketAddr::new(ip.clone(), port.clone()), Some(cert_file.clone()), Some(key_file.clone()), cert_path.clone(), option.clone()))
                }
            },
            SocketConfig::RawIpv6(ip, ports, option) => {
                for port in ports {
                    configs.push((SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()), None, None, None, option.clone()))
                }
            },
            SocketConfig::TlsIpv6(ip, ports, cert_file, key_file, cert_path, option) => {
                for port in ports {
                    configs.push((SocketAddr::new(IpAddr::V6(ip.clone()), port.clone()), Some(cert_file.clone()), Some(key_file.clone()), cert_path.clone(), option.clone()))
                }
            },
        }

        configs
    }
}

/*
* Tcp连接驱动器，用于处理接收和发送的二进制数据
*/
pub struct SocketDriver<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    addrs:      Rc<HashMap<SocketAddr, usize, FnvBuildHasher>>,                //驱动器绑定的地址
    controller: Option<Rc<Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>>>,   //连接接受器的控制器
    router:     Rc<Vec<Sender<S>>>,                                            //连接路由表
    adapter:    Option<Rc<A>>,                                                 //连接协议适配器
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for SocketDriver<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for SocketDriver<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Clone for SocketDriver<S, A> {
    fn clone(&self) -> Self {
        SocketDriver {
            addrs: self.addrs.clone(),
            controller: self.controller.clone(),
            router: self.router.clone(),
            adapter: self.adapter.clone(),
        }
    }
}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> SocketDriver<S, A> {
    //构建一个Tcp连接驱动器
    pub fn new(bind: &[(SocketAddr, Sender<S>)]) -> Self {
        let size = bind.len();
        let mut map = HashMap::with_capacity_and_hasher(size, FnvBuildHasher::default());
        let mut vec = Vec::with_capacity(size);

        let mut index: usize = 0;
        for (addr, sender) in bind {
            map.insert(addr.clone(), index);
            vec.push(sender.clone());
            index += 1;
        }

        SocketDriver {
            addrs: Rc::new(map),
            controller: None,
            router: Rc::new(vec),
            adapter: None,
        }
    }

    //获取Tcp连接驱动器绑定的地址
    pub fn get_addrs(&self) -> Vec<SocketAddr> {
        self.addrs.keys().map(|addr| {
            addr.clone()
        }).collect::<Vec<SocketAddr>>()
    }

    //获取连接驱动的控制器
    pub fn get_controller(&self) -> Option<&Rc<Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>>> {
        self.controller.as_ref()
    }

    //设置连接驱动的控制器
    pub fn set_controller(&mut self, controller: Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>) {
        self.controller = Some(Rc::new(controller));
    }

    //将连接路由到对应的连接池中等待处理
    pub fn route(&self, mut socket: S) -> Result<()> {
        if let Some(Token(id)) = socket.set_token(None) {
            let router = &self.router[id % self.router.len()];

            match router.try_send(socket) {
                Err(e) => {
                    Err(Error::new(ErrorKind::BrokenPipe, format!("tcp socket route failed, e: {:?}", e)))
                },
                Ok(_) => Ok(()),
            }
        } else {
            Err(Error::new(ErrorKind::Interrupted, format!("tcp socket route failed, e: invalid accept token")))
        }
    }

    //获取连接适配器
    pub fn get_adapter(&self) -> &A {
        self.adapter.as_ref().unwrap()
    }

    //设置连接适配器
    pub fn set_adapter(&mut self, adapter: A) {
        self.adapter = Some(Rc::new(adapter));
    }
}
