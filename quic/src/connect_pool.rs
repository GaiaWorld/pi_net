use std::rc::Rc;
use std::sync::Arc;
use std::cell::UnsafeCell;
use std::cmp::max;
use std::time::{Instant, Duration};
use std::io::{Error, Result, ErrorKind};
use std::collections::{VecDeque, HashMap};

use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded};
use quinn_proto::{EndpointEvent, ConnectionEvent, Event, ConnectionHandle, Dir, Transmit, StreamEvent, StreamId, Endpoint,
                  Event::{Connected, ConnectionLost, DatagramReceived, HandshakeDataReady, Stream}};
use dashmap::DashMap;
use slotmap::{Key, KeyData};
use bytes::{Buf, BufMut};
use log::{debug, warn, error};

use pi_async::rt::{serial::AsyncRuntime,
                   serial_local_thread::LocalTaskRuntime};
use pi_cancel_timer::Timer;
use udp::{Socket, SocketHandle as UdpSocketHandle};

use tracing::{self, Instrument};

use crate::{AsyncService, SocketHandle, SocketEvent, QuicEvent,
            connect::QuicSocket,
            server::QuicListener,
            utils::{Token, QuicSocketStatus, QuicSocketReady, QuicCloseEvent}};

///
/// Quic端点轮询器
///
pub trait EndPointPoller: 'static {
    /// 推动Quic端点处理Quic端点事件
    fn poll(&self,
            socket: &QuicSocket,
            handle: ConnectionHandle,
            events: VecDeque<EndpointEvent>);
}

///
/// Quic连接池
///
pub struct QuicSocketPool<P: EndPointPoller> {
    id:                     usize,                                          //连接池唯一id
    sockets:                DashMap<usize, Arc<UnsafeCell<QuicSocket>>>,    //Socket连接表
    poller:                 P,                                              //Quic端点轮询器
    event_send:             Sender<QuicEvent>,                              //Quic事件发送器
    event_recv:             Receiver<QuicEvent>,                            //Quic事件接收器
    service:                Arc<dyn AsyncService>,                          //连接服务
    clock:                  Instant,                                        //内部时钟
    timer:                  Timer<(usize, SocketEvent), 100, 60, 3>,        //定时器
    expired:                HashMap<usize, u64>,                            //已到期连接唯一id缓冲
    timeout:                u64,                                            //事件循环间隔时长
}

impl<P: EndPointPoller> QuicSocketPool<P> {
    /// 构建一个Quic连接池
    pub fn new(id: usize,
               poller: P,
               event_send: Sender<QuicEvent>,
               event_recv: Receiver<QuicEvent>,
               service: Arc<dyn AsyncService>,
               clock: Instant,
               timeout: u64) -> Self {
        let sockets = DashMap::default();
        let timer = Timer::default();
        let expired = HashMap::default();

        QuicSocketPool {
            id,
            sockets,
            poller,
            event_send,
            event_recv,
            service,
            clock,
            timer,
            expired,
            timeout,
        }
    }

    /// 在指定运行时上运行Quic连接池
    pub fn run(self,
               rt: LocalTaskRuntime<()>) {
        let mut pool = self;
        let timeout = Duration::from_millis(pool.timeout);
        rt.spawn(event_loop(rt.clone(), pool, timeout));
    }
}

