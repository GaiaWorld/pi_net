use std::thread;
use std::io::{Error, Result, ErrorKind};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Duration;

use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, bounded, unbounded};
use async_channel::unbounded as async_unbounded;
use socket2::{Domain, Type, Protocol, Socket as SocketDef};
use log::{warn, error};

use pi_async::rt::serial_local_thread::LocalTaskRuntime;

use crate::{Socket, AsyncService, SocketHandle, SocketHandleRef, UdpEvent, TaskResult,
            connect::{BlockingUdpSocket, new_blocking_udp_socket}};

/// 默认的线程堆栈大小
const DEFAULT_THREAD_STACK_SIZE: usize = 2 * 1024 * 1024;

/// 最小的接收缓冲区大小，单位字节
const MIN_RECV_BUFFER_LEN: usize = 0xffff;

/// 最小的发送缓冲区大小，单位字节
const MIN_SEND_BUFFER_LEN: usize = 0xffff;

/// 最小的读取报文大小，单位字节
const MIN_READ_PACKET_BLOCK_LEN: usize = 4096;

/// 最小的写入报文大小，单位字节
const MIN_WRITE_PACKET_BLOCK_LEN: usize = 4096;

//终端连接唯一id分配器
lazy_static! {
    static ref TERMINAL_SOCKET_UID_ALLOCTOR: AtomicUsize = AtomicUsize::new(1);
}

///
/// 分配终端Udp连接唯一id
///
pub fn alloc_terminal_socket_uid() -> usize {
    TERMINAL_SOCKET_UID_ALLOCTOR.fetch_add(1, Ordering::Relaxed)
}

///
/// Udp终端
///
pub struct UdpTerminal(Sender<UdpEvent>, BlockingUdpSocket, Option<SocketAddr>, Receiver<()>);

impl UdpTerminal {
    /// 绑定指定配置的终端Udp连接
    pub fn bind(address: SocketAddr,                //待绑定的Udp地址
                rt: LocalTaskRuntime<()>,           //运行时
                mut recv_buffer_size: usize,        //接收缓冲区大小
                mut send_buffer_size: usize,        //发送缓冲区大小
                mut read_packet_size: usize,        //读报文大小
                mut write_packet_size: usize,       //写报文大小
                mut service: Box<dyn AsyncService>, //地址映射的服务
    ) -> Result<Self> {
        let domain = if address.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = SocketDef::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

        //配置Udp连接
        if recv_buffer_size < MIN_RECV_BUFFER_LEN {
            recv_buffer_size = MIN_RECV_BUFFER_LEN
        }
        socket.set_recv_buffer_size(recv_buffer_size)?;
        if send_buffer_size < MIN_SEND_BUFFER_LEN {
            send_buffer_size = MIN_SEND_BUFFER_LEN;
        }
        socket.set_send_buffer_size(send_buffer_size)?;

        //为服务绑定Udp所在运行时
        service.bind_runtime(rt.clone());

        //绑定指定的本地地址
        socket.bind(&address.into())?;
        let socket: UdpSocket = socket.into();
        let running = Arc::new(AtomicBool::new(true));
        let (event_sender, event_receiver) = unbounded();
        let udp_socket = new_blocking_udp_socket(address.clone(),
                                                 None,
                                                 Arc::new(socket),
                                                 read_packet_size,
                                                 write_packet_size,
                                                 event_sender.clone());

        //启动接收循环
        let running_copy = running.clone();
        let socket_handle = udp_socket.clone();
        let event_sender_copy = event_sender.clone();
        thread::Builder::new()
            .name(format!("UDP-Socket-Receiver-{}", address))
            .stack_size(DEFAULT_THREAD_STACK_SIZE)
            .spawn(move || {
                let socket_handle_ref = socket_handle.as_ref();
                while running_copy.load(Ordering::Relaxed) {
                    if let Err(e) = recv_loop(socket_handle_ref,
                                              &event_sender_copy) {
                        error!("{:?}", e);
                        break;
                    }
                }
            });

        //启动事件处理循环
        let rt_copy = rt.clone();
        let socket_handle = udp_socket.clone();
        let (closed_send, closed_recv) = bounded(1);
        rt.spawn(event_loop(rt_copy,
                            event_receiver,
                            socket_handle,
                            service,
                            running,
                            closed_send));

        Ok(UdpTerminal(event_sender,
                       udp_socket,
                       None,
                       closed_recv))
    }

