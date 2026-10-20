//! TCP连接池；只在run的原驱动准备后/首个send前登记外部Local核心。
//! 无生产ID存储、注销、等待或热路径指标逻辑，原事件循环及关闭保持。
//! NET-MET-BATCH-DESIGN-001/B-TCP；参数/Ok专项见batch_runtime_metrics_tests。

use std::rc::Rc;
use std::sync::Arc;
use std::cell::UnsafeCell;
use std::time::{Duration, Instant};
use std::net::{Shutdown, SocketAddr};
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, LocalBoxFuture};
use mio::{Events, Poll, Token, Interest};
use crossbeam_channel::{Sender, Receiver, unbounded};
use bytes::BufMut;
use slotmap::{Key as SlotMapKey, KeyData as SlotMapKeyData};
use spin_sleep_util::{self, Interval};
use log::{warn, error};

use pi_async_rt::{lock::spin_lock::SpinLock,
                  rt::{serial::AsyncValue,
                       serial_local_thread::LocalTaskRuntime}};
use pi_hash::XHashMap;
use pi_cancel_timer::Timer;
use slotmap::{Key, DefaultKey, KeyData, SlotMap};

use crate::{Socket, Stream, SocketAdapter, SocketConfig, SocketEvent, SocketDriver,
            utils::register_close_sender};

// 测试专用纯观测，不持核心/锁/回调，不进入普通生产构建。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static TCP_METRICS_TEST_RESULT: std::cell::RefCell<Option<(usize, String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod batch_runtime_metrics_tests {
    //! 仅验证TcpSocketPool::run真实首投递前的登记参数/Ok，不打开网络或执行池循环。
    //! 使用真实原生Local及真实空池/驱动；原首任务已排队，局部单元不宣称网络或关闭验收。
    //! 原Local没有公开取消排队任务接口，未推进的首任务由独立测试进程退出回收，不新增
    //! 生产清理入口或测试侧伪造排空；登记自有ID在参数断言前显式注销。

    use super::*;
    use crate::{connect::TcpSocket, server::PortsAdapter};

    /// 被测生产：TcpSocketPool::run，不手动预登记，不将run的Ok冒充登记Ok。
    /// 只分配真实mio轮询器，不绑定端口；不跨所属线程调用本地队列方法，无采集/SDK。
    /// 命名核对O(L)，测试入口严格串行/有界，不保护原事件循环或资源析构的全部语义。
    #[test]
    fn test_tcp_pool_registers_runtime_metrics() {
        TCP_METRICS_TEST_RESULT.with(|value| { value.borrow_mut().take(); });
        let runner = pi_async_rt::rt::serial_local_thread::LocalTaskRunner::<()>::new();
        let runtime = runner.get_runtime();
        let expected_id = runtime.get_id();
        let (_sender, receiver) = crossbeam_channel::unbounded::<TcpSocket>();
        let pool = TcpSocketPool::<TcpSocket, PortsAdapter<TcpSocket>>::new(
            254, "metrics-unit".to_string(), receiver, SocketConfig::default(),
        ).expect("真实空TCP池构造");
        let driver = SocketDriver::<TcpSocket, PortsAdapter<TcpSocket>>::new(&[]);
        pool.run(runtime, driver, 1, Some(0)).expect("原首任务投递");
        let observed = TCP_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (actual_id, actual_name, registration) = observed.expect("原run必须实际登记");
        assert_eq!(actual_id, expected_id);
        assert_eq!(actual_name, format!("pi_tcp-local-{}", expected_id));
        assert!(registration.is_ok(), "真实登记必须成功: {:?}", registration);
    }
}

