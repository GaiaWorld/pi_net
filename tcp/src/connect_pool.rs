use std::mem;
use std::thread;
use std::rc::Rc;
use std::cell::RefCell;
use std::time::Duration;
use std::collections::HashMap;
use std::io::{ErrorKind, Result};
use std::net::{Shutdown, SocketAddr};

use slab::Slab;
use fnv::FnvBuildHasher;
use mio::{Events, Poll, Token};
use crossbeam_channel::{Sender, Receiver, unbounded};

use crate::driver::{Socket, Stream, SocketAdapter, SocketHandle, SocketOption, SocketConfig, SocketDriver};

/*
* Tcp连接池
*/
pub struct TcpSocketPool<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    name:                   String,                                     //Tcp连接池名称
    config:                 SocketConfig,                               //Tcp连接配置
    poll:                   Poll,                                       //Socket事件轮询器
    contexts:               Slab<Rc<RefCell<S>>>,                      //Socket上下文表
    map:                    HashMap<SocketAddr, Token, FnvBuildHasher>, //Socket映射表
    driver:                 Option<SocketDriver<S, A>>,                 //Socket驱动
    socket_recv:            Receiver<S>,                                //Socket接收器
    wakeup_readable_sent:   Sender<Token>,                              //唤醒可读事件连接的发送器
    wakeup_readable_recv:   Receiver<Token>,                            //唤醒可读事件连接的接收器
    wakeup_writable_sent:   Sender<Token>,                              //唤醒可写事件连接的发送器
    wakeup_writable_recv:   Receiver<Token>,                            //唤醒可写事件连接的接收器
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for TcpSocketPool<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for TcpSocketPool<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> TcpSocketPool<S, A> {
    //构建一个Tcp连接池
    pub fn new(name: String, receiver: Receiver<S>, config: SocketConfig) -> Result<Self> {
        Self::with_capacity(name, receiver, config, 10)
    }

    //构建一个指定初始大小的Tcp连接池
    pub fn with_capacity(name: String, receiver: Receiver<S>, config: SocketConfig, size: usize) -> Result<Self> {
        let contexts = Slab::with_capacity(size);
        let map = HashMap::with_capacity_and_hasher(size, FnvBuildHasher::default());

        let poll = match Poll::new() {
            Err(e) => {
                return Err(e);
            },
            Ok(p) => {
                p
            }
        };

        let (wakeup_readable_sent, wakeup_readable_recv) = unbounded();
        let (wakeup_writable_sent, wakeup_writable_recv) = unbounded();
        Ok(TcpSocketPool {
            name,
            config,
            poll,
            contexts,
            map,
            driver: None,
            socket_recv: receiver,
            wakeup_readable_sent,
            wakeup_readable_recv,
            wakeup_writable_sent,
            wakeup_writable_recv,
        })
    }

    //运行Tcp连接池，并设置Socket驱动
    pub fn run(self,
               driver: SocketDriver<S, A>,
               stack_size: usize,
               event_size: usize,
               timeout: Option<usize>) -> Result<()> {
        let mut pool = self;
        pool.driver = Some(driver);
        if let Err(e) = thread::Builder::new()
            .name("Tcp Socket Pool ".to_string() + &pool.name)
            .stack_size(stack_size)
            .spawn(move || {
                event_loop(pool, event_size, timeout);
            }) {
            return Err(e);
        }

        Ok(())
    }
}

//Tcp连接事件循环，当连接同时关注可读和可写事件时，轮询将不会返回任何事件
fn event_loop<S: Socket + Stream, A: SocketAdapter<Connect = S>>(mut pool: TcpSocketPool<S, A>, event_size: usize, timeout: Option<usize>) {
    let poll_timeout = if let Some(t) = timeout {
        Some(Duration::from_millis(t as u64))
    } else {
        None
    };

    let pool_name = pool.name.clone();
    let mut events = Events::with_capacity(event_size);
    loop {
        handle_accepted(&mut pool);

        handle_wakeup_readable(&mut pool);

        handle_wakeup_writable(&mut pool);

        if let Err(e) = pool.poll.poll(&mut events, poll_timeout) {
            println!("!!!> Tcp Socket Pool Poll Failed, timeout: {:?}, ports: {:?}, reason: {:?}", poll_timeout, &pool_name, e);
            break;
        }

        handle_poll_events(&mut pool, &events);
    }
}

