use std::rc::Rc;
use std::cell::RefCell;
use std::io::{Error, Result, ErrorKind};
use std::net::{ToSocketAddrs, SocketAddr};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use mio::{Poll, Token, Interest,
          net::UdpSocket as InnerUdpSocket};
use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded};
use bytes::Buf;
use dashmap::DashMap;

use pi_async::{lock::spin_lock::SpinLock,
               rt::serial_local_thread::LocalTaskRuntime};

use crate::{Socket, SocketHandle, SocketContext, SocketImage,
            utils::{UdpMultiInterface, UdpSocketStatus}};
use crate::utils::close_socket;

/// 最小的读取报文大小，单位字节
const MIN_READ_PACKET_BLOCK_LEN: usize = 4096;

/// 默认的读取报文块大小，单位字节
const DEFAULT_READ_PACKET_BLOCK_LEN: usize = 0xffff;

/// 最小的写入报文大小，单位字节
const MIN_WRITE_PACKET_BLOCK_LEN: usize = 4096;

/// 默认的写入报文块大小，单位字节
const DEFAULT_WRITE_PACKET_BLOCK_LEN: usize = 0xffff;

///
/// Udp连接
///
pub struct UdpSocket {
    rt:                 Option<LocalTaskRuntime<()>>,               //连接所在运行时
    local:              SocketAddr,                                 //连接本地地址
    remote:             Option<SocketAddr>,                         //连接远端地址
    token:              Option<Token>,                              //连接令牌
    socket:             SpinLock<InnerUdpSocket>,                   //内部Udp连接
    interest:           Arc<SpinLock<Interest>>,                    //连接当前感兴趣的事件类型
    poll:               Option<Arc<SpinLock<Poll>>>,                //连接所在轮询器
    read_block_len:     usize,                                      //读取报文块的大小
    write_block_len:    usize,                                      //写入报文块的大小
    write_sent:         Sender<(Vec<u8>, Option<SocketAddr>)>,      //待写入报文发送器
    write_recv:         Receiver<(Vec<u8>, Option<SocketAddr>)>,    //待写入报文接收器
    status:             UdpSocketStatus,                            //连接状态
    handle:             Option<SocketHandle<Self>>,                 //连接句柄
    context:            SocketContext,                              //连接上下文
    closed:             Rc<AtomicBool>,                             //连接关闭状态
    close_listener:     Option<Sender<(Token, Result<()>)>>,        //连接关闭事件监听器
}

unsafe impl Send for UdpSocket {}
unsafe impl Sync for UdpSocket {}