/// TCP连接池，运行时冷登记边界见run；其它池状态及热路径保持原逻辑。
pub struct TcpSocketPool<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    uid:            u8,                                                             //Tcp连接池唯一id
    name:           String,                                                         //Tcp连接池名称
    config:         SocketConfig,                                                   //Tcp连接配置
    poll:           Rc<UnsafeCell<Poll>>,                                           //Socket事件轮询器
    sockets:        Arc<SpinLock<SlotMap<DefaultKey, Option<Arc<UnsafeCell<S>>>>>>, //Socket连接表
    map:            XHashMap<SocketAddr, Token>,                                    //Socket映射表
    driver:         Option<SocketDriver<S, A>>,                                     //Socket驱动
    socket_recv:    Receiver<S>,                                                    //Socket接收器
    write_sent:     Sender<(Token, Vec<u8>)>,                                       //写事件的发送器
    write_recv:     Receiver<(Token, Vec<u8>)>,                                     //写事件的接收器
    close_sent:     Sender<(Token, Result<()>)>,                                    //关闭事件的发送器
    close_recv:     Receiver<(Token, Result<()>)>,                                  //关闭事件的接收器
    duration:       Instant,                                                        //定时器持续时间
    timer:          Timer<(Token, SocketEvent), 100, 60, 3>,                        //定时器
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

        let poll = Rc::new(UnsafeCell::new(Poll::new()?));

        let (write_sent, write_recv) = unbounded();
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
            write_sent,
            write_recv,
            close_sent,
            close_recv,
            duration,
            timer: Timer::<(Token, SocketEvent), 100, 60, 3>::default(),
            timer_sent,
            timer_recv,
        })
    }

    /// 运行Tcp连接池，并设置Socket驱动
    ///
    /// rt是外部已准备的原生Local，driver移动到原池，event_size/timeout保持原事件容量/
    /// 轮询时间单位及合法范围；成功返回Ok仅表示原首任务已投递，不表示任务或采样完成。
    /// 默认冷登记pi_tcp-local-原get_id，失败只告警继续，不增加业务错误、ID字段或注销。
    /// 名称O(L)时间/空间，另有上游登记/原send及logger成本；同步锁可能等待，无新增await/
    /// 异步锁/线程。只在冷启动使用，原send跨线程边界不变，不能从外部线程改用spawn。
    /// 登记由上游强保活，不转移外部核心所有权；原关闭/Drop不因本次增量自动注销。
    /// 生产参数/Ok：batch_runtime_metrics_tests::test_tcp_pool_registers_runtime_metrics；
    /// 原真实网络启动边界另由tests/connect_pool_startup.rs保护，不属本单元结论。
    pub fn run(self,
               rt: LocalTaskRuntime<()>,
               driver: SocketDriver<S, A>,
               event_size: usize,
               timeout: Option<usize>) -> Result<()> {
        let mut pool = self;
        pool.driver = Some(driver);
        #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
        {
            let metric_name = format!("pi_tcp-local-{}", rt.get_id());
            let registration = pi_async_rt::runtime_metrics::register_runtime_metrics(&rt, &metric_name);
            #[cfg(test)]
            TCP_METRICS_TEST_RESULT.with(|value| {
                *value.borrow_mut() = Some((rt.get_id(), metric_name.clone(), registration));
            });
            if let Err(error) = registration {
                warn!("TCP 运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
            }
        }
        let rt_copy = rt.clone();
        // `run` 由监听器启动流程调用，不保证运行在本地运行时所属线程上。
        // 因此首个连接池事件循环必须通过线程安全的外部队列投递；
        // 当该异步任务已经在所属线程上被轮询后，`event_loop` 内部仍可继续使用
        // 本地 `spawn` 完成自调度。
        rt.send(async move {
            //启动Tcp连接事件循环
            let (helper, poll_timeout) = if let Some(poll_timeout) = timeout {
                if cfg!(windows) && poll_timeout < 15000 {
                    (spin_sleep_util::interval(Duration::from_micros(poll_timeout as u64)),
                     Some(Duration::from_micros(0)))
                } else {
                    (spin_sleep_util::interval(Duration::from_micros(poll_timeout as u64)),
                     Some(Duration::from_micros(poll_timeout as u64)))
                }
            } else {
                (spin_sleep_util::interval(Duration::from_micros(100)),
                 Some(Duration::from_millis(0)))
            };
            event_loop(rt_copy,
                       pool,
                       event_size,
                       helper,
                       poll_timeout).await;
        });

        Ok(())
    }
}

