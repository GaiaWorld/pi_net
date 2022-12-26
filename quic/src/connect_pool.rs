use std::rc::Rc;
use std::sync::Arc;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::time::{Instant, Duration};
use std::io::{Error, Result, ErrorKind};
use std::thread;

use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded};
use quinn_proto::{EndpointEvent, ConnectionEvent, Event, ConnectionHandle, Dir, Transmit, StreamEvent, StreamId};
use dashmap::DashMap;
use slotmap::{Key, KeyData};
use bytes::{Buf, BufMut};
use log::{debug, warn, error};

use pi_cancel_timer::Timer;
use pi_async::rt::{serial::AsyncRuntime,
                   serial_local_thread::LocalTaskRuntime};
use quinn_proto::Event::{Connected, ConnectionLost, DatagramReceived, HandshakeDataReady, Stream};
use rustls::Connection;

use udp::{Socket,
          connect::UdpSocket};

use crate::{AsyncService, SocketHandle, SocketEvent,
            connect::QuicSocket,
            utils::{Token, QuicSocketStatus, QuicSocketReady, QuicCloseEvent}};

///
/// Quic连接池
///
pub struct QuicSocketPool<S: Socket = UdpSocket> {
    id:                     usize,                                                      //连接池唯一id
    sockets:                DashMap<usize, Arc<UnsafeCell<QuicSocket<S>>>>,             //Socket连接表
    endpoint_event_sent:    Sender<(ConnectionHandle, EndpointEvent)>,                  //连接监听器端点事件发送器
    socket_recv:            Receiver<QuicSocket<S>>,                                    //Socket接收器
    read_recv:              Receiver<(ConnectionHandle, ConnectionEvent)>,              //Socket接收事件接收器
    write_sent:             Sender<(ConnectionHandle, Transmit)>,                       //Socket发送事件发送器
    write_recv:             Receiver<(ConnectionHandle, Transmit)>,                     //Socket发送事件接收器
    ready_sent:             Sender<(ConnectionHandle, QuicSocketReady)>,                //连接流就绪请求发送器
    ready_recv:             Receiver<(ConnectionHandle, QuicSocketReady)>,              //连接流就绪请求接收器
    sender_sent:            Sender<(ConnectionHandle, Option<StreamId>, Vec<u8>)>,      //连接写事件发送器
    sender_recv:            Receiver<(ConnectionHandle, Option<StreamId>, Vec<u8>)>,    //连接写事件接收器
    service:                Arc<dyn AsyncService<S>>,                                   //连接服务
    clock:                  Instant,                                                    //内部时钟
    timer:                  Timer<(usize, SocketEvent), 100, 60, 3>,                    //定时器
    timer_sent:             Sender<(ConnectionHandle, Option<(usize, SocketEvent)>)>,   //定时器设置事件的发送器
    timer_recv:             Receiver<(ConnectionHandle, Option<(usize, SocketEvent)>)>, //定时器设置事件的接收器
    expired:                HashMap<usize, u64>,                                        //已到期连接唯一id缓冲
    close_sent:             Sender<QuicCloseEvent>,                                     //关闭事件的发送器
    close_recv:             Receiver<QuicCloseEvent>,                                   //关闭事件的接收器
    timeout:                Option<usize>,                                              //事件循环间隔时长
}

impl<S: Socket> QuicSocketPool<S> {
    /// 构建一个Quic连接池
    pub fn new(id: usize,
               endpoint_event_sent: Sender<(ConnectionHandle, EndpointEvent)>,
               socket_recv: Receiver<QuicSocket<S>>,
               read_recv: Receiver<(ConnectionHandle, ConnectionEvent)>,
               write_sent: Sender<(ConnectionHandle, Transmit)>,
               write_recv: Receiver<(ConnectionHandle, Transmit)>,
               service: Arc<dyn AsyncService<S>>,
               clock: Instant,
               timeout: Option<usize>) -> Self {
        let sockets = DashMap::default();
        let (ready_sent, ready_recv) = unbounded();
        let (sender_sent, sender_recv) = unbounded();
        let timer = Timer::default();
        let (timer_sent, timer_recv) = unbounded();
        let expired = HashMap::default();
        let (close_sent, close_recv) = unbounded();

        QuicSocketPool {
            id,
            sockets,
            endpoint_event_sent,
            socket_recv,
            read_recv,
            write_sent,
            write_recv,
            ready_sent,
            ready_recv,
            sender_sent,
            sender_recv,
            service,
            clock,
            timer,
            timer_sent,
            timer_recv,
            expired,
            close_sent,
            close_recv,
            timeout,
        }
    }