// Quic连接事件循环
#[inline]
fn event_loop<P: EndPointPoller>(rt: LocalTaskRuntime<()>,
                                 mut pool: QuicSocketPool<P>,
                                 timeout: Duration) -> LocalBoxFuture<'static, ()> {
    async move {
        poll_timer(&rt, &mut pool); //推动定时器

        match pool.event_recv.recv_timeout(timeout) {
            Err(e) if e.is_disconnected() => {
                error!("Loop quic connection pool failed, reason: {:?}", e);
                return;
            },
            Err(_) => {
                //等待接收事件超时，继续事件循环
                ()
            },
            Ok(event) => {
                //接收到事件
                match event {
                    QuicEvent::Accepted(quic_socket) => {
                        //已接受Quic连接请求
                        handle_udp_accepted(&rt,
                                            &mut pool,
                                            quic_socket);
                    },
                    QuicEvent::ConnectionReceived(handle, connection_event) => {
                        //接收Quic连接报文
                        handle_connection_received(&rt,
                                                   &mut pool,
                                                   handle,
                                                   connection_event);
                    },
                    QuicEvent::ConnectionSend(handle, trasmit) => {
                        //发送Quic连接报文
                        handle_connection_sended(&rt,
                                                 &mut pool,
                                                 handle,
                                                 trasmit);
                    },
                    QuicEvent::StreamReady(handle, ready) => {
                        //Quic连接流已就绪
                        handle_streams(&rt,
                                       &mut pool,
                                       handle,
                                       ready);
                    },
                    QuicEvent::StreamWrite(handle, stream_id, bin) => {
                        //Quic连接流写入
                        handle_stream_write_event(&mut pool,
                                                  handle,
                                                  stream_id,
                                                  bin);
                    },
                    QuicEvent::RebindUdp(new_udp_handle) => {
                        //Quic重绑定Udp连接句柄
                        handle_rebind_udp(&mut pool, new_udp_handle);
                    },
                    QuicEvent::Timeout(handle, timeout_event) => {
                        //Quic连接超时
                        handle_timeout_timer(&mut pool,
                                             handle,
                                             timeout_event);
                    },
                    QuicEvent::StreamClose(handle, stream_id, code) => {
                        //Quicl连接流关闭
                        handle_stream_close(&rt,
                                            &mut pool,
                                            handle,
                                            stream_id,
                                            code);
                    },
                    QuicEvent::ConnectionClose(handle) => {
                        //Quic连接关闭
                        handle_connection_close(&rt,
                                                &mut pool,
                                                handle);
                    },
                }
            },
        }

        poll_connection(&rt, &mut pool); //推动所有连接
        rt.spawn(event_loop(rt.clone(), pool, timeout));
    }.boxed_local()
}

// 处理已接受的Udp连接
#[inline]
fn handle_udp_accepted<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                          pool: &mut QuicSocketPool<P>,
                                          mut socket: QuicSocket) {
    //接受新的Quic连接，并创建Quic连接初始双向流
    socket.set_status(QuicSocketStatus::Handshaking); //为注册成功的连接设置状态为正在握手
    socket.set_runtime(rt.clone()); //为注册成功的连接绑定当前运行时
    socket.set_event_sent(Some(pool.event_send.clone())); //为注册成功的连接绑定Quic事件发送器

    //加入连接池
    let socket_uid = socket.get_uid();
    let socket_shared = Arc::new(UnsafeCell::new(socket));
    pool.sockets.insert(socket_uid, socket_shared);
}

// 轮询连接池定时器
#[inline]
fn poll_timer<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                 pool: &mut QuicSocketPool<P>) {
    //推动连接池定时器
    let now = pool.clock.elapsed().as_millis() as u64;
    while pool.timer.is_ok(now) {
        if let Some((connection_handle_number, timer_event)) = pool.timer.pop(now) {
            if timer_event.is_empty() {
                //指定连接已内部超时，则写入连接超时表
                if pool.sockets.contains_key(&connection_handle_number) {
                    //超时的连接存在，则记录已到期的连接唯一id
                    pool.expired.insert(connection_handle_number, now);
                }
            } else {
                //指定连接已外部超时
                if let Some(item) = pool.sockets.get(&connection_handle_number) {
                    let socket = item.value();
                    if unsafe { (&*socket.get()).is_closed() } {
                        //连接已关闭，则忽略
                        continue;
                    }

                    //移除连接上的定时器句柄
                    unsafe { (&mut *socket.get()).unset_timer_handle(); }

                    //连接已超时，则执行超时回调
                    let service = pool.service.clone();
                    let handle = unsafe { (&*socket.get()).get_socket_handle() };
                    rt.spawn(service.handle_timeouted(handle, Ok(timer_event)));
                }
            }
        }
    }
}