// Tcp连接事件循环，当连接同时关注可读和可写事件时，轮询将不会返回任何事件，轮询超时时长会影响到最快响应时间，最快响应时间是实际处理时间加上轮询超时时长
#[inline]
fn event_loop<S, A>(rt: LocalTaskRuntime<()>,
                    mut pool: TcpSocketPool<S, A>,
                    event_size: usize,
                    mut helper: Interval,
                    poll_timeout: Option<Duration>) -> LocalBoxFuture<'static, ()>
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    async move {
        let mut events = Events::with_capacity(event_size);
        handle_accepted(&rt, &mut pool).await;

        handle_write_event(&mut pool).await;

        unsafe {
            let mut is_interrupted = false;
            if let Err(e) = (&mut *pool
                .poll
                .get())
                .poll(&mut events, poll_timeout.clone()) {
                if e.kind() != ErrorKind::Interrupted {
                    //轮询连接事件错误，则立即退出Tcp连接事件循环
                    error!("Tcp socket pool poll failed, timeout: {:?}, ports: {:?}, reason: {:?}",
                        poll_timeout,
                        pool.name,
                        e);
                    return;
                }

                is_interrupted = true;
            } else {
                is_interrupted = false;
            }

            if is_interrupted {
                //轮询连接事件被中断，则继续异步调用Tcp连接事件循环
                let event_loop = event_loop(rt.clone(),
                                            pool,
                                            event_size,
                                            helper,
                                            poll_timeout);
                rt.spawn(event_loop);
                return;
            }
        }

        handle_poll_events(&rt, &mut pool, &events).await;

        handle_close_event(&rt, &mut pool).await;

        handle_timer(&mut pool).await; //必须在关闭处理完成后执行

        //继续异步调用Tcp连接事件循环
        if cfg!(windows) || poll_timeout.is_some_and(|time| time.is_zero()) {
            helper.tick_no_spin(); //开始休眠
        }
        let event_loop = event_loop(rt.clone(),
                                    pool,
                                    event_size,
                                    helper,
                                    poll_timeout);
        rt.spawn(event_loop);
    }.boxed_local()
}

// 处理已接受的Tcp连接
#[inline]
async fn handle_accepted<S, A>(rt: &LocalTaskRuntime<()>,
                               pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    let socket_opts = pool.config.option();

    let mut sockets = pool.socket_recv.try_iter().collect::<Vec<S>>();
    if sockets.len() == 0 {
        match pool.socket_recv.try_recv() {
            Err(e) if e.is_disconnected() => {
                //接受器的发送器已失效，则立即退出当前连接运行时
                let _ = rt.clone().close();
                error!("Handle accepted failed, reason: {:?}", e);
                return;
            },
            Err(_) => (),
            Ok(socket) => {
                sockets.push(socket);
            },
        }
    }

    for mut socket in sockets {
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
        unsafe {
            if let Err(e) = (&mut *pool
                .poll
                .get())
                .registry()
                .register(socket.get_stream_mut(), token, ready) {
                //连接注册失败
                warn!("Tcp socket poll register error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                token,
                socket.get_remote(),
                socket.get_local(),
                e);

                //立即关闭未注册的连接
                if let Err(e) = socket.close(Err(Error::new(ErrorKind::Other, "register socket failed"))) {
                    warn!("Tcp socket close error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                    token,
                    socket.get_remote(),
                    socket.get_local(),
                    e);
                }
            } else {
                //连接注册成功
                socket.set_write_listener(Some(pool.write_sent.clone())); //为注册成功的连接绑定写事件监听器
                socket.set_close_listener(Some(pool.close_sent.clone())); //为注册成功的连接绑定关闭事件监听器
                socket.set_timer_listener(Some(pool.timer_sent.clone())); //为注册成功的连接绑定定时事件监听器
                socket.set_runtime(rt.clone()); //为注册成功的连接绑定运行时
                socket.set_token(Some(token)); //为注册成功的连接绑定新的令牌
                socket.set_uid(((pool.uid as usize) << 56) | token.0); //为注册成功的连接设置唯一id
                socket.set_poll(pool.poll.clone()); //为注册成功的连接设置轮询器
                let socket_arc = Arc::new(UnsafeCell::new(socket));
                let handle = {
                    unsafe {
                        (&mut *socket_arc.get()).set_handle(&socket_arc); //设置连接句柄
                        (&mut *socket_arc.get()).get_handle()
                    }
                };
                pool.sockets.lock()[id] = Some(socket_arc); //加入连接池上下文

                //异步调用连接回调任务
                let connected = pool
                    .driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .connected(Ok(handle));
                rt.spawn(connected);
            }
        }
    }
}