    /// 在指定运行时上运行Quic连接池
    pub fn run(self,
               rt: LocalTaskRuntime<()>) {
        let mut pool = self;
        let rt_copy = rt.clone();
        rt.spawn(event_loop(rt_copy, pool));
    }
}

// Quic连接事件循环
#[inline]
fn event_loop<S>(rt: LocalTaskRuntime<()>,
                 mut pool: QuicSocketPool<S>) -> LocalBoxFuture<'static, ()>
    where S: Socket {
    async move {
        let now = Instant::now();

        handle_udp_accepted(&rt, &mut pool);

        poll_timer(&rt, &mut pool);

        handle_connection_readed(&rt, &mut pool);

        handle_connection_writed(&rt, &mut pool);

        handle_stream_write_event(&mut pool);

        handle_streams(&rt, &mut pool);

        handle_close(&rt, &mut pool);

        handle_timeout_timer(&mut pool);

        if let Some(timeout) = pool.timeout {
            //如果事件循环执行过快，则强制休眠指定时长
            let sleep_timeout = timeout
                .checked_sub(now.elapsed().as_millis() as usize)
                .unwrap_or(0);
            thread::sleep(Duration::from_millis(sleep_timeout as u64));
        }

        rt.spawn(event_loop(rt.clone(), pool));
    }.boxed_local()
}

// 处理已接受的Udp连接
fn handle_udp_accepted<S>(rt: &LocalTaskRuntime<()>,
                          pool: &mut QuicSocketPool<S>)
    where S: Socket {
    for mut socket in pool.socket_recv.try_iter().collect::<Vec<QuicSocket<S>>>() {
        //接受新的Quic连接，并创建Quic连接初始双向流
        socket.set_status(QuicSocketStatus::Handshaking); //为注册成功的连接设置状态为正在握手
        socket.set_runtime(rt.clone()); //为注册成功的连接绑定当前运行时
        socket.set_write_sent(Some(pool.write_sent.clone())); //为注册成功的连接绑定写入Quic数据报文发送器
        socket.set_timer_listener(Some(pool.timer_sent.clone())); //为注册成功的连接绑定连接超时事件监听器
        socket.set_close_listener(Some(pool.close_sent.clone())); //为注册成功的连接绑定连接关闭事件监听器

        //加入连接池
        let socket_uid = socket.get_uid();
        let socket_shared = Arc::new(UnsafeCell::new(socket));
        pool.sockets.insert(socket_uid, socket_shared);
    }
}

