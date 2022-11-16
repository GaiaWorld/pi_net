use std::mem;
use std::sync::Arc;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::io::{Error, Result, ErrorKind};

use mio::{Poll, Token, Events};
use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded};
use pi_slotmap::{Key, DefaultKey, KeyData, SlotMap};
use log::{warn, error};
use pi_async::{lock::spin_lock::SpinLock,
               rt::{serial::AsyncRuntime,
                    serial_local_thread::LocalTaskRuntime}};

use crate::{Socket, SocketAdapter, SocketDriver, SocketHandle, utils::{SharedSocket, register_close_sender}};

///
/// Udp连接池
///
pub struct UdpSocketPool<
    S: Socket,
    A: SocketAdapter<Connect = S>,
> {
    uid:                u8,                                                             //Udp连接池唯一id
    name:               String,                                                         //Udp连接池名称
    poll:               Arc<SpinLock<Poll>>,                                            //Socket事件轮询器
    sockets:            Arc<SpinLock<SlotMap<DefaultKey, Option<SharedSocket<S>>>>>,    //Socket连接表
    driver:             Option<SocketDriver<S, A>>,                                     //Socket驱动
    socket_recv:        Receiver<S>,                                                    //Socket接收器
    close_sent:         Sender<(Token, Result<()>)>,                                    //关闭事件的发送器
    close_recv:         Receiver<(Token, Result<()>)>,                                  //关闭事件的接收器
}

unsafe impl<
    S: Socket,
    A: SocketAdapter<Connect = S>,
> Send for UdpSocketPool<S, A> {}
unsafe impl<
    S: Socket,
    A: SocketAdapter<Connect = S>,
> Sync for UdpSocketPool<S, A> {}

impl<
    S: Socket,
    A: SocketAdapter<Connect = S>,
> UdpSocketPool<S, A> {
    /// 构建一个Udp连接池
    pub fn new(uid: u8,
               name: String,
               receiver: Receiver<S>,
    ) -> Result<Self> {
        Self::with_capacity(uid, name, receiver, 10)
    }

    /// 构建一个指定初始大小的Udp连接池
    pub fn with_capacity(uid: u8,
                         name: String,
                         receiver: Receiver<S>,
                         size: usize) -> Result<Self> {
        let sockets = Arc::new(SpinLock::new(SlotMap::with_capacity(size)));
        let poll = Arc::new(SpinLock::new(Poll::new()?));

        let (close_sent, close_recv) = unbounded();
        register_close_sender(uid, close_sent.clone()); //注册全局关闭事件发送器

        Ok(UdpSocketPool {
            uid,
            name,
            poll,
            sockets,
            driver: None,
            socket_recv: receiver,
            close_sent,
            close_recv,
        })
    }

    /// 运行Udp连接池
    pub fn run(self,
               rt: LocalTaskRuntime<()>,
               driver: SocketDriver<S, A>,
               event_size: usize,
               timeout: Option<usize>) -> Result<()> {
        let mut pool = self;
        pool.driver = Some(driver);
        let rt_copy = rt.clone();
        rt.spawn(async move {
            let poll_timeout = if let Some(t) = timeout {
                Some(Duration::from_millis(t as u64))
            } else {
                None
            };

            //启动Udp连接事件循环
            event_loop(rt_copy,
                       pool,
                       event_size,
                       poll_timeout).await;
        });

        Ok(())
    }
}

// Udp连接事件循环，当连接同时关注可读和可写事件时，轮询将不会返回任何事件，轮询超时时长会影响到最快响应时间，最快响应时间是实际处理时间加上轮询超时时长
#[inline]
fn event_loop<S, A>(rt: LocalTaskRuntime<()>,
                    mut pool: UdpSocketPool<S, A>,
                    event_size: usize,
                    poll_timeout: Option<Duration>) -> LocalBoxFuture<'static, ()>
    where S: Socket,
          A: SocketAdapter<Connect = S> {
    async move {
        let mut events = Events::with_capacity(event_size);
        handle_binded(&rt, &mut pool);

        if let Err(e) = pool
            .poll
            .lock()
            .poll(&mut events, poll_timeout.clone()) {
            //轮询连接事件错误，则立即退出Udp连接事件循环
            error!("Udp socket pool poll failed, timeout: {:?}, ports: {:?}, reason: {:?}",
                poll_timeout,
                pool.name,
                e);
            return;
        }

        handle_poll_events(&rt, &mut pool, &events);

        handle_close_event(&rt, &mut pool);

        //继续异步调用Udp连接事件循环
        let event_loop = event_loop(rt.clone(),
                                    pool,
                                    event_size,
                                    poll_timeout);
        rt.spawn(event_loop);
    }.boxed_local()
}