// 推动所有连接
#[inline]
fn poll_connection<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                      pool: &mut QuicSocketPool<P>) {
    //轮询所有连接
    let mut iter = pool.sockets.iter();
    while let Some(item) = iter.next() {
        let connection_handle_number = *item.key();
        let socket = item.value();

        unsafe {
            let mut keep_going = false;
            let now = (&*socket.get())
                .get_clock()
                .clone();
            let connect = (&mut *socket.get())
                .get_connect_mut();

            //处理当前连接的超时
            if let Some(_expired_time) = pool.expired.get(&connection_handle_number) {
                connect.handle_timeout(pool.clock);
            }

            //推动连接下行数据报文的生成
            let mut count = 0;
            while let Some(t) = connect.poll_transmit(now, 16) {
                count += match t.segment_size {
                    None => 1,
                    Some(s) => (t.contents.len() + s - 1) / s,
                };

                //写入下行数据报文
                if let Err(e) = (&*socket.get()).write_transmit(t) {
                    //TODO...
                }

                if count > 20 {
                    //处理的下行数据报文已达上限，则可以继续处理
                    keep_going = true;
                    break;
                }
            }

            //推动连接定时器
            if let Some(time) = connect.poll_timeout() {
                //连接的下次到期时间存在
                let next_expire_time = time.elapsed().as_millis() as usize;
                if let Some(timer_ref) = (&*socket.get()).get_timer_ref() {
                    //当前连接已设置定时器
                    if let Some(expired) = (&*socket.get()).get_expired() {
                        //当前连接已设置了下个到期时间
                        if (expired != &time) {
                            //当前连接的下次到期时间不匹配，则重置当前连接的定时器到下次到期时间
                            let _ = pool.timer.cancel(KeyData::from_ffi(timer_ref).into());
                            let key = pool.timer.push(next_expire_time, (connection_handle_number, SocketEvent::empty()));
                            (&mut *socket.get()).set_timer_ref(Some(key.data().as_ffi()));
                        }
                    } else {
                        //当前连接未设置下次到期时间，则重置当前连接的定时器到下次到期时间
                        let _ = pool.timer.cancel(KeyData::from_ffi(timer_ref).into());
                        let key = pool.timer.push(next_expire_time, (connection_handle_number, SocketEvent::empty()));
                        (&mut *socket.get()).set_timer_ref(Some(key.data().as_ffi()));
                    }
                } else {
                    //当前连接未设置定时器，则设置当前连接的定时器到下次到期时间
                    let key = pool.timer.push(next_expire_time, (connection_handle_number, SocketEvent::empty()));
                    (&mut *socket.get()).set_timer_ref(Some(key.data().as_ffi()));
                }

                //重置当前连接的下次到期时间
                (&mut *socket.get()).set_expired(Some(time));
            } else {
                //连接的下次到期时间不存在，则重置当前连接的下次到期时间为空
                (&mut *socket.get()).set_expired(None);
            }
            if (&*socket.get()).get_expired().is_none() {
                keep_going = false;
            }

            //推动连接所在端点事件的处理
            let mut endpoint_events = VecDeque::new();
            while let Some(endpoint_event) = connect.poll_endpoint_events() {
                debug!("!!!!!!endpoint_event: {:?}", endpoint_event);
                endpoint_events.push_back(endpoint_event);
            }
            pool
                .poller
                .poll((&*socket.get()),
                      ConnectionHandle(connection_handle_number),
                      endpoint_events);

            //推动连接应用事件的处理
            while let Some(app_event) = connect.poll() {
                match app_event {
                    HandshakeDataReady => {
                        //TODO 当前连接握手数据已就绪...
                        debug!("Quic handshake data ready, uid: {:?}, remtoe: {:?}, local: {:?}",
                            (&*socket.get()).get_uid(),
                            (&*socket.get()).get_remote(),
                            (&*socket.get()).get_local());
                    }
                    Connected => {
                        //当前连接已建立，则为当前连接打开初始的双向流
                        (&mut *socket.get()).set_status(QuicSocketStatus::Connecting); //为注册成功的连接设置状态为Quic建立连接中，正在打开初始化流
                        debug!("Quic connect accepted, uid: {:?}, remtoe: {:?}, local: {:?}",
                                (&*socket.get()).get_uid(),
                                (&*socket.get()).get_remote(),
                                (&*socket.get()).get_local());

                        if (&mut *socket.get()).is_client() {
                            //当前连接是客户端的连接
                            let socket_mut = (&mut *socket.get());
                            socket_mut.set_socket_handle(socket.clone()); //为已建立的连接设置Quic连接句柄
                            socket_mut.set_status(QuicSocketStatus::Connected); //为已建立的连接设置状态为Quic连接已建立

                            let service = pool.service.clone();
                            rt.spawn(service.handle_connected(socket_mut.get_socket_handle(), Ok(()))); //异步调用连接回调
                        }
                    }
                    ConnectionLost { reason } => {
                        //当前连接已丢失，则立即关闭当前连接
                        (&mut *socket.get()).close(0,
                                                   Err(Error::new(ErrorKind::ConnectionAborted,
                                                                  format!("Quic connect closed, uid: {:?}, remtoe: {:?}, local: {:?}, code: 0, reason: {:?}",
                                                                          (&*socket.get()).get_uid(),
                                                                          (&*socket.get()).get_remote(),
                                                                          (&*socket.get()).get_local(),
                                                                          reason))));

                        debug!("Quic connect closed, uid: {:?}, remtoe: {:?}, local: {:?}",
                            (&*socket.get()).get_uid(),
                            (&*socket.get()).get_remote(),
                            (&*socket.get()).get_local());
                    }
                    Stream(StreamEvent::Writable { id }) => {
                        //TODO 当前连接的指定流的可写事件...
                    }
                    Stream(StreamEvent::Opened { dir: Dir::Uni }) => {
                        //TODO 当前连接打开一个或多个单向流事件...
                    }
                    Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                        //TODO 当前连接打开一个或多个双向流事件...
                        debug!("Quic bi stream opened , uid: {:?}, remtoe: {:?}, local: {:?}",
                                 (&*socket.get()).get_uid(),
                                 (&*socket.get()).get_remote(),
                                 (&*socket.get()).get_local());

                        if let Some(stream_id) = (&mut *socket.get()).get_connect_mut().streams().accept(Dir::Bi) {
                            let socket_mut = (&mut *socket.get());
                            socket_mut.accept_main_streams(stream_id); //为已建立的连接设置初始双向流
                            socket_mut.set_socket_handle(socket.clone()); //为已建立的连接设置Quic连接句柄
                            socket_mut.set_status(QuicSocketStatus::Connected); //为已建立的连接设置状态为Quic连接已建立

                            debug!("Open bi stream accepted, uid: {:?}, remtoe: {:?}, local: {:?}, stream_id: {:?}",
                                (&*socket.get()).get_uid(),
                                (&*socket.get()).get_remote(),
                                (&*socket.get()).get_local(),
                                stream_id);
                            let service = pool.service.clone();
                            rt.spawn(service.handle_connected(socket_mut.get_socket_handle(), Ok(()))); //异步调用连接回调
                        } else {
                            //创建Quic连接初始流失败，则继续处理下一个接受的Quic连接
                            error!("Open bi stream unaccepted, uid: {:?}, remote: {:?}, local: {:?}, reason: invalid init stream",
                                (&*socket.get()).get_uid(),
                                (&*socket.get()).get_remote(),
                                (&*socket.get()).get_local());
                        };
                    }
                    DatagramReceived => {
                        //TODO 当前连接接收到一个或多个Quic数据报文事件...
                    }
                    Stream(StreamEvent::Readable { id }) => {
                        //当前连接的指定流的可读事件
                        (&mut *socket.get()).set_ready(QuicSocketReady::Readable);

                        debug!("Quic stream readable, uid: {:?}, remtoe: {:?}, local: {:?}, stream_id: {:?}",
                            (&*socket.get()).get_uid(),
                            (&*socket.get()).get_remote(),
                            (&*socket.get()).get_local(),
                            id);
                    }
                    Stream(StreamEvent::Available { dir }) => {
                        //TODO 当前连接可打开指定方向的流事件...
                    }
                    Stream(StreamEvent::Finished { id }) => {
                        //当前连接的指定流已完成
                        if (&mut *socket.get()).is_send_finished() {
                            //当前连接的所有流都已完成，则继续关闭当前连接
                            pool
                                .event_send
                                .send(QuicEvent::ConnectionClose(ConnectionHandle(connection_handle_number)));
                        }

                        debug!("Quic stream finished, uid: {:?}, remtoe: {:?}, local: {:?}, stream_id: {:?}",
                            (&*socket.get()).get_uid(),
                            (&*socket.get()).get_remote(),
                            (&*socket.get()).get_local(),
                            id);
                    }
                    Stream(StreamEvent::Stopped { id, error_code }) => {
                        //当前连接的指定流被对端关闭，则移除定时器，并继续移除当前连接
                        let code: u32 = error_code.into_inner() as u32;

                        if (&mut *socket.get()).is_send_finished() {
                            //当前连接的所有流都已完成，则继续关闭当前连接
                            (&mut *socket.get()).close(code,
                                                       Err(Error::new(ErrorKind::ConnectionAborted,
                                                                      format!("Quic stream closed, uid: {:?}, remtoe: {:?}, local: {:?}, stream_id: {:?}, code: {:?}, reason: peer already closed",
                                                                              (&*socket.get()).get_uid(),
                                                                              (&*socket.get()).get_remote(),
                                                                              (&*socket.get()).get_local(),
                                                                              code,
                                                                              id))));
                        }

                        debug!("Quic stream closed, uid: {:?}, remtoe: {:?}, local: {:?}, stream_id: {:?}, code: {:?}",
                            (&*socket.get()).get_uid(),
                            (&*socket.get()).get_remote(),
                            (&*socket.get()).get_local(),
                            id,
                            error_code);
                    }
                }
            }
        }
    }
}

