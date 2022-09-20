use std::mem;
use std::thread;
use std::sync::Arc;
use std::str::FromStr;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::io::{ErrorKind, Result, Error};
use std::net::{Shutdown, SocketAddr, IpAddr, Ipv6Addr};

use futures::future::{FutureExt, BoxFuture};
use mio::{Events, Poll, Token, Interest};
use crossbeam_channel::{Sender, Receiver, unbounded};
use log::{warn, error};

use pi_async::{lock::spin_lock::SpinLock,
               rt::{AsyncRuntime, AsyncValue,
                    worker_thread::WorkerRuntime}};
use pi_hash::XHashMap;
use pi_local_timer::local_timer::LocalTimer;
use pi_slotmap::{Key, DefaultKey, KeyData, SlotMap};

use crate::{DEFAULT_TCP_IP_V6, Socket, Stream, SocketAdapter, SocketOption, SocketConfig, SocketEvent, SocketDriver,
            utils::{SharedStream, register_close_sender}};

///
/// Tcp连接池
///
pub struct TcpSocketPool<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    uid:            u8,                                                             //Tcp连接池唯一id
    name:           String,                                                         //Tcp连接池名称
    config:         SocketConfig,                                                   //Tcp连接配置
    poll:           Arc<SpinLock<Poll>>,                                            //Socket事件轮询器
    sockets:        Arc<SpinLock<SlotMap<DefaultKey, Option<SharedStream<S>>>>>,    //Socket连接表
    map:            XHashMap<SocketAddr, Token>,                                    //Socket映射表
    driver:         Option<SocketDriver<S, A>>,                                     //Socket驱动
    socket_recv:    Receiver<S>,                                                    //Socket接收器
    close_sent:     Sender<(Token, Result<()>)>,                                    //关闭事件的发送器
    close_recv:     Receiver<(Token, Result<()>)>,                                  //关闭事件的接收器
    duration:       Instant,                                                        //定时器持续时间
    timer:          LocalTimer<(Token, SocketEvent), 100, 60, 60, 24>,              //定时器
    timer_sent:     Sender<(Token, Option<(usize, SocketEvent)>)>,                  //定时器设置事件的发送器
    timer_recv:     Receiver<(Token, Option<(usize, SocketEvent)>)>,                //定时器设置事件的接收器
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for TcpSocketPool<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for TcpSocketPool<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> TcpSocketPool<S, A> {
    /// 构建一个Tcp连接池
    pub fn new(uid: u8,
               name: String,
               receiver: Receiver<S>,
               config: SocketConfig,
    ) -> Result<Self> {
        Self::with_capacity(uid, name, receiver, config, 10)
    }

    /// 构建一个指定初始大小的Tcp连接池
    pub fn with_capacity(uid: u8,
                         name: String,
                         receiver: Receiver<S>,
                         config: SocketConfig,
                         size: usize) -> Result<Self> {
        let contexts = Arc::new(SpinLock::new(SlotMap::with_capacity(size)));
        let map = XHashMap::default();

        let poll = Arc::new(SpinLock::new(Poll::new()?));

        let (close_sent, close_recv) = unbounded();
        let (timer_sent, timer_recv) = unbounded();
        register_close_sender(uid, close_sent.clone()); //注册全局关闭事件发送器
        let duration = Instant::now();

        Ok(TcpSocketPool {
            uid,
            name,
            config,
            poll,
            sockets: contexts,
            map,
            driver: None,
            socket_recv: receiver,
            close_sent,
            close_recv,
            duration,
            timer: LocalTimer::<(Token, SocketEvent), 100, 60, 60, 24>::new(10, duration.elapsed().as_millis() as u64),
            timer_sent,
            timer_recv,
        })
    }

    /// 运行Tcp连接池，并设置Socket驱动
    pub fn run(self,
               rt: WorkerRuntime<()>,
               driver: SocketDriver<S, A>,
               event_size: usize,
               timeout: Option<usize>) -> Result<()> {
        let mut pool = self;
        pool.driver = Some(driver);
        let rt_copy = rt.clone();
        rt.spawn(rt.alloc(), async move {
            let poll_timeout = if let Some(t) = timeout {
                Some(Duration::from_millis(t as u64))
            } else {
                None
            };

            //启动Tcp连接事件循环
            event_loop(rt_copy,
                       pool,
                       event_size,
                       poll_timeout).await;
        });

        Ok(())
    }
}