// 处理已绑定本地端口的Udp连接
#[inline]
fn handle_binded<S, A>(rt: &LocalTaskRuntime<()>,
                       pool: &mut UdpSocketPool<S, A>)
    where S: Socket,
          A: SocketAdapter<Connect = S> {
    for mut socket in pool.socket_recv.try_iter().collect::<Vec<S>>() {
        //接受的新的Udp连接
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
        if let Err(e) = pool
            .poll
            .lock()
            .registry()
            .register(&mut *socket.get_socket().lock(), token, ready) {
            //连接注册失败
            warn!("Udp socket poll register error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                token,
                socket.get_remote(),
                socket.get_local(),
                e);

            //立即关闭未注册的连接
            if let Err(e) = socket.close(Err(Error::new(ErrorKind::Other, "register socket failed"))) {
                warn!("Udp socket close error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                    token,
                    socket.get_remote(),
                    socket.get_local(),
                    e);
            }
        } else {
            //连接注册成功
            socket.set_close_listener(Some(pool.close_sent.clone())); //为注册成功的连接绑定关闭监听器
            socket.set_runtime(rt.clone()); //为注册成功的连接绑定运行时
            socket.set_token(Some(token)); //为注册成功的连接绑定新的令牌
            socket.set_poll(pool.poll.clone()); //为注册成功的连接设置轮询器
            let socket_arc = SharedSocket::new(socket);
            let handle = {
                socket_arc.borrow_mut().set_handle(socket_arc.inner_ref()); //设置连接句柄
                socket_arc.borrow_mut().get_handle()
            };
            pool.sockets.lock()[id] = Some(socket_arc); //加入连接池上下文

            //异步调用连接绑定本地端口回调任务
            let binded = pool
                .driver
                .as_ref()
                .unwrap()
                .get_adapter()
                .binded(Ok(handle));
            rt.spawn(binded);
        }
    }
}

// 处理Udp连接的轮询事件
#[inline]
fn handle_poll_events<S, A>(rt: &LocalTaskRuntime<()>,
                            pool: &mut UdpSocketPool<S, A>,
                            events: &Events)
    where S: Socket,
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
                rt.spawn(async move {
                    let mut close_reason = None;
                    {
                        let mut s = socket_copy.borrow_mut();
                        match s.recv() {
                            Ok((bin, peer)) => {
                                //接收完成，则重新注册当前Udp连接关注的事件，并执行已读回调
                                let interest = s.get_interest().unwrap();
                                if let Err(e) = poll
                                    .lock()
                                    .registry()
                                    .reregister(&mut *s.get_socket().lock(),
                                                token,
                                                interest) {
                                    //重新注册关注的事件失败
                                    close_reason = Some(Err(e));
                                }

                                //调用接收回调
                                let handle = s.get_handle();
                                mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                                adapter.readed(Ok((handle, bin, peer))).await;
                            },
                            Err(e) => {
                                //按需接收失败，准备关闭当前连接
                                close_reason = Some(Err(e));
                            },
                        }
                    }

                    //关闭轮询时出错的Udp连接
                    if let Some(reason) = close_reason.take() {
                        if let Some(Some(socket)) = sockets
                            .lock()
                            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                            let result = socket.borrow_mut().close(reason); //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                warn!("Udp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
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
                rt.spawn(async move {
                    let mut close_reason = None;
                    {
                        let mut s = socket_copy.borrow_mut();
                        match s.send() {
                            Ok(len) => {
                                //发送完成，并执行已写回调
                                let interest = s.get_interest().unwrap();
                                if let Err(e) = poll
                                    .lock()
                                    .registry()
                                    .reregister(&mut *s.get_socket().lock(),
                                                token,
                                                interest) {
                                    //重新注册关注的事件失败
                                    close_reason = Some(Err(e));
                                }

                                //调用发送回调
                                let handle = s.get_handle();
                                mem::drop(s); //因为后续操作在连接引用的作用域内，所以必须显示释放连接引用，以保证后续可以继续借用连接
                                adapter.writed(Ok(handle)).await;
                            },
                            Err(e) => {
                                //发送失败，准备关闭当前连接
                                close_reason = Some(Err(e));
                            },
                        }
                    }

                    //关闭轮询时出错的Udp连接
                    if let Some(reason) = close_reason.take() {
                        if let Some(Some(socket)) = sockets
                            .lock()
                            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                            let result = socket.borrow_mut().close(reason); //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                warn!("Udp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
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

// 批量处理Udp连接的关闭事件
// 关闭指定的Udp连接，并清理上下文
fn handle_close_event<S, A>(rt: &LocalTaskRuntime<()>,
                            pool: &mut UdpSocketPool<S, A>)
    where S: Socket,
          A: SocketAdapter<Connect = S> {
    for (token, reason) in pool
        .close_recv
        .try_iter()
        .collect::<Vec<(Token, Result<()>)>>() {
        //从连接表中移除被关闭的Udp连接
        if !pool
            .sockets
            .lock()
            .contains_key(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
            //如果指定令牌的Udp连接不存在，则忽略
            return;
        }
        let socket = pool
            .sockets
            .lock()
            .remove(DefaultKey::from(KeyData::from_ffi(token.0 as u64)))
            .unwrap()
            .unwrap();

        //从轮询器中注销Udp连接
        let r = pool
            .poll
            .lock()
            .registry()
            .deregister(&mut *socket.borrow_mut().get_socket().lock());

        //异步执行已关闭回调
        let handle = socket.borrow().get_handle();
        let closed = if let Err(e) = reason {
            //因为内部错误，关闭Udp连接
            pool.driver
                .as_ref()
                .unwrap()
                .get_adapter()
                .closed(Err((handle, e)))
        } else {
            if let Err(e) = r {
                //注销时错误，关闭Udp连接
                pool.driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .closed(Err((handle, e)))
            } else {
                //正常关闭Udp连接
                pool.driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .closed(Ok(handle))
            }
        };
        rt.spawn(closed);
    }
}