//处理已接受的Tcp连接
fn handle_accepted<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>) {
    let socket_opts = pool.config.option();
    for mut socket in pool.socket_recv.try_iter().collect::<Vec<S>>() {
        //接受的新的Tcp连接
        let entry = pool.contexts.vacant_entry();
        let id = entry.key();
        let token = Token(id);

        //注册指定连接的轮询事件，暂时不关注读写事件，等待上层通知后，开始关注读写事件
        pool.map.insert(socket.get_remote().clone(), token);
        if let Err(e) = pool.poll.register(socket.get_stream(), token, socket.get_ready().clone(), socket.get_poll_opt().clone()) {
            //连接注册失败
            println!("!!!> Tcp Socket Poll Register Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.get_remote(), socket.get_local(), e);

            //立即关闭未注册的连接
            if let Err(e) = socket.close(Shutdown::Both) {
                println!("!!!> Tcp Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.get_remote(), socket.get_local(), e);
            }
        } else {
            //连接注册成功
            init_socket::<S, A>(&mut socket,
                                pool.wakeup_readable_sent.clone(),
                                pool.wakeup_writable_sent.clone(),
                                &socket_opts);

            socket.set_token(Some(token)); //为注册成功的连接绑定新的令牌
            let socket_rc = Rc::new(RefCell::new(socket));
            let weak = Rc::downgrade(&socket_rc);
            socket_rc.borrow_mut().set_handle(weak.clone()); //设置连接句柄
            pool.driver.as_ref().unwrap().get_adapter().connected(Ok(SocketHandle::new(weak))); //执行连接回调
            entry.insert(socket_rc); //加入连接池上下文
        }
    }
}

//初始化Tcp连接，为连接绑定唤醒器，并设置连接通用选项
fn init_socket<S: Socket + Stream, A: SocketAdapter<Connect = S>>(socket: &mut S,
                                                                  wakeup_readable_sent: Sender<Token>,
                                                                  wakeup_writable_sent: Sender<Token>,
                                                                  socket_opts: &SocketOption) {
    //连接绑定唤醒器
    socket.set_readable_rouser(Some(wakeup_readable_sent));
    socket.set_writable_rouser(Some(wakeup_writable_sent));

    //设置连接是否ipv6独占，独占后可以与ipv4共享相同的端口
    let stream = socket.get_stream();
    if socket.get_local().is_ipv6() {
        //如果本地地址是ipv6，则设置当前流为ipv6独占
        if let Err(e) = stream.set_only_v6(true) {
            panic!("init socket failed, reason: {:?}", e);
        }
    }

    //设置连接通用选项
    if let Err(e) = stream.set_recv_buffer_size(socket_opts.recv_buffer_size) {
        panic!("init socket failed, reason: {:?}", e);
    }
    if let Err(e) = stream.set_send_buffer_size(socket_opts.send_buffer_size) {
        panic!("init socket failed, reason: {:?}", e);
    }
    socket.init_buffer_capacity(socket_opts.read_buffer_capacity, socket_opts.write_buffer_capacity);
}

//批量唤醒Tcp连接的可读事件
fn handle_wakeup_readable<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>) {
    //接收所有唤醒令牌，因为单线程异步唤醒，所以不会出现一次轮询多次唤醒，所以不需要对唤醒令牌去重
    for token in pool.wakeup_readable_recv.try_iter().collect::<Vec<Token>>() {
        if let Some(socket) = pool.contexts.get(token.0) {
            if let Err(e) = pool.poll.reregister(socket.borrow().get_stream(), token, socket.borrow().get_ready().clone(), socket.borrow().get_poll_opt().clone()) {
                panic!("handle wakeup readable failed, reason: {:?}", e);
            }
        }
    }
}

//批量唤醒Tcp连接的可写事件
fn handle_wakeup_writable<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>) {
    //接收所有唤醒令牌，因为多线程异步唤醒，导致在一次轮询中可能多次唤醒，所以需要对唤醒令牌去重
    let mut tokens = pool.wakeup_writable_recv.try_iter().collect::<Vec<Token>>();
    tokens.sort();
    tokens.dedup();

    for token in tokens {
        if let Some(socket) = pool.contexts.get(token.0) {
            if let Err(e) = pool.poll.reregister(socket.borrow().get_stream(), token, socket.borrow().get_ready().clone(), socket.borrow().get_poll_opt().clone()) {
                panic!("handle wakeup writable failed, reason: {:?}", e);
            }
        }
    }
}