// Tcp连接事件循环，当连接同时关注可读和可写事件时，轮询将不会返回任何事件，轮询超时时长会影响到最快响应时间，最快响应时间是实际处理时间加上轮询超时时长
#[inline]
fn event_loop<S, A>(rt: WorkerRuntime<()>,
                    mut pool: TcpSocketPool<S, A>,
                    event_size: usize,
                    poll_timeout: Option<Duration>) -> BoxFuture<'static, ()>
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    async move {
        let mut events = Events::with_capacity(event_size);
        handle_accepted(&rt, &mut pool).await;

        if let Err(e) = pool
            .poll
            .lock()
            .poll(&mut events, poll_timeout.clone()) {
            //轮询连接事件错误，则立即退出Tcp连接事件循环
            error!("Tcp socket pool poll failed, timeout: {:?}, ports: {:?}, reason: {:?}", poll_timeout, pool.name, e);
            return;
        }

        handle_poll_events(&rt, &mut pool, &events).await;

        handle_close_event(&rt, &mut pool).await;

        handle_timer(&mut pool).await; //必须在关闭处理完成后执行

        //继续异步调用Tcp连接事件循环
        let event_loop = event_loop(rt.clone(),
                                    pool,
                                    event_size,
                                    poll_timeout);
        rt.spawn(rt.alloc(), event_loop);
    }.boxed()
}

// 处理已接受的Tcp连接
#[inline]
async fn handle_accepted<S, A>(rt: &WorkerRuntime<()>,
                               pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    let socket_opts = pool.config.option();
    for mut socket in pool.socket_recv.try_iter().collect::<Vec<S>>() {
        //接受的新的Tcp连接
        let id = pool
            .sockets
            .lock()
            .insert(None);
        let token = Token(id.data().as_ffi() as usize);

        //注册指定连接的轮询事件，暂时不关注读写事件，等待上层通知后，开始关注读写事件
        let ready = socket
            .get_interest()
            .expect(format!("Handle accepted falied, token: {:?}, reason: invalid inited interest",
                    token).as_str());
        pool.map.insert(socket.get_remote().clone(), token);
        if let Err(e) = pool
            .poll
            .lock()
            .registry()
            .register(socket.get_stream_mut(), token, ready) {
            //连接注册失败
            warn!("Tcp socket poll register error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.get_remote(), socket.get_local(), e);

            //立即关闭未注册的连接
            if let Err(e) = socket.close(Err(Error::new(ErrorKind::Other, "register socket failed"))) {
                warn!("Tcp socket close error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}", token, socket.get_remote(), socket.get_local(), e);
            }
        } else {
            //连接注册成功
            init_socket::<S, A>(&mut socket,
                                pool.close_sent.clone(),
                                pool.timer_sent.clone(),
                                &socket_opts);

            socket.set_runtime(rt.clone()); //为注册成功的连接绑定运行时
            socket.set_token(Some(token)); //为注册成功的连接绑定新的令牌
            socket.set_uid(token.0); //为注册成功的连接设置唯一id
            socket.set_poll(pool.poll.clone()); //为注册成功的连接设置轮询器
            let socket_arc = SharedStream::new(socket);
            let handle = {
                socket_arc.borrow_mut().set_handle(socket_arc.inner_ref()); //设置连接句柄
                socket_arc.borrow_mut().get_handle()
            };
            pool.sockets.lock()[id] = Some(socket_arc); //加入连接池上下文

            //异步调用连接回调任务
            let connected = pool
                .driver
                .as_ref()
                .unwrap()
                .get_adapter()
                .connected(Ok(handle));
            rt.spawn(rt.alloc(), connected);
        }
    }
}

// 初始化Tcp连接，为连接绑定唤醒器，并设置连接通用选项
fn init_socket<S, A>(socket: &mut S,
                     close_listener: Sender<(Token, Result<()>)>,
                     timer_listener: Sender<(Token, Option<(usize, SocketEvent)>)>,
                     socket_opts: &SocketOption)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    //连接绑定唤醒器和监听器
    // socket.set_rouser(Some(wakeup_sent));
    socket.set_close_listener(Some(close_listener));
    socket.set_timer_listener(Some(timer_listener));

    //设置连接是否ipv6独占，独占后可以与ipv4共享相同的端口
    let stream = socket.get_stream_ref();
    if (socket.get_local().ip().ne(&IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).ok().unwrap()))) && socket.get_local().is_ipv6() {
        //如果本地地址是ipv6，则设置当前流为ipv6独占
        // if let Err(e) = stream.set_only_v6(true) {
        //     panic!("init socket failed, reason: {:?}", e);
        // }
    }

    //设置连接通用选项
    // if let Err(e) = stream.set_recv_buffer_size(socket_opts.recv_buffer_size) {
    //     panic!("init socket failed, reason: {:?}", e);
    // }
    // if let Err(e) = stream.set_send_buffer_size(socket_opts.send_buffer_size) {
    //     panic!("init socket failed, reason: {:?}", e);
    // }
    socket.init_buffer_capacity(socket_opts.read_buffer_capacity, socket_opts.write_buffer_capacity);
}

