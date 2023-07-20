use std::thread;
use std::time::Duration;
use std::io::{Error, Result, ErrorKind};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};

use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded};
use async_channel::{Sender as AsyncSender, Receiver as AsyncReceiver, unbounded as async_unbounded};
use socket2::{Domain, Type, Protocol, Socket as SocketDef};
use dashmap::DashMap;
use log::error;

use pi_async_rt::rt::{AsyncValueNonBlocking,
                   serial_local_thread::LocalTaskRuntime};

use crate::{Socket, AsyncService, SocketHandle, SocketHandleRef, UdpEvent, TaskResult,
            connect::new_blocking_client_udp_socket};

/// 默认的线程堆栈大小
const DEFAULT_THREAD_STACK_SIZE: usize = 2 * 1024 * 1024;

/// 最小的接收报文大小，单位字节
const MIN_RECV_BUFFER_LEN: usize = 0xffff;

/// 最小的发送报文大小，单位字节
const MIN_SEND_BUFFER_LEN: usize = 0xffff;

/// 最小的读取报文大小，单位字节
const MIN_READ_PACKET_BLOCK_LEN: usize = 4096;

/// 最小的写入报文大小，单位字节
const MIN_WRITE_PACKET_BLOCK_LEN: usize = 4096;

lazy_static! {
    static ref CLIENT_SOCKET_UID_ALLOCTOR: AtomicUsize = AtomicUsize::new(1);
}

///
/// 分配服务端Udp连接唯一id
///
pub fn alloc_client_socket_uid() -> usize {
    CLIENT_SOCKET_UID_ALLOCTOR.fetch_add(1, Ordering::Relaxed)
}

///
/// Udp客户端
///
pub struct UdpClient(Arc<InnerUdpClient>);

impl Clone for UdpClient {
    fn clone(&self) -> Self {
        UdpClient(self.0.clone())
    }
}

/*
* Udp客户端同步方法
*/
impl UdpClient {
    /// 绑定指定配置的客户端Udp连接
    pub fn new(rt: LocalTaskRuntime<()>,        //运行时
               mut recv_buffer_size: usize,     //接收缓冲区大小
               mut send_buffer_size: usize,     //发送缓冲区大小
               mut read_packet_size: usize,     //读报文大小
               mut write_packet_size: usize,    //写报文大小
    ) -> Result<Self> {
        //配置Udp客户端
        if recv_buffer_size < MIN_RECV_BUFFER_LEN {
            recv_buffer_size = MIN_RECV_BUFFER_LEN
        }
        if send_buffer_size < MIN_SEND_BUFFER_LEN {
            send_buffer_size = MIN_SEND_BUFFER_LEN;
        }

        //启动事件处理循环
        let rt_copy = rt.clone();
        let (event_sender, event_receiver) = unbounded();
        let connections = DashMap::new();
        let inner = InnerUdpClient {
            rt,
            recv_buffer_size,
            send_buffer_size,
            read_packet_size,
            write_packet_size,
            connections,
            event_sender,
        };
        let rt_clone = rt_copy.clone();
        let client = UdpClient(Arc::new(inner));
        rt_copy.spawn(event_loop(rt_clone,
                            client.clone(),
                            event_receiver));

        Ok(client)
    }

    /// 判断Udp客户端是否没有连接
    pub fn is_empty(&self) -> bool {
        self.0.connections.is_empty()
    }

    /// 获取Udp客户端的连接数
    pub fn len(&self) -> usize {
        self.0.connections.len()
    }

    /// 获取指定唯一id的Udp客户端连接
    pub fn get_connection(&self, uid: &usize) -> Option<UdpClientConnection> {
        if let Some(item) = self.0.connections.get(uid) {
            Some(item.value().clone())
        } else {
            None
        }
    }

    /// 关闭Udp客户端
    pub fn close(self) -> Result<()> {
        if let Err(e) = self.0.event_sender.send(UdpEvent::CloseListener(Ok(()))) {
            Err(Error::new(ErrorKind::Other,
                           format!("Close udp client failed, reason: {:?}",
                                    e)))
        } else {
            Ok(())
        }
    }
}