//处理Tcp连接的写事件
#[inline]
async fn handle_write_event<S, A>(pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    for (token, buf) in pool
        .write_recv
        .try_iter()
        .collect::<Vec<(Token, Vec<u8>)>>() {
        //当前连接池有写事件
        if let Some(Some(socket)) = pool
            .sockets
            .lock()
            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
            //写事件指定的连接存在
            let interest = if let Some(interest) = unsafe { (&*socket.get()).get_interest() } {
                //当前连接有感兴趣的事件
                interest
            } else {
                //当前连接没有感兴趣的事件
                Interest::READABLE
            };

            //强制重置连接当前感兴趣的事件
            if let Err(e) = unsafe { (&mut *pool
                .poll
                .get())
                .registry()
                .reregister(unsafe { (&mut *socket.get()).get_stream_mut() }, token,
                            interest.add(Interest::WRITABLE)) } {
                //重新注册关注的事件失败，则关闭出错的Tcp连接
                if let Err(e) = unsafe { (&mut *socket.get()).close(Err(e)) } {
                    //关闭指定连接失败
                    unsafe {
                        warn!("Tcp socket close failed by handle write event, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    (&*socket.get()).get_token(),
                                    (&*socket.get()).get_remote(),
                                    (&*socket.get()).get_local(),
                                    e);
                    }
                }
            }

            unsafe {
                if let Some(write_buf) = (&mut *socket.get()).get_write_buffer() {
                    write_buf.put_slice(buf.as_ref());
                }
            }
        }
    }
}