// 处理Tcp连接的轮询事件
#[inline]
async fn handle_poll_events<S, A>(rt: &WorkerRuntime<()>,
                                  pool: &mut TcpSocketPool<S, A>,
                                  events: &Events)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    for event in events {
        let token = event.token(); //当前事件的令牌

        if let Some(Some(socket)) = pool
            .sockets
            .lock()
            .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64)))
        {
            let poll = pool.poll.clone();
            let socket_copy = socket.clone();
            let adapter = pool
                .driver
                .as_ref()
                .unwrap()
                .clone_adapter();
            let sockets = pool.sockets.clone();

            if event.is_readable() {
                //可读事件，表示读就绪，则异步处理读事件
                rt.spawn(rt.alloc(), async move {
                    let mut close_reason = None;
                    {
                        let mut s = socket_copy.borrow_mut();
                        match s.recv().await {
                            Ok(len) => {
                                println!("!!!!!!readable, token: {:?}, len: {:?}", s.get_token().unwrap(), len);
                                //按需接收完成，则重新注册当前Tcp连接关注的事件，并执行已读回调
                                if let Some(interest) = s.get_interest() {
                                    //需要修改当前连接感兴趣的事件类型
                                    println!("!!!!!!reset interest, token: {:?}, interest: {:?}", s.get_token().unwrap(), interest);
                                    if let Err(e) = poll
                                        .lock()
                                        .registry()
                                        .reregister(s.get_stream_mut(),
                                                    token,
                                                    interest) {
                                        //重新注册关注的事件失败
                                        close_reason = Some(Err(e));
                                    }
                                }

                                if s.is_wait_wakeup_read_ready() {
                                    println!("!!!!!!wakeup read ready");
                                    //当前连接有需要唤醒的异步准备读取器，则唤醒当前的异步准备读取器
                                    s.wakeup_read_ready();
                                    mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                                } else {
                                    if s.is_hibernated() {
                                        println!("######push hibernated_read_task");
                                        //当前连接已休眠，则将本次读任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                                        let handle = s.get_handle();
                                        s.push_hibernated_task(adapter.readed(Ok(handle)));
                                        mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                                    } else {
                                        println!("!!!!!!call readed");
                                        //当前连接没有需要唤醒的异步准备读取器，则调用接收回调
                                        let handle = s.get_handle();
                                        mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                                        adapter.readed(Ok(handle)).await;
                                    }
                                }
                            },
                            Err(e) => {
                                //按需接收失败，准备关闭当前连接
                                close_reason = Some(Err(e));
                            },
                        }
                    }

                    //关闭轮询时出错的Tcp连接
                    if let Some(reason) = close_reason.take() {
                        if let Some(Some(socket)) = sockets
                            .lock()
                            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                            let result = socket.borrow_mut().close(reason); //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                warn!("Tcp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    socket.borrow().get_token(),
                                    socket.borrow().get_remote(),
                                    socket.borrow().get_local(),
                                    e);
                            }
                        }
                    }
                });
            } else if event.is_writable() {
                //可写事件，表示写就绪，则异步处理写事件
                rt.spawn(rt.alloc(), async move {
                    let mut close_reason = None;
                    {
                        let mut s = socket_copy.borrow_mut();
                        match s.send() {
                            Ok(len) => {
                                println!("!!!!!!writable, token: {:?}, len: {:?}", s.get_token().unwrap(), len);
                                //发送完成，并执行已写回调
                                if let Some(interest) = s.get_interest() {
                                    println!("!!!!!!reset interest, token: {:?}, interest: {:?}", s.get_token().unwrap(), interest);
                                    // 需要修改当前连接感兴趣的事件类型
                                    if let Err(e) = poll
                                        .lock()
                                        .registry()
                                        .reregister(s.get_stream_mut(), token, interest) {
                                        //重新注册关注的事件失败
                                        close_reason = Some(Err(e));
                                    }
                                }

                                if s.is_hibernated() {
                                    println!("######push hibernated_write_task");
                                    //当前连接已休眠，则将本次写任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                                    let handle = s.get_handle();
                                    s.push_hibernated_task(adapter.writed(Ok(handle)));
                                    mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                                } else {
                                    println!("!!!!!!call writed");
                                    //调用发送回调
                                    let handle = s.get_handle();
                                    mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                                    let writed = adapter.writed(Ok(handle)).await;
                                }
                            },
                            Err(e) => {
                                //发送失败，准备关闭当前连接
                                close_reason = Some(Err(e));
                            },
                        }
                    }

                    //关闭轮询时出错的Tcp连接
                    if let Some(reason) = close_reason.take() {
                        if let Some(Some(socket)) = sockets
                            .lock()
                            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                            let result = socket.borrow_mut().close(reason); //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                warn!("Tcp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    socket.borrow().get_token(),
                                    socket.borrow().get_remote(),
                                    socket.borrow().get_local(),
                                    e);
                            }
                        }
                    }
                });
            }
        }
    }
}

