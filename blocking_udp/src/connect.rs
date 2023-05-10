use std::net::{SocketAddr, UdpSocket};
use std::io::{Error, Result, ErrorKind};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};

use futures::future::LocalBoxFuture;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use dashmap::DashMap;
use bytes::{Buf, Bytes};

use pi_async::rt::serial_local_thread::LocalTaskRuntime;
use crate::{Socket, UdpEvent, TaskResult,
            terminal::alloc_terminal_socket_uid,
            utils::{UdpMultiInterface, UdpSocketStatus, SocketContext}};

///
/// 阻塞的Udp连接
///
pub type BlockingUdpSocket = Arc<InnerBlockingUdpSocket>;

///
/// 构建一个阻塞的终端Udp连接
///
pub fn new_blocking_udp_socket(local: SocketAddr,
                               remote: Option<SocketAddr>,
                               socket: Arc<UdpSocket>,
                               read_block_size: usize,
                               write_block_size: usize,
                               write_send: Sender<UdpEvent>) -> BlockingUdpSocket {
    let uid = alloc_terminal_socket_uid();
    let read_block_size = AtomicUsize::new(read_block_size);
    let write_block_size = AtomicUsize::new(write_block_size);
    let status = RwLock::new(UdpSocketStatus::SingleCast(None));
    let context = SocketContext::empty();
    let closed = Arc::new(AtomicBool::new(false));
    let inner = InnerBlockingUdpSocket {
        uid,
        local,
        remote,
        socket,
        read_block_size,
        write_block_size,
        write_send,
        status,
        context,
        closed,
    };

    Arc::new(inner)
}

// 内部阻塞的Udp连接
pub struct InnerBlockingUdpSocket {
    uid:                usize,                      //连接唯一id
    local:              SocketAddr,                 //连接本地地址
    remote:             Option<SocketAddr>,         //连接远端地址
    socket:             Arc<UdpSocket>,             //内部Udp连接
    read_block_size:    AtomicUsize,                //读取报文块的大小
    write_block_size:   AtomicUsize,                //写入报文块的大小
    write_send:         Sender<UdpEvent>,           //待写入报文发送器
    status:             RwLock<UdpSocketStatus>,    //连接状态
    context:            SocketContext,              //连接上下文
    closed:             Arc<AtomicBool>,            //连接是否已关闭
}

unsafe impl Send for InnerBlockingUdpSocket {}
unsafe impl Sync for InnerBlockingUdpSocket {}

impl Socket for InnerBlockingUdpSocket {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn get_uid(&self) -> usize {
        self.uid
    }

    fn get_local(&self) -> &SocketAddr {
        &self.local
    }

    fn get_remote(&self) -> Option<&SocketAddr> {
        self.remote.as_ref()
    }

    fn get_status(&self) -> UdpSocketStatus {
        (*self.status.read()).clone()
    }

    fn set_ttl(&self, ttl: u32) -> Result<()> {
        self.socket.set_ttl(ttl)
    }

    fn join_multicast_group(&self,
                            address: SocketAddr,
                            interface: UdpMultiInterface,
                            ttl: u32,
                            is_loop: bool) -> Result<()> {
        match address {
            SocketAddr::V4(addr0) => {
                match interface {
                    UdpMultiInterface::V4(SocketAddr::V4(addr1)) => {
                        if !self.status.read().is_multicast() {
                            //当前不是多播状态，则设置为多播状态
                            *self.status.write() = UdpSocketStatus::MultiCast(Arc::new(DashMap::new()));
                        }
                        self.status.write().insert_multicast(address.clone());
                        self.socket.join_multicast_v4(addr0.ip(), addr1.ip())?;
                        self.socket.set_multicast_ttl_v4(ttl)?;
                        self.socket.set_multicast_loop_v4(is_loop)
                    },
                    _ => unimplemented!(),
                }
            },
            SocketAddr::V6(addr0) => {
                match interface {
                    UdpMultiInterface::V6(interface) => {
                        if !self.status.read().is_multicast() {
                            //当前不是多播状态，则设置为多播状态
                            *self.status.write() = UdpSocketStatus::MultiCast(Arc::new(DashMap::new()));
                        }
                        self.status.write().insert_multicast(address.clone());
                        self.socket.join_multicast_v6(addr0.ip(), interface)?;
                        self.socket.set_multicast_loop_v6(is_loop)
                    },
                    _ => unimplemented!(),
                }
            },
        }
    }