//处理Tcp连接的轮询事件
fn handle_poll_events<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>, events: &Events) {
    let mut token;
    let mut ready;
    let mut close_reason = None;

    for event in events {
        token = event.token(); //当前事件的令牌
        ready = event.readiness(); //当前事件的类型

        if let Some(socket) = pool.contexts.get_mut(token.0) {
            let mut s = socket.borrow_mut();
            if ready.is_readable() {
                //可读事件，表示读就绪
                match s.recv() {
                    Ok(0) => {
                        //没有接收任何数据，则只重新注册当前Tcp连接关注的事件
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready().clone(), s.get_poll_opt().clone()) {
                            panic!("handle poll events failed, reason: {:?}", e);
                        }
                    },
                    Ok(_len) => {
                        //按需接收完成，则重新注册当前Tcp连接关注的事件，并执行已读回调
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready().clone(), s.get_poll_opt().clone()) {
                            panic!("handle poll events failed, reason: {:?}", e);
                        }

                        let handle = s.get_handle();
                        mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                        pool.driver.as_ref().unwrap().get_adapter().readed(Ok(handle));
                    },
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        //按需接收将阻塞，则继续关注可读事件，并等待下次事件轮询时尝试完成按需接收
                        continue;
                    },
                    Err(e) => {
                        //按需接收失败，准备关闭当前连接
                        close_reason = Some(Err(e));
                    },
                }
            } else if ready.is_writable() {
                //可写事件，表示写就绪
                match s.send() {
                    Ok(0) => {
                        //没有发送任何数据，则只重新注册当前Tcp连接关注的事件
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready().clone(), s.get_poll_opt().clone()) {
                            panic!("handle poll events failed, reason: {:?}", e);
                        }
                    },
                    Ok(_len) => {
                        //发送完成，则重新注册当前Tcp连接关注的事件，并执行已写回调
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready().clone(), s.get_poll_opt().clone()) {
                            panic!("handle poll events failed, reason: {:?}", e);
                        }
                        
                        let handle = s.get_handle();
                        mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                        pool.driver.as_ref().unwrap().get_adapter().writed(Ok(handle));
                    },
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        //发送将阻塞，则继续关注可写事件，并等待下次事件轮询时尝试完成发送
                        continue;
                    },
                    Err(e) => {
                        //发送失败，准备关闭当前连接
                        close_reason = Some(Err(e));
                    },
                }
            }
        }

        //关闭需要关闭的Tcp连接
        if let Some(reason) = close_reason.take() {
            close_socket(pool, token, reason);
        }
    }
}

//关闭已注册的Tcp连接，并清理Tcp连接池中的上下文
fn close_socket<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>, token: Token, reason: Result<()>) {
    let socket = pool.contexts.remove(token.0); //从Tcp连接池上下文中，移除指定令牌的Tcp连接

    pool.map.remove(socket.borrow().get_remote()); //从Tcp连接池的对端地址映射表中，移除Tcp连接的令牌

    //从轮询器中注销Tcp连接
    if let Err(e) = pool.poll.deregister(socket.borrow().get_stream()) {
        //从轮询器中注销Tcp连接失败
        println!("!!!Tcp Socket Unregister Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.borrow().get_remote(), socket.borrow().get_local(), e);
    }

    if let Err(e) = reason {
        //因为出错关闭Tcp连接，则先关闭Tcp连接，再执行关闭回调
        if let Err(e) = socket.borrow().close(Shutdown::Both) {
            //关闭Tcp连接失败
            println!("!!!> Tcp Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.borrow().get_remote(), socket.borrow().get_local(), e);
        }

        pool.driver.as_ref().unwrap().get_adapter().closed(Err((socket.borrow().get_handle(), e)));
    } else {
        //正常关闭Tcp连接，则先执行关闭回调，再关闭Tcp连接
        pool.driver.as_ref().unwrap().get_adapter().closed(Ok(socket.borrow().get_handle()));

        if let Err(e) = socket.borrow().close(Shutdown::Both) {
            //关闭Tcp连接失败
            println!("!!!> Tcp Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.borrow().get_remote(), socket.borrow().get_local(), e);
        }
    }
}