    /// 绑定指定配置的终端Udp连接，不需要指定服务
    pub fn bind_without_service(address: SocketAddr,            //待绑定的Udp地址
                                rt: LocalTaskRuntime<()>,       //运行时
                                mut recv_buffer_size: usize,    //接收缓冲区大小
                                mut send_buffer_size: usize,    //发送缓冲区大小
                                mut read_packet_size: usize,    //读报文大小
                                mut write_packet_size: usize,   //写报文大小
    ) -> Result<(Self, impl FnOnce(Box<dyn AsyncService>))> {
        let domain = if address.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = SocketDef::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

        //配置Udp连接
        if recv_buffer_size < MIN_RECV_BUFFER_LEN {
            recv_buffer_size = MIN_RECV_BUFFER_LEN
        }
        socket.set_recv_buffer_size(recv_buffer_size)?;
        if send_buffer_size < MIN_SEND_BUFFER_LEN {
            send_buffer_size = MIN_SEND_BUFFER_LEN;
        }
        socket.set_send_buffer_size(send_buffer_size)?;

        //绑定指定的本地地址
        socket.bind(&address.into())?;
        let socket: UdpSocket = socket.into();
        let running = Arc::new(AtomicBool::new(true));
        let (event_sender, event_receiver) = unbounded();
        let udp_socket = new_blocking_udp_socket(address.clone(),
                                                 None,
                                                 Arc::new(socket),
                                                 read_packet_size,
                                                 write_packet_size,
                                                 event_sender.clone());

        //启动接收循环
        let running_copy = running.clone();
        let socket_handle = udp_socket.clone();
        let event_sender_copy = event_sender.clone();
        thread::Builder::new()
            .name(format!("UDP-Socket-Receiver-{}", address))
            .stack_size(DEFAULT_THREAD_STACK_SIZE)
            .spawn(move || {
                let socket_handle_ref = socket_handle.as_ref();
                while running_copy.load(Ordering::Relaxed) {
                    if let Err(e) = recv_loop(socket_handle_ref,
                                              &event_sender_copy) {
                        error!("{:?}", e);
                        break;
                    }
                }
            });

        //生成初始化闭包
        let socket_handle = udp_socket.clone();
        let (closed_send, closed_recv) = bounded(1);
        let func = move |mut service: Box<dyn AsyncService>| {
            let rt_copy = rt.clone();

            //为服务绑定Udp所在运行时
            service.bind_runtime(rt.clone());

            //启动事件处理循环
            rt.spawn(event_loop(rt_copy,
                                event_receiver,
                                socket_handle,
                                service,
                                running,
                                closed_send));
        };

        Ok((UdpTerminal(event_sender,
                        udp_socket,
                        None,
                        closed_recv),
            func))
    }

    /// 是否是客户端
    pub fn is_client(&self) -> bool {
        true
    }

    /// 是否是服务端
    pub fn is_server(&self) -> bool {
        self.2.is_none()
    }

    /// 获取终端Udp连接的唯一id
    pub fn get_uid(&self) -> usize {
        self.1.get_uid()
    }

    /// 连接指定的对端地址，可连接多个对端地址
    pub fn connect_to_peer(&self, _to: SocketAddr) -> Result<BlockingUdpSocket> {
        if self.2.is_some() {
            Err(Error::new(ErrorKind::AlreadyExists,
                           format!("Connect to peer failed, uid: {:?}, remote: {:?}, local: {:?}, reason: already connect to server",
                                   self.1.get_uid(),
                                   self.2,
                                   self.1.get_local())))
        } else {
            Ok(self.1.clone())
        }
    }

    /// 重新连接
    pub fn reconnect_to_peer(&self) -> Result<BlockingUdpSocket> {
        if self.2.is_some() {
            Err(Error::new(ErrorKind::AlreadyExists,
                           format!("Connect to peer failed, uid: {:?}, remote: {:?}, local: {:?}, reason: already connect to server",
                                   self.1.get_uid(),
                                   self.2,
                                   self.1.get_local())))
        } else {
            Ok(self.1.clone())
        }
    }

    /// 连接指定的服务端地址，不允许多次连接
    pub fn connect_to_server(&mut self, to: SocketAddr) -> Result<BlockingUdpSocket> {
        if self.2.is_some() {
            Err(Error::new(ErrorKind::AlreadyExists,
                           format!("Connect to peer failed, uid: {:?}, remote: {:?}, local: {:?}, reason: already connect to server",
                                   self.1.get_uid(),
                                   self.2,
                                   self.1.get_local())))
        } else {
            self.1.connect(to)?;
            self.2 = Some(to);
            Ok(self.1.clone())
        }
    }