// 处理所有Quic连接接收和超时
#[inline]
fn handle_connection_received<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                                 pool: &mut QuicSocketPool<P>,
                                                 connection_handle: ConnectionHandle,
                                                 event: ConnectionEvent) {
    match pool.sockets.get(&connection_handle.0) {
        None => {
            //指定连接不存在，则忽略
            return;
        },
        Some(item) => {
            //处理当前连接产生的Socket事件
            let socket = item.value();
            unsafe {
                let connect = (&mut *socket.get())
                    .get_connect_mut();
                connect.handle_event(event);
            }
        },
    }
}

// 处理所有准备写数据报文的Quic连接
#[inline]
fn handle_connection_sended<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                               pool: &mut QuicSocketPool<P>,
                                               connection_handle: ConnectionHandle,
                                               transmit: Transmit) {
    if let Some(item) = pool.sockets.get(&connection_handle.0) {
        //准备写数据报文的Quic连接存在
        let socket = item.value();
        unsafe {
            if let Err(e) = (&*socket.get()).send_transmit(transmit) {
                //TODO 发送数据报文错误...
            }
        }
    }
}

//处理Quic连接的写事件
#[inline]
fn handle_stream_write_event<P: EndPointPoller>(pool: &mut QuicSocketPool<P>,
                                                connection_handle: ConnectionHandle,
                                                stream_id: Option<StreamId>,
                                                buf: Vec<u8>) {
    //当前连接池有连接流的写事件
    if let Some(item) = pool.sockets.get(&connection_handle.0) {
        //写事件指定的连接存在
        let socket = item.value();
        unsafe {
            (&mut *socket.get()).set_ready(QuicSocketReady::Writable);
            if let Some(stream_id) = stream_id {
                //TODO 向当前连接的指定流的写缓冲写入数据...
            } else {
                //向当前连接的主流的写缓冲写入数据
                if let Some(write_buf) = (&mut *socket.get()).get_write_buffer() {
                    write_buf.put_slice(buf.as_ref());
                }
            }
        }
    }
}