// 事件处理循环
#[inline]
fn event_loop(rt: LocalTaskRuntime<()>,
              client: UdpClient,
              event_receiver: Receiver<UdpEvent>) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut closeing = false;
        loop {
            match event_receiver.recv() {
                Err(e) => {
                    //事件接收错误，则立即退出
                    error!("Udp event loop failed, reason: {:?}", e);
                    closeing = true; //准备关闭Udp客户端
                    for connection in &client.0.connections {
                        let socket_handle = connection.0.handle.clone();
                        let running = connection.0.running.clone();

                        connection
                            .0
                            .service
                            .handle_closed(socket_handle.clone(),
                                           Err(Error::new(ErrorKind::Other,
                                                          format!("Udp event loop failed, reason: {:?}",
                                                                  e.clone()))))
                            .await;
                        running.store(false, Ordering::Relaxed); //退出指定连接的接收循环
                    }

                    break;
                },
                Ok(event) => {
                    match event {
                        UdpEvent::Recv(uid, packet, peer) => {
                            //处理接收事件
                            if let Some(connection) = client.get_connection(&uid) {
                                connection
                                    .0
                                    .service
                                    .handle_received(connection.0.handle.clone(),
                                                     Ok((packet, peer))).await;
                            }
                        },
                        UdpEvent::Sent(uid, packet, peer) => {
                            //处理发送事件
                            if let Some(connection) = client.get_connection(&uid) {
                                let result = connection.0.handle.send(packet.into(), peer);
                                connection
                                    .0
                                    .service
                                    .handle_sended(connection.0.handle.clone(), result).await;
                            }
                        },
                        UdpEvent::Task(task) => {
                            //处理异步事件
                            if let TaskResult::ExitReady = task.await {
                                //立即退出事件处理循环
                                return;
                            }
                        },
                        UdpEvent::CloseConnection(uid, reason) => {
                            //处理关闭连接事件
                            if let Some((_uid, connection)) = client.0.connections.remove(&uid) {
                                let socket_handle = connection.0.handle.clone();
                                let running = connection.0.running.clone();

                                connection
                                    .0
                                    .service
                                    .handle_closed(socket_handle.clone(),
                                                   reason)
                                    .await;
                                running.store(false, Ordering::Relaxed); //退出指定连接的接收循环
                            }

                            if closeing && client.is_empty() {
                                //准备关闭Udp客户端，且当前没有任何连接，则立即退出事件处理循环
                                break;
                            }
                        },
                        UdpEvent::CloseListener(_reason) => {
                            //处理关闭监听器事件
                            closeing = true; //准备关闭Udp客户端
                            for item in &client.0.connections {
                                item.value().close(Ok(()));
                            }
                        },
                    }
                },
            }
        }
    }.boxed_local()
}

/*
* Udp客户端异步方法
*/
impl UdpClient {
    /// 异步连接到指定的服务端
    pub async fn connect(&self,
                         local: SocketAddr,
                         remote: SocketAddr,
                         recv_buffer_size: Option<usize>,
                         send_buffer_size: Option<usize>,
                         read_packet_size: Option<usize>,
                         write_packet_size: Option<usize>,
                         mut service: Option<Box<dyn AsyncService>>) -> Result<UdpClientConnection> {
        let domain = if local.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = SocketDef::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

        //配置Udp连接
        let recv_buffer_size = if let Some(size) = recv_buffer_size {
            size
        } else {
            self.0.recv_buffer_size
        };
        socket.set_recv_buffer_size(recv_buffer_size)?;
        let send_buffer_size = if let Some(size) = send_buffer_size {
            size
        } else {
            self.0.send_buffer_size
        };
        socket.set_send_buffer_size(send_buffer_size)?;
        let read_packet_size = if let Some(size) = read_packet_size {
            size
        } else {
            self.0.read_packet_size
        };
        let write_packet_size = if let Some(size) = write_packet_size {
            size
        } else {
            self.0.write_packet_size
        };

        //绑定指定的本地地址，并连接指定的远端地址
        socket.bind(&local.into())?;
        socket.connect(&remote.into())?;
        let socket: UdpSocket = socket.into();
        let running = Arc::new(AtomicBool::new(true));
        let client_socket = new_blocking_client_udp_socket(local.clone(),
                                                           Some(remote),
                                                           Arc::new(socket),
                                                           read_packet_size,
                                                           write_packet_size,
                                                           self.0.event_sender.clone());

        //创建并注册客户端连接
        let client = self.clone();
        let handle = client_socket.clone();
        let service = if let Some(mut s) = service {
            s.bind_runtime(self.0.rt.clone()); //为服务绑定Udp客户端所在运行时
            UdpClientService::Service(s)
        } else {
            let (send, recv) = async_unbounded();
            UdpClientService::Queue(send, recv)
        };
        let inner = InnerUdpClientConnection {
            client,
            handle,
            running: running.clone(),
            service,
        };
        let connection = UdpClientConnection(Arc::new(inner));
        self
            .0
            .connections
            .insert(client_socket.get_uid(), connection.clone());

        //启动连接接收循环
        let event_sender = self.0.event_sender.clone();
        thread::Builder::new()
            .name(format!("UDP-Socket-Receiver-{}", local))
            .stack_size(DEFAULT_THREAD_STACK_SIZE)
            .spawn(move || {
                let socket_handle_ref = client_socket.as_ref();
                while running.load(Ordering::Relaxed) {
                    if let Err(e) = recv_loop(socket_handle_ref, &event_sender) {
                        error!("{:?}", e);
                        break;
                    }
                }
            });

        Ok(connection)
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
                           format!("Udp receive loop failed, uid: {:?}, local: {:?}, peer: {:?}, reason: {:?}",
                                   socket_handle.get_uid(),
                                   socket_handle.get_local(),
                                   socket_handle.get_remote(),
                                   e)))
        },
        Ok((packet, peer)) => {
            //接收对端的数据成功，则将数据发送给事件处理循环
            if let Err(e) = event_sender.send(UdpEvent::Recv(socket_handle.get_uid(), packet, peer)) {
                Err(Error::new(ErrorKind::Other,
                               format!("Udp receive loop failed, uid: {:?}, local: {:?}, peer: {:?}, reason: {:?}",
                                       socket_handle.get_uid(),
                                       socket_handle.get_local(),
                                       socket_handle.get_remote(),
                                       e)))
            } else {
                Ok(())
            }
        },
    }
}