// 批量处理Tcp连接的关闭事件
// 关闭指定的Tcp连接，并清理上下文
async fn handle_close_event<S, A>(rt: &WorkerRuntime<()>,
                                  pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    for (token, reason) in pool
        .close_recv
        .try_iter()
        .collect::<Vec<(Token, Result<()>)>>() {
        //从连接表中移除被关闭的Tcp连接
        if !pool
            .sockets
            .lock()
            .contains_key(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
            //如果指定令牌的Tcp连接不存在，则忽略
            return;
        }
        let socket = pool
            .sockets
            .lock()
            .remove(DefaultKey::from(KeyData::from_ffi(token.0 as u64)))
            .unwrap()
            .unwrap();

        //从映射表中移除被关闭Tcp连接的信息
        pool.map.remove(socket.borrow().get_remote());

        //从轮询器中注销Tcp连接
        let r = pool
            .poll
            .lock()
            .registry()
            .deregister(socket.borrow_mut().get_stream_mut());

        //关闭流
        socket
            .borrow()
            .get_stream_ref()
            .shutdown(Shutdown::Both);

        //移除定时器
        if let Some(timer) = socket.borrow_mut().unset_timer_handle() {
            let _ = pool.timer.try_remove(timer);
        }

        //异步执行已关闭回调
        let handle = socket.borrow().get_handle();
        let closed = if let Err(e) = reason {
            //因为内部错误，关闭Tcp连接
            pool.driver
                .as_ref()
                .unwrap()
                .get_adapter()
                .closed(Err((handle, e)))
        } else {
            if let Err(e) = r {
                //注销时错误，关闭Tcp连接
                pool.driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .closed(Err((handle, e)))
            } else {
                //正常关闭Tcp连接
                pool.driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .closed(Ok(handle))
            }
        };
        rt.spawn(rt.alloc(), closed);
    }
}

// 处理Tcp连接的定时器
#[inline]
async fn handle_timer<S, A>(pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    //设置或取消指定Tcp连接的定时器
    for (token, opt) in pool.timer_recv.try_iter().collect::<Vec<(Token, Option<(usize, SocketEvent)>)>>() {
        if let Some((timeout, event)) = opt {
            //为指定令牌的连接设置指定的定时器
            if let Some(Some(socket)) = pool
                .sockets
                .lock()
                .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                if socket.borrow().is_closed() {
                    //连接已关闭，则忽略
                    continue;
                }

                if let Some(timer) = socket.borrow_mut().unset_timer_handle() {
                    //连接已设置定时器，则先移除指定句柄的定时器
                    let _ = pool.timer.try_remove(timer);
                }

                //设置指定事件的定时器，并在连接上设置定时器句柄
                let timer = pool.timer.insert((token, event), timeout as u64);
                socket.borrow_mut().set_timer_handle(timer);
            }
        } else {
            //为指定令牌的连接取消指定的定时器
            if let Some(Some(socket)) = pool
                .sockets
                .lock()
                .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                if socket.borrow().is_closed() {
                    //连接已关闭，则忽略
                    continue;
                }

                //移除连接上的定时器句柄，并移除指定句柄的定时器
                if let Some(timer) = socket.borrow_mut().unset_timer_handle() {
                    let _ = pool.timer.try_remove(timer);
                }
            }
        }
    }

    //轮询所有超时的定时器，执行已超时回调
    while pool.timer.check_sleep(pool.duration.elapsed().as_millis() as u64) == 0 {
        //需要继续获取超时的回调
        if let Some((item, _index)) = pool.timer.pop(pool.duration.elapsed().as_millis() as u64) {
            //存在超时的回调
            let (token, event) = item.elem;
            if let Some(Some(socket)) = pool
                .sockets
                .lock()
                .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                if socket.borrow().is_closed() {
                    //连接已关闭，则忽略
                    continue;
                }

                //移除连接上的定时器句柄
                socket.borrow_mut().unset_timer_handle();

                //连接已超时
                let handle = socket.borrow().get_handle();
                pool.driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .timeouted(handle, event)
                    .await;
            }
        }
    }
}