// 处理Tcp连接的轮询事件
#[inline]
async fn handle_poll_events<S, A>(rt: &LocalTaskRuntime<()>,
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
                rt.spawn(async move {
                    let mut close_reason = None;
                    {
                        let s = unsafe { (&mut *socket_copy.get()) };
                        match s.recv() {
                            Ok(len) => {
                                //按需接收完成，则重新注册当前Tcp连接关注的事件，并执行已读回调
                                if let Some(interest) = s.get_interest() {
                                    //需要修改当前连接感兴趣的事件类型
                                    if let Err(e) = unsafe { (&mut *poll
                                        .get())
                                        .registry()
                                        .reregister(s.get_stream_mut(),
                                                    token,
                                                    interest) } {
                                        //重新注册关注的事件失败
                                        close_reason = Some(Err(e));
                                    }
                                }

                                if s.is_wait_wakeup_read_ready() {
                                    //当前连接有需要唤醒的异步准备读取器，则唤醒当前的异步准备读取器
                                    s.wakeup_read_ready();
                                } else {
                                    if s.is_hibernated() {
                                        //当前连接已休眠，则将本次读任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                                        let handle = s.get_handle();
                                        s.push_hibernated_task(adapter.readed(Ok(handle)));
                                    } else {
                                        //当前连接没有需要唤醒的异步准备读取器，则调用接收回调
                                        let handle = s.get_handle();
                                        drop(s); //在继续调用前释放锁
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
                            let result = unsafe { (&mut *socket.get()).close(reason) }; //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                unsafe {
                                    warn!("Tcp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    (&*socket.get()).get_token(),
                                    (&*socket.get()).get_remote(),
                                    (&*socket.get()).get_local(),
                                    e);
                                }
                            }
                        }
                    }
                });
            } else if event.is_writable() {
                //可写事件，表示写就绪，则异步处理写事件
                rt.spawn(async move {
                    let mut close_reason = None;
                    {
                        let s = unsafe { (&mut *socket_copy.get()) };
                        match s.send() {
                            Ok(len) => {
                                //发送完成，并执行已写回调
                                if let Some(interest) = s.get_interest() {
                                    // 需要修改当前连接感兴趣的事件类型
                                    unsafe {
                                        if let Err(e) = (&mut *poll
                                            .get())
                                            .registry()
                                            .reregister(s.get_stream_mut(), token, interest) {
                                            //重新注册关注的事件失败
                                            close_reason = Some(Err(e));
                                        }
                                    }
                                }

                                if s.is_hibernated() {
                                    //当前连接已休眠，则将本次写任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                                    let handle = s.get_handle();
                                    s.push_hibernated_task(adapter.writed(Ok(handle)));
                                } else {
                                    //调用发送回调
                                    let handle = s.get_handle();
                                    drop(s); //在继续调用前释放锁
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
                            let result = unsafe { (&mut *socket.get()).close(reason) }; //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                unsafe {
                                    warn!("Tcp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    (&*socket.get()).get_token(),
                                    (&*socket.get()).get_remote(),
                                    (&*socket.get()).get_local(),
                                    e);
                                }
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
async fn handle_close_event<S, A>(rt: &LocalTaskRuntime<()>,
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
        pool.map.remove(unsafe { (&*socket.get()).get_remote() });

        //从轮询器中注销Tcp连接
        let r = unsafe {
            (&mut *pool
                .poll
                .get())
                .registry()
                .deregister(unsafe { (&mut *socket.get()).get_stream_mut() })
        };

        //关闭流
        unsafe {
            (&*socket.get())
                .get_stream_ref()
                .shutdown(Shutdown::Both);
        }

        //移除定时器
        if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
            let _ = pool.timer.cancel(SlotMapKeyData::from_ffi(timer as u64).into());
        }

        //异步执行已关闭回调
        let handle = unsafe { (&*socket.get()).get_handle() };
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
        rt.spawn(async move {
            //执行关闭回调
            closed.await;

            //因为Tcp连接句柄握住了当前连接，所以需要移除当前连接绑定的Tcp连接句柄，以释放当前连接
            unsafe {
                (&mut *socket.get()).remove_handle();
            }
        });
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
                if unsafe { (&*socket.get()).is_closed() } {
                    //连接已关闭，则忽略
                    continue;
                }

                if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
                    //连接已设置定时器，则先移除指定句柄的定时器
                    let _ = pool.timer.cancel(SlotMapKeyData::from_ffi(timer as u64).into());
                }

                //设置指定事件的定时器，并在连接上设置定时器句柄
                let current_time = pool.duration.elapsed().as_millis() as u64;
                let timer = pool.timer.push_time(current_time + timeout as u64, (token, event));
                unsafe { (&mut *socket.get()).set_timer_handle(timer.data().as_ffi() as usize); }
            }
        } else {
            //为指定令牌的连接取消指定的定时器
            if let Some(Some(socket)) = pool
                .sockets
                .lock()
                .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                if unsafe { (&*socket.get()).is_closed() } {
                    //连接已关闭，则忽略
                    continue;
                }

                //移除连接上的定时器句柄，并移除指定句柄的定时器
                if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
                    let _ = pool.timer.cancel(SlotMapKeyData::from_ffi(timer as u64).into());
                }
            }
        }
    }

    //轮询所有超时的定时器，执行已超时回调
    let current_time = pool.duration.elapsed().as_millis() as u64;
    while let Some((_key, item)) = pool.timer.pop_kv(current_time) {
        //存在超时的回调
        let (token, event) = item;
        if let Some(Some(socket)) = pool
            .sockets
            .lock()
            .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
            if unsafe { (&*socket.get()).is_closed() } {
                //连接已关闭，则忽略
                continue;
            }

            //移除连接上的定时器句柄
            unsafe { (&mut *socket.get()).unset_timer_handle(); }

            //连接已超时
            let handle = unsafe { (&*socket.get()).get_handle() };
            pool.driver
                .as_ref()
                .unwrap()
                .get_adapter()
                .timeouted(handle, event)
                .await;
        }
    }
}