// 处理所有Quic连接的流
#[inline]
fn handle_streams<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                     pool: &mut QuicSocketPool<P>,
                                     connction_handle: ConnectionHandle,
                                     ready: QuicSocketReady) {
    //处理所有连接流的就绪请求
    if let Some(item) = pool.sockets.get(&connction_handle.0) {
        //指定的连接存在
        let socket = item.value();
        if ready.is_readable() {
            //连接当前读就绪
            let socket_copy = socket.clone();
            let service = pool.service.clone();

            rt.spawn(async move {
                let mut close_reason: Option<Result<()>> = None;
                let socket_mut = unsafe { (&mut *socket_copy.get()) };

                match socket_mut.recv() {
                    Ok(0) => {
                        //本次接收已阻塞，则立即结束本次接收，当前流会等待下次接收
                        return;
                    },
                    Ok(len) => {
                        //按需接收完成，则执行已读回调
                        if socket_mut.is_wait_wakeup_read_ready() {
                            //当前连接有需要唤醒的异步准备读取器，则唤醒当前的异步准备读取器
                            socket_mut.wakeup_read_ready();
                        } else {
                            if socket_mut.is_hibernated() {
                                //当前连接已休眠，则将本次读任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                                let socket_handle = socket_mut.get_socket_handle();
                                socket_mut.push_hibernated_task(service.handle_readed(socket_handle, Ok(len)));
                            } else {
                                //当前连接没有需要唤醒的异步准备读取器，则调用接收回调
                                let socket_handle = socket_mut.get_socket_handle();
                                drop(socket_mut); //在继续调用前释放连接引用
                                service.handle_readed(socket_handle, Ok(len)).await;
                            }
                        }
                    },
                    Err(e) => {
                        //按需接收失败，准备关闭当前连接
                        close_reason = Some(Err(e));
                    },
                }
            });
        }

        if ready.is_writable() {
            //连接当前写就绪
            let socket_copy = socket.clone();
            let service = pool.service.clone();

            rt.spawn(async move {
                let mut close_reason: Option<Result<()>> = None;
                let socket_mut = unsafe { (&mut *socket_copy.get()) };

                match socket_mut.send() {
                    Ok(_len) => {
                        //发送完成，并执行已写回调
                        if socket_mut.is_hibernated() {
                            //当前连接已休眠，则将本次写任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                            let socket_handle = socket_mut.get_socket_handle();
                            socket_mut.push_hibernated_task(service.handle_writed(socket_handle, Ok(())));
                        } else {
                            //调用发送回调
                            let socket_handle = socket_mut.get_socket_handle();
                            drop(socket_mut); //在继续调用前释放连接引用
                            service.handle_writed(socket_handle, Ok(())).await;
                        }
                    },
                    Err(e) => {
                        //发送失败，准备关闭当前连接
                        close_reason = Some(Err(e));
                    },
                }
            });
        }
    }
}