impl Socket for UdpSocket {
    fn new(local: SocketAddr,
           remote: Option<SocketAddr>,
           token: Option<Token>,
           socket: InnerUdpSocket,
           read_block_len: usize,
           write_block_len: usize) -> Self {
        let read_packet_size = if read_block_len < MIN_READ_PACKET_BLOCK_LEN {
            DEFAULT_READ_PACKET_BLOCK_LEN
        } else {
            read_block_len
        };

        let write_packet_size = if write_block_len < MIN_WRITE_PACKET_BLOCK_LEN {
            DEFAULT_WRITE_PACKET_BLOCK_LEN
        } else {
            write_block_len
        };

        let socket = SpinLock::new(socket);
        let interest = Arc::new(SpinLock::new(Interest::READABLE));
        let (write_sent, write_recv) = unbounded();
        let status = UdpSocketStatus::SingleCast(None);
        let context = SocketContext::empty();
        let closed = Rc::new(AtomicBool::new(false));

        UdpSocket {
            rt: None,
            local,
            remote,
            token,
            socket,
            interest,
            poll: None,
            read_block_len,
            write_block_len,
            write_sent,
            write_recv,
            status,
            handle: None,
            context,
            closed,
            close_listener: None,
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn set_runtime(&mut self, rt: LocalTaskRuntime<()>) {
        self.rt = Some(rt);
    }

    fn get_handle(&self) -> SocketHandle<Self> {
        self
            .handle
            .as_ref()
            .unwrap()
            .clone()
    }

    fn set_handle(&mut self, shared: &Arc<RefCell<Self>>) {
        if let Some(token) = self.token {
            if let Some(close_listener) = &self.close_listener {
                let image = SocketImage::new(shared,
                                             self.local,
                                             token,
                                             self.closed.clone(),
                                             close_listener.clone()
                );
                self.handle = Some(SocketHandle::new(image));
            }
        }
    }

    fn get_socket(&self) -> &SpinLock<InnerUdpSocket> {
        &self.socket
    }

    fn get_local(&self) -> &SocketAddr {
        &self.local
    }

    fn get_remote(&self) -> Option<&SocketAddr> {
        self.remote.as_ref()
    }

    fn get_token(&self) -> Option<&Token> {
        self.token.as_ref()
    }

    fn set_token(&mut self, token: Option<Token>) -> Option<Token> {
        let last = self.token.take();
        self.token = token;
        last
    }

    fn get_status(&self) -> &UdpSocketStatus {
        &self.status
    }

    fn bind(&mut self, peer: Option<SocketAddr>) -> Result<()> {
        self.status = UdpSocketStatus::SingleCast(peer.clone());

        if let Some(addr) = peer {
            self.remote = Some(addr);
            self.socket.lock().connect(addr)
        } else {
            Ok(())
        }
    }

    fn set_ttl(&self, ttl: u32) -> Result<u32> {
        let result = self.socket.lock().ttl();

        if let Err(e) = self.socket.lock().set_ttl(ttl) {
            Err(e)
        } else {
            result
        }
    }

    fn join_multicast_group(&mut self,
                            address: SocketAddr,
                            interface: UdpMultiInterface,
                            ttl: u32,
                            is_loop: bool) -> Result<()> {
        match address {
            SocketAddr::V4(addr0) => {
                match interface {
                    UdpMultiInterface::V4(SocketAddr::V4(addr1)) => {
                        if !self.status.is_multicast() {
                            //当前不是多播状态，则设置为多播状态
                            self.status = UdpSocketStatus::MultiCast(DashMap::new());
                        }
                        self.status.insert_multicast(address.clone());
                        self.socket.lock().join_multicast_v4(addr0.ip(), addr1.ip())?;
                        self.socket.lock().set_multicast_ttl_v4(ttl)?;
                        self.socket.lock().set_multicast_loop_v4(is_loop)
                    },
                    _ => unimplemented!(),
                }
            },
            SocketAddr::V6(addr0) => {
                match interface {
                    UdpMultiInterface::V6(interface) => {
                        if !self.status.is_multicast() {
                            //当前不是多播状态，则设置为多播状态
                            self.status = UdpSocketStatus::MultiCast(DashMap::new());
                        }
                        self.status.insert_multicast(address.clone());
                        self.socket.lock().join_multicast_v6(addr0.ip(), interface)?;
                        self.socket.lock().set_multicast_loop_v6(is_loop)
                    },
                    _ => unimplemented!(),
                }
            },
        }
    }

    fn leave_multicast_group(&mut self,
                             address: SocketAddr,
                             interface: UdpMultiInterface) -> Result<()> {
        match address {
            SocketAddr::V4(addr0) => {
                match interface {
                    UdpMultiInterface::V4(SocketAddr::V4(addr1)) => {
                        if self.status.remove_multicast(&address) == 0 {
                            //当前已退出所有多播组，则设置状态为单播状态
                            self.status = UdpSocketStatus::SingleCast(None);
                        }
                        self.socket.lock().leave_multicast_v4(addr0.ip(), addr1.ip())
                    },
                    _ => unimplemented!(),
                }
            },
            SocketAddr::V6(addr0) => {
                match interface {
                    UdpMultiInterface::V6(interface) => {
                        if self.status.remove_multicast(&address) == 0 {
                            //当前已退出所有多播组，则设置状态为单播状态
                            self.status = UdpSocketStatus::SingleCast(None);
                        }
                        self.socket.lock().leave_multicast_v6(addr0.ip(), interface)
                    },
                    _ => unimplemented!(),
                }
            },
        }
    }

    fn enable_broadcast(&mut self) -> Result<()> {
        self
            .socket
            .lock()
            .set_broadcast(true)
    }

    fn disable_boradcast(&mut self) -> Result<()> {
        self
            .socket
            .lock()
            .set_broadcast(false)
    }

    fn get_context(&self) -> &SocketContext {
        &self.context
    }

    fn get_context_mut(&mut self) -> &mut SocketContext {
        &mut self.context
    }

    fn get_interest(&self) -> Option<Interest> {
        Some(*self.interest.lock())
    }

    fn set_interest(&self, ready: Interest) {
        *self.interest.lock() = ready;
    }

    fn get_read_block_len(&self) -> usize {
        self.read_block_len
    }

    fn set_read_block_len(&mut self, len: usize) {
        self.read_block_len = len;
    }

    fn get_write_block_len(&self) -> usize {
        self.write_block_len
    }

    fn set_write_block_len(&mut self, len: usize) {
        self.write_block_len = len;
    }

    fn set_poll(&mut self, poll: Arc<SpinLock<Poll>>) {
        self.poll = Some(poll);
    }

    fn set_close_listener(&mut self, listener: Option<Sender<(Token, Result<()>)>>) {
        self.close_listener = listener;
    }

    fn write_ready<B>(&mut self,
                      mut buf: B,
                      peer: Option<SocketAddr>) -> Result<()>
        where B: Buf + Send + 'static {
        if self.is_closed() {
            //连接已关闭，则忽略，并立即返回
            return Err(Error::new(ErrorKind::ConnectionAborted,
                                  format!("Write ready failed, token: {:?}, peer: {:?}, local: {:?}, reason: connection already closed",
                                          self.get_token(),
                                          self.get_remote(),
                                          self.get_local())));
        }

        let peer = if self.remote.is_some() {
            //已绑定对端地址，则忽略指定的对端地址
            None
        } else {
            //未绑定对端地址，则需要指定的对端地址
            if peer.is_none() {
                return Err(Error::new(ErrorKind::ConnectionAborted,
                                      format!("Write ready failed, token: {:?}, peer: {:?}, local: {:?}, reason: empty peer address",
                                              self.get_token(),
                                              self.get_remote(),
                                              self.get_local())));
            }

            peer
        };

        while buf.remaining() > 0 {
            let len = if buf.remaining() > self.write_block_len {
                self.write_block_len
            } else {
                buf.remaining()
            };

            let packet = buf.copy_to_bytes(len);
            self.write_sent.send((packet.to_vec(), peer.clone()));
        }

        //设置连接当前对写事件感兴趣
        if let Some(poll) = &self.poll {
            if let Err(e) = poll.lock()
                .registry()
                .reregister(&mut *self.get_socket().lock(),
                            self.get_token().unwrap().clone(),
                            Interest::WRITABLE) {
                return Err(e);
            }
        }

        Ok(())
    }

    fn recv(&mut self) -> Result<(Vec<u8>, Option<SocketAddr>)> {
        let mut buf = Vec::with_capacity(self.read_block_len);
        buf.resize(self.read_block_len, 0);

        if self.remote.is_some() {
            //已绑定对端地址
            match self.socket.lock().recv(buf.as_mut_slice()) {
                Err(e) => Err(e),
                Ok(len) => {
                    buf.truncate(len); //只保留填充了数据的部分
                    Ok((buf, None))
                },
            }
        } else {
            //未绑定对端地址
            match self.socket.lock().recv_from(buf.as_mut_slice()) {
                Err(e) => Err(e),
                Ok((len, peer)) => {
                    buf.truncate(len); //只保留填充了数据的部分
                    Ok((buf, Some(peer)))
                },
            }
        }
    }

    fn send(&mut self) -> Result<usize> {
        let mut packets: Vec<(Vec<u8>, Option<SocketAddr>)> = self.write_recv.try_iter().collect();

        let mut len = 0; //本次发送的字节数
        if self.remote.is_some() {
            //已绑定对端地址
            for (packet, _) in packets {
                len += self
                    .socket
                    .lock()
                    .send(packet.as_slice())?;
            }
        } else {
            //未绑定对端地址
            for (packet, peer) in packets {
                if let Some(addr) = peer {
                    len += self
                        .socket
                        .lock()
                        .send_to(packet.as_slice(), addr)?;
                }
            }
        }

        Ok(len)
    }

    fn close(&mut self, reason: Result<()>) -> Result<()> {
        //更新连接状态为已关闭
        if let Ok(true) = self.closed.compare_exchange(false,
                                                       true,
                                                       Ordering::AcqRel,
                                                       Ordering::Relaxed) {
            //当前已关闭，则忽略
            return Ok(());
        }

        //通知Udp连接的监听器连接已关闭
        if let Some(listener) = &self.close_listener {
            if let Some(token) = self.get_token() {
                if let Err(e) = listener.send((token.clone(), reason)) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(())
    }
}