    /// 关闭Udp连接
    pub fn close_connection(&self, reason: Result<()>) -> Result<()> {
        self.1.close(reason)
    }

    /// 关闭Udp终端
    pub fn close(self, reason: Result<()>) -> Result<()> {
        if let Err(e) = self.0.send(UdpEvent::CloseListener(reason)) {
            Err(Error::new(ErrorKind::Other,
                           format!("Close Udp listener failed, reason: {:?}",
                                   e)))
        } else {
            if let Err(e) = self.3.recv_timeout(Duration::from_millis(5000 )) {
                Err(Error::new(ErrorKind::Other,
                               format!("Close Udp listener failed, reason: {:?}",
                                       e)))
            } else {
                Ok(())
            }
        }
    }
}

// 接收循环
#[inline]
fn recv_loop(socket_handle: SocketHandleRef,
             event_sender: &Sender<UdpEvent>) -> Result<()> {
    match socket_handle.recv() {
        Err(e) => {
            //接收对端的数据失败，则立即返回错误原因
            Err(Error::new(ErrorKind::Other,
                           format!("Udp receive loop failed, uid: {:?}, local: {:?}, reason: {:?}",
                                   socket_handle.get_uid(),
                                   socket_handle.get_local(),
                                   e)))
        },
        Ok((packet, peer)) => {
            //接收对端的数据成功，则将数据发送给事件处理循环
            if let Err(e) = event_sender.send(UdpEvent::Recv(socket_handle.get_uid(), packet, peer)) {
                Err(Error::new(ErrorKind::Other,
                               format!("Udp receive loop failed, uid: {:?}, local: {:?}, reason: {:?}",
                                       socket_handle.get_uid(),
                                       socket_handle.get_local(),
                                       e)))
            } else {
                Ok(())
            }
        },
    }
}

// 事件处理循环
#[inline]
fn event_loop(rt: LocalTaskRuntime<()>,
              event_receiver: Receiver<UdpEvent>,
              socket_handle: SocketHandle,
              service: Box<dyn AsyncService>,
              recv_loop_running: Arc<AtomicBool>,
              closed_send: Sender<()>) -> LocalBoxFuture<'static, ()> {
    async move {
        //调用服务的处理Udp连接已绑定本地地址的处理方法
        service
            .handle_binded(socket_handle.clone(), Ok(()))
            .await;

        loop {
            match event_receiver.recv() {
                Err(e) => {
                    //事件接收错误，则立即退出
                    error!("Udp event loop failed, local: {:?}, reason: {:?}",
                    socket_handle.get_local(),
                    e);
                    service
                        .handle_received(socket_handle.clone(),
                                         Err(Error::new(ErrorKind::Other, format!("Udp event loop failed, local: {:?}, reason: {:?}",
                                                                                  socket_handle.get_local(),
                                                                                  e)))).await;
                    return;
                },
                Ok(event) => {
                    match event {
                        UdpEvent::Recv(_uid, packet, peer) => {
                            //处理接收事件
                            service
                                .handle_received(socket_handle.clone(),
                                                 Ok((packet, peer))).await;
                        },
                        UdpEvent::Sent(_uid, packet, peer) => {
                            //处理发送事件
                            let result = socket_handle.send(packet.into(), peer);
                            service.handle_sended(socket_handle.clone(), result).await;
                        },
                        UdpEvent::Task(task) => {
                            //处理异步事件
                            if let TaskResult::ExitReady = task.await {
                                //立即退出事件处理循环
                                warn!("Event loop already exited, local: {:?}", socket_handle.get_local());
                                closed_send.send(()); //通知已关闭
                                return;
                            }
                        },
                        UdpEvent::CloseConnection(uid, reason) => {
                            //处理关闭连接事件
                            warn!("Close udp socket, uid: {:?}, local: {:?}, remote: {:?}, reason: {:?}",
                                uid,
                                socket_handle.get_local(),
                                socket_handle.get_remote(),
                                reason);
                            service
                                .handle_closed(socket_handle.clone(),
                                               reason)
                                .await;
                            recv_loop_running.store(false, Ordering::Relaxed); //退出接收循环

                            //异步通知退出当前事件循环
                            socket_handle.spawn(async move {
                                TaskResult::ExitReady
                            }.boxed_local());
                        },
                        UdpEvent::CloseListener(reason) => {
                            //处理关闭监听器事件
                            socket_handle.close(reason); //关闭Udp连接
                        },
                    }
                },
            }
        }
    }.boxed_local()
}