// 轮询连接池定时器
fn poll_timer<S>(rt: &LocalTaskRuntime<()>,
                 pool: &mut QuicSocketPool<S>)
    where S: Socket {
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

// 处理所有Quic连接的读和超时
fn handle_connection_readed<S>(rt: &LocalTaskRuntime<()>,
                               pool: &mut QuicSocketPool<S>)
    where S: Socket {
    //分派连接的Socket事件
    let mut events_map: HashMap<usize, Vec<ConnectionEvent>> = HashMap::default();
    for (connection_handle, event) in pool.read_recv.try_iter().collect::<Vec<(ConnectionHandle, ConnectionEvent)>>() {
        if let Some(events) = events_map.get_mut(&connection_handle.0) {
            //指定连接的事件列表存在
            events.push(event);
        } else {
            //指定连接的事件列表不存在
            let mut vec = Vec::new();
            vec.push(event);
            events_map.insert(connection_handle.0, vec);
        }
    }

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

            //处理当前连接产生的Socket事件
            if let Some(events) = events_map.remove(&connection_handle_number) {
                for event in events {
                    connect.handle_event(event);
                }
            }

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
            while let Some(endpoint_event) = connect.poll_endpoint_events() {
                println!("!!!!!!endpoint_event: {:?}", endpoint_event);
                pool
                    .endpoint_event_sent
                    .send((ConnectionHandle(connection_handle_number), endpoint_event));
            }

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
                            socket_mut.set_ready_sent(Some(pool.ready_sent.clone())); //为已建立的连接设置连接流就绪请求发送器
                            socket_mut.set_write_listener(Some(pool.sender_sent.clone())); //为已建立的连接设置连接写事件发送器
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
                            socket_mut.set_ready_sent(Some(pool.ready_sent.clone())); //为已建立的连接设置连接流就绪请求发送器
                            socket_mut.set_write_listener(Some(pool.sender_sent.clone())); //为已建立的连接设置连接写事件发送器
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
                                .close_sent
                                .send(QuicCloseEvent::CloseConnect(ConnectionHandle(connection_handle_number)));
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

// 处理所有准备写数据报文的Quic连接
fn handle_connection_writed<S>(rt: &LocalTaskRuntime<()>,
                               pool: &mut QuicSocketPool<S>)
    where S: Socket {
    for (connection_handle, transmit) in pool.write_recv.try_iter().collect::<Vec<(ConnectionHandle, Transmit)>>() {
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
}

//处理Quic连接的写事件
fn handle_stream_write_event<S>(pool: &mut QuicSocketPool<S>)
    where S: Socket {
    for (connection_handle, stream_id, buf) in pool
        .sender_recv
        .try_iter()
        .collect::<Vec<(ConnectionHandle, Option<StreamId>, Vec<u8>)>>() {
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
}

// 处理所有Quic连接的流
fn handle_streams<S>(rt: &LocalTaskRuntime<()>,
                     pool: &mut QuicSocketPool<S>)
    where S: Socket {
    for (connction_handle, ready) in pool.ready_recv.try_iter().collect::<Vec<(ConnectionHandle, QuicSocketReady)>>() {
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
}

// 批量处理Quic连接的关闭事件
// 关闭指定的Quic连接，并清理上下文
fn handle_close<S>(rt: &LocalTaskRuntime<()>,
                   pool: &mut QuicSocketPool<S>)
    where S: Socket {
    for close_event in pool
        .close_recv
        .try_iter()
        .collect::<Vec<QuicCloseEvent>>() {
        match close_event {
            QuicCloseEvent::CloseStream(connection_handle, stream_id, code) => {
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
                                .close_sent
                                .send(QuicCloseEvent::CloseConnect(ConnectionHandle(connection_handle.0)));
                        } else {
                            //当前连接是服务端，且已完成所有发送，则继续关闭当前连接的流
                            if let Err(e) = (&mut *socket.get()).shutdown(code) {
                                error!("{:?}", e);
                            } else {
                                //关闭当前连接的流成功
                                if code == 0 {
                                    //当前连接的对端已关闭，则继续关闭当前连接
                                    pool
                                        .close_sent
                                        .send(QuicCloseEvent::CloseConnect(connection_handle));
                                }
                            }
                        }
                    }
                }
            },
            QuicCloseEvent::CloseConnect(mut connection_handle) => {
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
            },
        }
    }
}

// 处理Quic连接的外部定时器
#[inline]
async fn handle_timeout_timer<S>(pool: &mut QuicSocketPool<S>)
    where S: Socket {
    //设置或取消指定Tcp连接的定时器
    for (connection_handle, opt) in pool.timer_recv.try_iter().collect::<Vec<(ConnectionHandle, Option<(usize, SocketEvent)>)>>() {
        if let Some((timeout, event)) = opt {
            //为指定令牌的连接设置指定的定时器
            if let Some(item) = pool.sockets.get(&connection_handle.0) {
                let socket = item.value();
                if unsafe { (&*socket.get()).is_closed() } {
                    //连接已关闭，则忽略
                    continue;
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
                    continue;
                }

                //移除连接上的定时器句柄，并移除指定句柄的定时器
                if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
                    let _ = pool.timer.cancel(KeyData::from_ffi(timer).into());
                }
            }
        }
    }
}