// 批量处理Quic连接流的关闭事件
#[inline]
fn handle_stream_close<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                          pool: &mut QuicSocketPool<P>,
                                          connection_handle: ConnectionHandle,
                                          stream_id: Option<StreamId>,
                                          code: u32) {
    //关闭指定连接的指定流
    if let Some(item) = pool
        .sockets
        .get(&connection_handle.0) {
        //如果指定令牌的Quic连接存在
        let socket = item.value();

        unsafe {
            //强制关闭连接
            let (code, reason) = if let Some((code, reason)) = (&mut *socket.get()).get_close_reason() {
                //有连接关闭的原因
                (*code,
                 format!("Closed connect, reason: {:?}",
                         reason))
            } else {
                //无连接关闭的原因
                (0,
                 format!("Closed connect, reason: normal"))
            };
            (&mut *socket.get()).force_close(code, reason);

            //关闭连接的流
            if (&mut *socket.get()).is_client() {
                //当前连接是客户端，则直接关闭连接
                pool
                    .event_send
                    .send(QuicEvent::ConnectionClose(ConnectionHandle(connection_handle.0)));
            } else {
                //当前连接是服务端，且已完成所有发送，则继续关闭当前连接的流
                if let Err(e) = (&mut *socket.get()).shutdown(code) {
                    error!("{:?}", e);
                } else {
                    //关闭当前连接的流成功
                    if code == 0 {
                        //当前连接的对端已关闭，则继续关闭当前连接
                        pool
                            .event_send
                            .send(QuicEvent::ConnectionClose(connection_handle));
                    }
                }
            }
        }
    }
}