    fn leave_multicast_group(&self,
                             address: SocketAddr,
                             interface: UdpMultiInterface) -> Result<()> {
        match address {
            SocketAddr::V4(addr0) => {
                match interface {
                    UdpMultiInterface::V4(SocketAddr::V4(addr1)) => {
                        if self.status.read().remove_multicast(&address) == 0 {
                            //当前已退出所有多播组，则设置状态为单播状态
                            *self.status.write() = UdpSocketStatus::SingleCast(None);
                        }
                        self.socket.leave_multicast_v4(addr0.ip(), addr1.ip())
                    },
                    _ => unimplemented!(),
                }
            },
            SocketAddr::V6(addr0) => {
                match interface {
                    UdpMultiInterface::V6(interface) => {
                        if self.status.read().remove_multicast(&address) == 0 {
                            //当前已退出所有多播组，则设置状态为单播状态
                            *self.status .write()= UdpSocketStatus::SingleCast(None);
                        }
                        self.socket.leave_multicast_v6(addr0.ip(), interface)
                    },
                    _ => unimplemented!(),
                }
            },
        }
    }

    fn enable_broadcast(&self) -> Result<()> {
        self
            .socket
            .set_broadcast(true)
    }

    fn disable_boradcast(&self) -> Result<()> {
        self
            .socket
            .set_broadcast(false)
    }

    fn get_context(&self) -> &SocketContext {
        &self.context
    }

    fn get_read_block_len(&self) -> usize {
        self.read_block_size.load(Ordering::Relaxed)
    }

    fn set_read_block_len(&self, len: usize) {
        self.read_block_size.store(len, Ordering::Relaxed);
    }

    fn get_write_block_len(&self) -> usize {
        self.write_block_size.load(Ordering::Relaxed)
    }

    fn set_write_block_len(&self, len: usize) {
        self.write_block_size.store(len, Ordering::Relaxed);
    }

    fn spawn(&self, task: LocalBoxFuture<'static, TaskResult>) {
        self.write_send.send(UdpEvent::Task(task));
    }

    fn connect(&self, to: SocketAddr) -> Result<()> {
        self.socket.connect(to)
    }

    fn write(&self,
             buf: Vec<u8>,
             peer: Option<SocketAddr>) -> Result<()> {
        let to = if peer.is_none() {
            //如果已设置了对端地址
            self.remote
        } else {
            //如果未设置了对端地址
            peer
        };

        if let Err(e) = self
            .write_send
            .send(UdpEvent::Sent(self.uid,
                                 buf,
                                 to)) {
            Err(Error::new(ErrorKind::Other,
                           format!("Write udp data failed, uid: {:?}, local: {:?}, peer: {:?}, reason: {:?}",
                                   self.uid,
                                   self.local,
                                   peer,
                                   e)))
        } else {
            Ok(())
        }
    }

    fn recv(&self) -> Result<(Vec<u8>, Option<SocketAddr>)> {
        let read_block_size = self.read_block_size.load(Ordering::Relaxed);
        let mut buf = Vec::with_capacity(read_block_size);
        buf.resize(read_block_size, 0);

        if self.remote.is_some() {
            //已绑定对端地址
            match self.socket.recv(buf.as_mut_slice()) {
                Err(e) => Err(e),
                Ok(len) => {
                    buf.truncate(len); //只保留填充了数据的部分
                    Ok((buf, None))
                },
            }
        } else {
            //未绑定对端地址
            match self.socket.recv_from(buf.as_mut_slice()) {
                Err(e) => Err(e),
                Ok((len, peer)) => {
                    buf.truncate(len); //只保留填充了数据的部分
                    Ok((buf, Some(peer)))
                },
            }
        }
    }

    fn send(&self,
            mut buf: Bytes,
            peer: Option<SocketAddr>) -> Result<usize> {
        //将数据分包
        let mut size = 0; //本次发送的字节数
        let write_block_size = self.write_block_size.load(Ordering::Relaxed);
        while buf.remaining() > 0 {
            let len = if buf.remaining() > write_block_size {
                write_block_size
            } else {
                buf.remaining()
            };
            let packet = buf.copy_to_bytes(len);

            if self.remote.is_some() {
                //已绑定对端地址
                size += self
                    .socket
                    .send(packet.to_vec().as_slice())?;
            } else {
                //未绑定对端地址
                if let Some(addr) = peer {
                    size += self
                        .socket
                        .send_to(packet.to_vec().as_slice(), addr)?;
                }
            }
        }

        Ok(size)
    }

    fn close(&self, reason: Result<()>) -> Result<()> {
        if let Ok(true) = self.closed.compare_exchange(false,
                                                       true,
                                                       Ordering::AcqRel,
                                                       Ordering::Relaxed) {
            //当前已关闭，则忽略
            return Ok(());
        }

        //发送连接关闭事件
        if let Err(e) = self.write_send.send(UdpEvent::CloseConnection(self.uid, reason)) {
            Err(Error::new(ErrorKind::Other,
                           format!("Close udp socket failed, uid: {:?}, local: {:?}, remote: {:?}, reason: {:?}",
                                   self.uid,
                                   self.local,
                                   self.remote,
                                   e)))
        } else {
            Ok(())
        }
    }
}
