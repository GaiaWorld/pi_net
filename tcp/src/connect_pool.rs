use std::mem;
use std::thread;
use std::sync::Arc;
use std::str::FromStr;
use std::cell::RefCell;
use std::time::Duration;
use std::collections::HashMap;
use std::io::{ErrorKind, Result, Error};
use std::net::{Shutdown, SocketAddr, IpAddr, Ipv6Addr};

use slab::Slab;
use fnv::FnvBuildHasher;
use mio::{Events, Poll, Token, Ready};
use crossbeam_channel::{Sender, Receiver, unbounded};
use log::warn;

use local_timer::LocalTimer;

use crate::{driver::{DEFAULT_TCP_IP_V6, Socket, Stream, SocketAdapter, SocketOption, SocketConfig, SocketDriver, SocketWakeup},
            buffer_pool::WriteBufferPool,
            util::{register_close_sender, SocketEvent}};

/*
* Tcp连接池
*/
pub struct TcpSocketPool<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    uid:            u8,                                                 //Tcp连接池唯一id
    name:           String,                                             //Tcp连接池名称
    config:         SocketConfig,                                       //Tcp连接配置
    poll:           Poll,                                               //Socket事件轮询器
    sockets:        Slab<Arc<RefCell<S>>>,                              //Socket连接表
    map:            HashMap<SocketAddr, Token, FnvBuildHasher>,         //Socket映射表
    driver:         Option<SocketDriver<S, A>>,                         //Socket驱动
    socket_recv:    Receiver<S>,                                        //Socket接收器
    wakeup_sent:    Sender<(Token, SocketWakeup)>,                      //唤醒事件的发送器
    wakeup_recv:    Receiver<(Token, SocketWakeup)>,                    //唤醒事件的接收器
    close_sent:     Sender<(Token, Result<()>)>,                        //关闭事件的发送器
    close_recv:     Receiver<(Token, Result<()>)>,                      //关闭事件的接收器
    wait_close:     Vec<(Token, Result<()>)>,                           //等待关闭队列
    timer:          LocalTimer<(Token, SocketEvent)>,                   //定时器
    timer_sent:     Sender<(Token, Option<(usize, SocketEvent)>)>,      //定时器设置事件的发送器
    timer_recv:     Receiver<(Token, Option<(usize, SocketEvent)>)>,    //定时器设置事件的接收器
    buffer:         WriteBufferPool,                                    //写缓冲池
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for TcpSocketPool<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for TcpSocketPool<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> TcpSocketPool<S, A> {
    //构建一个Tcp连接池
    pub fn new(uid: u8,
               name: String,
               receiver: Receiver<S>,
               config: SocketConfig,
               buffer: WriteBufferPool) -> Result<Self> {
        Self::with_capacity(uid, name, receiver, config, buffer, 10)
    }

    //构建一个指定初始大小的Tcp连接池
    pub fn with_capacity(uid: u8,
                         name: String,
                         receiver: Receiver<S>,
                         config: SocketConfig,
                         buffer: WriteBufferPool,
                         size: usize) -> Result<Self> {
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

        let (wakeup_sent, wakeup_recv) = unbounded();
        let (close_sent, close_recv) = unbounded();
        let (timer_sent, timer_recv) = unbounded();
        register_close_sender(uid, close_sent.clone()); //注册全局关闭事件发送器

        Ok(TcpSocketPool {
            uid,
            name,
            config,
            poll,
            sockets: contexts,
            map,
            driver: None,
            socket_recv: receiver,
            wakeup_sent,
            wakeup_recv,
            close_sent,
            close_recv,
            wait_close: Vec::new(),
            timer: LocalTimer::new(),
            timer_sent,
            timer_recv,
            buffer,
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
            .name("Tcp Socket Pool #".to_string() + &pool.uid.to_string() + " " + &pool.name)
            .stack_size(stack_size)
            .spawn(move || {
                event_loop(pool, event_size, timeout);
            }) {
            return Err(e);
        }

        Ok(())
    }
}

//Tcp连接事件循环，当连接同时关注可读和可写事件时，轮询将不会返回任何事件，轮询超时时长会影响到最快响应时间，最快响应时间是实际处理时间加上轮询超时时长
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

        handle_wakeup(&mut pool);

        if let Err(e) = pool.poll.poll(&mut events, poll_timeout) {
            warn!("!!!> Tcp Socket Pool Poll Failed, timeout: {:?}, ports: {:?}, reason: {:?}", poll_timeout, &pool_name, e);
            break;
        }

        handle_poll_events(&mut pool, &events);

        handle_close_event(&mut pool); //必须在事件处理完成后执行

        handle_timer(&mut pool); //必须在关闭处理完成后执行

        pool.buffer.collect();
    }
}