// 内部Udpp客户端
struct InnerUdpClient {
    rt:                 LocalTaskRuntime<()>,                                           //运行时
    recv_buffer_size:   usize,                                                          //接收缓冲区大小
    send_buffer_size:   usize,                                                          //发送缓冲区大小
    read_packet_size:   usize,                                                          //读报文大小
    write_packet_size:  usize,                                                          //写报文大小
    connections:        DashMap<usize, UdpClientConnection>,                            //连接表
    event_sender:       Sender<UdpEvent>,                                               //连接事件发送器
}

///
/// Udp客户端连接
///
pub struct UdpClientConnection(Arc<InnerUdpClientConnection>);

unsafe impl Send for UdpClientConnection {}
unsafe impl Sync for UdpClientConnection {}

impl Clone for UdpClientConnection {
    fn clone(&self) -> Self {
        UdpClientConnection(self.0.clone())
    }
}

/*
* Udp客户端连接同步方法
*/
impl UdpClientConnection {
    /// 线程安全的判断连接是否关闭
    pub fn is_closed(&self) -> bool {
        self
            .0
            .handle
            .is_closed()
    }

    /// 线程安全的获取连接唯一id
    pub fn get_uid(&self) -> usize {
        self
            .0
            .handle
            .get_uid()
    }

    /// 线程安全的获取连接本地地址
    pub fn get_local(&self) -> &SocketAddr {
        self
            .0
            .handle
            .get_local()
    }

    /// 线程安全的获取连接远端地址
    pub fn get_remote(&self) -> Option<&SocketAddr> {
        self
            .0
            .handle
            .get_remote()
    }

    /// 线程安全的设置非多播报文的生存周期
    pub fn set_ttl(&self, ttl: u32) -> Result<()> {
        self
            .0
            .handle
            .set_ttl(ttl)
    }

    /// 线程安全的写
    pub fn write(&self, buf: Vec<u8>) -> Result<()> {
        self
            .0
            .handle
            .write(buf, None)
    }

    /// 线程安全的关闭连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        if let Err(e) = self
            .0
            .client
            .0
            .event_sender
            .send(UdpEvent::CloseConnection(self.get_uid(),
                                            reason)) {
            Err(Error::new(ErrorKind::Other,
                           format!("Close client udp connection failed, uid: {:?}, local: {:?}, peer: {:?}, reason: {:?}",
                                   self.get_uid(),
                                   self.get_local(),
                                   self.get_remote(),
                                   e)))
        } else {
            Ok(())
        }
    }
}

/*
* Udp客户端连接异步方法
*/
impl UdpClientConnection {
    /// 线程安全的读取下个数据报文
    pub async fn read(&self) -> Option<Result<(Vec<u8>, Option<SocketAddr>)>> {
        if let UdpClientService::Queue(_sender, receiver) = &self.0.service {
            match receiver.recv().await {
                Err(e) => {
                    //异步通道已关闭
                    None
                },
                Ok(result) => {
                    Some(result)
                },
            }
        } else {
            //异步通道不存在
            None
        }
    }
}

// 内部Udp客户端连接
struct InnerUdpClientConnection {
    client:             UdpClient,          //客户端
    handle:             SocketHandle,       //连接句柄
    running:            Arc<AtomicBool>,    //连接接收循环开关
    service:            UdpClientService,   //连接服务
}

enum UdpClientService {
    //异步消息接收器
    Queue(AsyncSender<Result<(Vec<u8>, Option<SocketAddr>)>>, AsyncReceiver<Result<(Vec<u8>, Option<SocketAddr>)>>),
    //异步服务
    Service(Box<dyn AsyncService>),
}

impl UdpClientService {
    /// 异步处理已接收
    pub async fn handle_received(&self,
                                 handle: SocketHandle,
                                 result: Result<(Vec<u8>, Option<SocketAddr>)>) {
        match self {
            UdpClientService::Queue(sender, _receiver) => {
                sender.send(result);
            },
            UdpClientService::Service(service) => {
                service.handle_received(handle, result).await;
            },
        }
    }

    /// 异步处理已发送
    pub async fn handle_sended(&self,
                               handle: SocketHandle,
                               result: Result<usize>) {
        if let UdpClientService::Service(service) = self {
            service.handle_sended(handle, result).await;
        }
    }

    /// 异步处理已关闭
    pub async fn handle_closed(&self,
                               handle: SocketHandle,
                               result: Result<()>) {
        if let UdpClientService::Service(service) = self {
            service.handle_closed(handle, result).await;
        }
    }
}