// 批量处理Quic连接的关闭事件
// 关闭指定的Quic连接，并清理上下文
#[inline]
fn handle_connection_close<P: EndPointPoller>(rt: &LocalTaskRuntime<()>,
                                              pool: &mut QuicSocketPool<P>,
                                              connection_handle: ConnectionHandle) {
    //关闭指定连接
    if let Some((_, socket)) = pool.sockets.remove(&connection_handle.0) {
        //指定关闭的连接存在
        unsafe {
            //取消连接的定时器
            if let Some(timer_ref) = (&mut *socket.get()).unset_timer_ref() {
                let _ = pool.timer.cancel(KeyData::from_ffi(timer_ref).into());
            }

            //异步执行已关闭回调
            let socket_handle = (&*socket.get()).get_socket_handle();
            let stream_id = if let Some(id) = (&*socket.get()).get_main_stream_id() {
                Some(id.clone())
            } else {
                None
            };
            let (code, reason) = if let Some(e) = (&mut *socket.get()).close_reason() {
                e
            } else {
                (0, Err(Error::new(ErrorKind::Other, format!("Closed connect, reason: normal"))))
            };
            let closed = if reason.is_err() {
                //因为内部错误，关闭Quic连接
                pool.service
                    .handle_closed(socket_handle, stream_id, code, reason)
            } else {
                //正常关闭Quic连接
                pool.service
                    .handle_closed(socket_handle, stream_id, code, Ok(()))
            };
            rt.spawn(async move {
                //执行关闭回调
                closed.await;

                //因为Quic连接句柄握住了当前连接，所以需要移除当前连接绑定的Quic连接句柄，以释放当前连接
                (&mut *socket.get()).remove_socket_handle();
            });
        }
    }
}

// 处理Quic连接重绑定Udp连接句柄
#[inline]
fn handle_rebind_udp<P: EndPointPoller>(pool: &mut QuicSocketPool<P>,
                                        udp_handle: UdpSocketHandle) {
    //轮询所有连接
    let mut iter = pool.sockets.iter();
    while let Some(item) = iter.next() {
        let socket = item.value();

        unsafe {
            //替换Quic连接上的Udp连接句柄
            (&*socket.get())
                .get_socket_handle()
                .set_local(udp_handle.get_local().clone());
            (&mut *socket.get()).set_udp_handle(udp_handle.clone());

            //通知对端重绑定Udp连接句柄
            (&mut *socket.get()).get_connect_mut().ping();
        }
    }
}

// 处理Quic连接的外部定时器
#[inline]
async fn handle_timeout_timer<P: EndPointPoller>(pool: &mut QuicSocketPool<P>,
                                                 connection_handle: ConnectionHandle,
                                                 opt: Option<(usize, SocketEvent)>) {
    //设置或取消指定Tcp连接的定时器
    if let Some((timeout, event)) = opt {
        //为指定令牌的连接设置指定的定时器
        if let Some(item) = pool.sockets.get(&connection_handle.0) {
            let socket = item.value();
            if unsafe { (&*socket.get()).is_closed() } {
                //连接已关闭，则忽略
                return;
            }

            if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
                //连接已设置定时器，则先移除指定句柄的定时器
                let _ = pool.timer.cancel(KeyData::from_ffi(timer).into());
            }

            //设置指定事件的定时器，并在连接上设置定时器句柄
            let timer = pool.timer.push(timeout, (connection_handle.0, event));
            unsafe { (&mut *socket.get()).set_timer_handle(timer.data().as_ffi()); }
        }
    } else {
        //为指定令牌的连接取消指定的定时器
        if let Some(item) = pool.sockets.get(&connection_handle.0) {
            let socket = item.value();
            if unsafe { (&*socket.get()).is_closed() } {
                //连接已关闭，则忽略
                return;
            }

            //移除连接上的定时器句柄，并移除指定句柄的定时器
            if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
                let _ = pool.timer.cancel(KeyData::from_ffi(timer).into());
            }
        }
    }
}