//处理已接受的Tcp连接
fn handle_accepted<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>) {
    let socket_opts = pool.config.option();
    for mut socket in pool.socket_recv.try_iter().collect::<Vec<S>>() {
        //接受的新的Tcp连接
        let entry = pool.sockets.vacant_entry();
        let id = entry.key();
        let token = Token(id);

        //注册指定连接的轮询事件，暂时不关注读写事件，等待上层通知后，开始关注读写事件
        pool.map.insert(socket.get_remote().clone(), token);
        if let Err(e) = pool.poll.register(socket.get_stream(), token, socket.get_ready(), socket.get_poll_opt().clone()) {
            //连接注册失败
            warn!("!!!> Tcp Socket Poll Register Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.get_remote(), socket.get_local(), e);

            //立即关闭未注册的连接
            if let Err(e) = socket.close(Err(Error::new(ErrorKind::Other, "register socket failed"))) {
                warn!("!!!> Tcp Socket Close Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.get_remote(), socket.get_local(), e);
            }
        } else {
            //连接注册成功
            init_socket::<S, A>(&mut socket,
                                pool.wakeup_sent.clone(),
                                pool.close_sent.clone(),
                                pool.timer_sent.clone(),
                                &socket_opts);

            socket.set_token(Some(token)); //为注册成功的连接绑定新的令牌
            socket.set_uid(create_socket_uid(pool.uid, token)); //为注册成功的连接设置唯一id
            socket.set_write_buffer(pool.buffer.clone()); //为注册成功的连接绑定写缓冲池
            let socket_arc = Arc::new(RefCell::new(socket));
            socket_arc.borrow_mut().set_handle(&socket_arc); //设置连接句柄
            let handle = socket_arc.borrow().get_handle();
            pool.driver.as_ref().unwrap().get_adapter().connected(Ok(handle)); //执行连接回调
            entry.insert(socket_arc); //加入连接池上下文
        }
    }
}

//创建连接唯一id，由8位连接池唯一id和24位的Token组成
fn create_socket_uid(pool_uid: u8, Token(id): Token) -> usize {
    (((pool_uid as usize) << 24) & 0xffffffff) | (id & 0xffffff)
}

//初始化Tcp连接，为连接绑定唤醒器，并设置连接通用选项
fn init_socket<S: Socket + Stream, A: SocketAdapter<Connect = S>>(socket: &mut S,
                                                                  wakeup_sent: Sender<(Token, SocketWakeup)>,
                                                                  close_listener: Sender<(Token, Result<()>)>,
                                                                  timer_listener: Sender<(Token, Option<(usize, SocketEvent)>)>,
                                                                  socket_opts: &SocketOption) {
    //连接绑定唤醒器和监听器
    socket.set_rouser(Some(wakeup_sent));
    socket.set_close_listener(Some(close_listener));
    socket.set_timer_listener(Some(timer_listener));

    //设置连接是否ipv6独占，独占后可以与ipv4共享相同的端口
    let stream = socket.get_stream();
    if (socket.get_local().ip().ne(&IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).ok().unwrap()))) && socket.get_local().is_ipv6() {
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

//批量唤醒Tcp连接
fn handle_wakeup<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>) {
    //接收所有唤醒令牌，因为单线程异步唤醒，所以不会出现一次轮询多次唤醒，所以不需要对唤醒令牌去重
    for item in pool.wakeup_recv.try_iter().collect::<Vec<(Token, SocketWakeup)>>() {
        match item {
            (token, SocketWakeup::Read(false)) => {
                //唤醒并执行已读回调
                if let Some(socket) = pool.sockets.get(token.0) {
                    let handle = socket.borrow_mut().get_handle();
                    mem::drop(socket); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接

                    pool.driver.as_ref().unwrap().get_adapter().readed(Ok(handle));
                }
            },
            (token, SocketWakeup::Read(true)) => {
                //唤醒并注册可读事件
                if let Some(socket) = pool.sockets.get(token.0) {
                    socket.borrow().set_ready(Ready::readable());
                    if let Err(e) = pool.poll.reregister(socket.borrow().get_stream(), token, socket.borrow().get_ready(), socket.borrow().get_poll_opt().clone()) {
                        //注册可读事件失败，则通知
                        let handle = socket.borrow_mut().get_handle();
                        mem::drop(socket); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接

                        pool.driver.as_ref().unwrap().get_adapter().readed(Err((handle, e)));
                    }
                }
            },
            (token, SocketWakeup::Write(buf)) => {
                //唤醒并注册可写事件
                if let Some(socket) = pool.sockets.get(token.0) {
                    socket.borrow().set_ready(Ready::writable());
                    //唤醒指定令牌的Tcp连接
                    if let Err(e) = pool.poll.reregister(socket.borrow().get_stream(), token, socket.borrow().get_ready(), socket.borrow().get_poll_opt().clone()) {
                        //注册可写事件失败，则通知，并立即释放连接的引用
                        let handle = socket.borrow_mut().get_handle();
                        mem::drop(socket); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接

                        return pool.driver.as_ref().unwrap().get_adapter().writed(Err((handle, e)));
                    }

                    //注册可写事件成功，则为指定令牌的Tcp连接写入数据
                    socket.borrow_mut().write(buf);
                }
            },
            (token, SocketWakeup::Wake) => {
                //唤醒并执行已唤醒回调
                if let Some(socket) = pool.sockets.get(token.0) {
                    let handle = socket.borrow_mut().get_handle();
                    mem::drop(socket); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接

                    pool.driver.as_ref().unwrap().get_adapter().waked(handle);
                }
            },
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

        if let Some(socket) = pool.sockets.get_mut(token.0) {
            let mut s = socket.borrow_mut();
            if ready.is_readable() {
                //可读事件，表示读就绪
                match s.recv() {
                    Ok(0) => {
                        //没有接收任何数据，则只重新注册当前Tcp连接关注的事件
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready(), s.get_poll_opt().clone()) {
                            //重新注册关注的事件失败
                            close_reason = Some(Err(e));
                        }
                    },
                    Ok(_len) => {
                        //按需接收完成，则重新注册当前Tcp连接关注的事件，并执行已读回调
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready(), s.get_poll_opt().clone()) {
                            //重新注册关注的事件失败
                            close_reason = Some(Err(e));
                        } else {
                            //注册关注的事件成功
                            let handle = s.get_handle();
                            mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接

                            pool.driver.as_ref().unwrap().get_adapter().readed(Ok(handle.clone()));
                        }
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
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready(), s.get_poll_opt().clone()) {
                            //重新注册关注的事件失败
                            close_reason = Some(Err(e));
                        }
                    },
                    Ok(_len) => {
                        //发送完成，则重新注册当前Tcp连接关注的事件，并执行已写回调
                        if let Err(e) = pool.poll.reregister(s.get_stream(), token, s.get_ready(), s.get_poll_opt().clone()) {
                            //重新注册关注的事件失败
                            close_reason = Some(Err(e));
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

        //关闭轮询时出错的Tcp连接
        if let Some(reason) = close_reason.take() {
            if let Some(socket) = pool.sockets.get(token.0) {
                socket.borrow().close(reason);
            }
        }
    }
}

//批量处理Tcp连接的关闭事件
fn handle_close_event<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>) {
    let len = pool.wait_close.len();
    if len > 0 {
        //有正在等待关闭的连接
        let i = len - 1;
        for n in 0..len {
            //关闭正在等待的连接
            let (token, reason) = pool.wait_close.remove(i - n);
            close_socket(pool, token, Shutdown::Both, reason);
        }
    }

    for wait in pool.close_recv.try_iter().collect::<Vec<(Token, Result<()>)>>() {
        //将需要关闭的连接，放入等待关闭队列
        pool.wait_close.push(wait);
    }
}

//关闭指定的Tcp连接，并清理相关上下文
fn close_socket<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>,
                                                                   token: Token,
                                                                   how: Shutdown,
                                                                   reason: Result<()>) {
    //从连接表中移除被关闭的Tcp连接
    if !pool.sockets.contains(token.0) {
        //如果指定令牌的Tcp连接不存在，则忽略
        return;
    }
    let socket = pool.sockets.remove(token.0);

    //从映射表中移除被关闭Tcp连接的信息
    pool.map.remove(socket.borrow().get_remote());

    //从轮询器中注销Tcp连接
    let r = pool.poll.deregister(socket.borrow().get_stream());

    //关闭流
    socket.borrow().get_stream().shutdown(how);

    //移除定时器
    if let Some(timer) = socket.borrow_mut().unset_timer_handle() {
        pool.timer.cancel(timer);
    }

    //执行已关闭回调
    let handle = socket.borrow().get_handle();
    if let Err(e) = reason {
        //因为内部错误，关闭Tcp连接
        pool.driver.as_ref().unwrap().get_adapter().closed(Err((handle, e)));
    } else {
        if let Err(e) = r {
            //注销时错误，关闭Tcp连接
            pool.driver.as_ref().unwrap().get_adapter().closed(Err((handle, e)));
        } else {
            //正常关闭Tcp连接
            pool.driver.as_ref().unwrap().get_adapter().closed(Ok(handle));
        }
    }
}

//处理Tcp连接的定时器
fn handle_timer<S: Socket + Stream, A: SocketAdapter<Connect = S>>(pool: &mut TcpSocketPool<S, A>) {
    //设置或取消指定Tcp连接的定时器
    for (token, opt) in pool.timer_recv.try_iter().collect::<Vec<(Token, Option<(usize, SocketEvent)>)>>() {
        if let Some((timeout, event)) = opt {
            //为指定令牌的连接设置指定的定时器
            if let Some(socket) = pool.sockets.get_mut(token.0) {
                if socket.borrow().is_closed() {
                    //连接已关闭，则忽略
                    continue;
                }

                if let Some(timer) = socket.borrow_mut().unset_timer_handle() {
                    //连接已设置定时器，则先移除指定句柄的定时器
                    pool.timer.cancel(timer);
                }

                //设置指定事件的定时器，并在连接上设置定时器句柄
                let timer = pool.timer.set_timeout((token, event), timeout);
                socket.borrow_mut().set_timer_handle(timer);
            }
        } else {
            //为指定令牌的连接取消指定的定时器
            if let Some(socket) = pool.sockets.get_mut(token.0) {
                if socket.borrow().is_closed() {
                    //连接已关闭，则忽略
                    continue;
                }

                //移除连接上的定时器句柄，并移除指定句柄的定时器
                if let Some(timer) = socket.borrow_mut().unset_timer_handle() {
                    pool.timer.cancel(timer);
                }
            }
        }
    }
    
    //轮询所有超时的定时器，执行已超时回调
    let items = pool.timer.poll();
    for (token, event) in items {
        if let Some(socket) = pool.sockets.get_mut(token.0) {
            if socket.borrow().is_closed() {
                //连接已关闭，则忽略
                return;
            }

            //移除连接上的定时器句柄
            socket.borrow_mut().unset_timer_handle();

            //连接已超时
            let handle = socket.borrow().get_handle();
            pool.driver.as_ref().unwrap().get_adapter().timeouted(handle, event);
        }
    }
}
