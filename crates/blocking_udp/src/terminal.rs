//! 阻塞UDP终端：只在两条原服务初始化链的bind_runtime之前冷登记外部Local。
//! 不修改原socket/接收线程/事件循环/同步recv；登记不保证所属线程采样进度。
//! NET-MET-BATCH-DESIGN-001/B-UDP；参数/Ok专项见batch_runtime_metrics_tests。

use std::thread;
use std::io::{Error, Result, ErrorKind};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Duration;

use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, bounded, unbounded};
use async_channel::unbounded as async_unbounded;
use socket2::{Domain, Type, Protocol, Socket as SocketDef};
use log::{debug, warn, error};

use pi_async_rt::rt::serial_local_thread::LocalTaskRuntime;

use crate::{Socket, AsyncService, SocketHandle, SocketHandleRef, UdpEvent, TaskResult,
            connect::{BlockingUdpSocket, new_blocking_udp_socket}};

// 只在测试构建保存实际参数/原Result，普通库没有此槽或观测写入。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static UDP_METRICS_TEST_RESULT: std::cell::RefCell<Option<(usize, String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>)>> = const { std::cell::RefCell::new(None) };
}

/// 仅为两条终端冷初始化复用原Local登记，不持有/返回身份，不处理生命周期。
/// 借用runtime不跨异步边界，不读本地队列、不派发任务；返回()不代表登记成功。
/// 名称O(L)时间/空间加上游短同步锁/分配；失败logger可能等待，无异步锁/await。
/// 同核同名幂等归上游；成功强保活，外部负责其原生命周期，不在Drop/失败中注销。
/// 功能关闭时无操作。测试：batch_runtime_metrics_tests::test_udp_registers_runtime_metrics。
fn register_terminal_runtime_metrics(runtime: &LocalTaskRuntime<()>) {
    #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
    {
        let metric_name = format!("pi_blocking_udp-local-{}", runtime.get_id());
        let registration = pi_async_rt::runtime_metrics::register_runtime_metrics(runtime, &metric_name);
        #[cfg(test)]
        UDP_METRICS_TEST_RESULT.with(|value| {
            *value.borrow_mut() = Some((runtime.get_id(), metric_name.clone(), registration));
        });
        if let Err(error) = registration {
            warn!("阻塞 UDP 运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
        }
    }
    #[cfg(not(all(feature = "runtime_metrics", not(target_arch = "wasm32"))))]
    let _ = runtime;
}

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod batch_runtime_metrics_tests {
    //! 只验证register_terminal_runtime_metrics向上游传入真实Local/稳定名称且原Result为Ok。
    //! 这是无网络局部单元，不执行UDP/QUIC循环，不声称完整网络冷装配已动态测试。
    //! 各实际调用点/原顺序由整批源码审查核对，注册算法/SDK/生命周期不重复测试。

    use super::*;

    /// 被测生产入口：register_terminal_runtime_metrics，与冷启动实际共用，不手动预登记或模拟上游。
    /// 本测试自有真实空Local，不派发采样/业务任务，不读本地队列；取真实返回ID后
    /// 在断言前显式注销清理，再随空runner释放，不为观测扩大生产可见性。
    /// O(L)命名核对，严格串行，无await/网络/无界等待。
    #[test]
    fn test_udp_registers_runtime_metrics() {
        UDP_METRICS_TEST_RESULT.with(|value| { value.borrow_mut().take(); });
        let runner = pi_async_rt::rt::serial_local_thread::LocalTaskRunner::<()>::new();
        let runtime = runner.get_runtime();
        let expected_id = runtime.get_id();
        register_terminal_runtime_metrics(&runtime);
        let observed = UDP_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (actual_id, actual_name, registration) = observed.expect("实际冷登记必须发生");
        assert_eq!(actual_id, expected_id);
        assert_eq!(actual_name, format!("pi_blocking_udp-local-{}", expected_id));
        assert!(registration.is_ok(), "真实登记必须成功: {:?}", registration);
    }
}

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
    ///
    /// 原参数、网络I/O、错误和推进者责任保持；在服务首次取得rt前冷登记，失败仅告警。
    /// 名称/强保活/同步等待边界见register_terminal_runtime_metrics；不在关闭时注销。
    /// 专项batch_runtime_metrics_tests只核对真实helper参数/Ok，非网络绑定验收。
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
        register_terminal_runtime_metrics(&rt);
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
                        debug!("{:?}", e);
                    }
                }
                warn!("Udp receive loop already exited, local: {:?}", socket_handle.get_local());
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
    ///
    /// 原返回闭包消费service并初始化时，才在bind_runtime之前冷登记；放弃闭包不登记。
    /// 返回值/网络/派发顺序不变，不增加生命周期处理；登记边界及专项同bind。
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
                        debug!("{:?}", e);
                    }
                }
                warn!("Udp receive loop already exited, local: {:?}", socket_handle.get_local());
            });

        //生成初始化闭包
        let socket_handle = udp_socket.clone();
        let (closed_send, closed_recv) = bounded(1);
        let func = move |mut service: Box<dyn AsyncService>| {
            let rt_copy = rt.clone();

            //为服务绑定Udp所在运行时
            register_terminal_runtime_metrics(&rt);
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

    /// 向连接所在运行时派发指定的异步任务
    pub fn spawn(&self, task: LocalBoxFuture<'static, TaskResult>) {
        self.1.spawn(task)
    }

    /// 关闭Udp连接
    pub fn close_connection(&self, reason: Result<()>) -> Result<()> {
        self.1.close(reason)
    }

    /// 关闭Udp终端
    pub fn close(self, reason: Result<()>) -> Result<()> {
        let mut server_local_address = self.1.get_local().clone();
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
                //关闭监听器成功，则关闭UDP端口
                let mut tmp_client_address = server_local_address.clone();
                tmp_client_address.set_port(0);
                let tmp_client = UdpSocket::bind(tmp_client_address)?;
                server_local_address.set_ip(IpAddr::from([127, 0, 0, 1]));
                tmp_client.connect(server_local_address)?;
                let _ = tmp_client.send(&[0])?;
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


