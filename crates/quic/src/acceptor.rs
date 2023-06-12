use std::rc::Rc;
use std::time::Instant;
use std::net::SocketAddr;
use std::cell::UnsafeCell;

use quinn_proto::{ConnectionHandle, DatagramEvent, ConnectionEvent, Endpoint};
use crossbeam_channel::Sender;
use bytes::BytesMut;

use udp::SocketHandle;

use crate::connect::QuicSocket;
use crate::QuicEvent;

///
/// Quic连接接受器
///
pub struct QuicAcceptor {
    end_point:  Rc<UnsafeCell<Endpoint>>,   //Quic端点
    router:     Vec<Sender<QuicEvent>>,     //Quic连接路由器
    clock:      Instant,                    //内部时钟
}

unsafe impl Send for QuicAcceptor {}
unsafe impl Sync for QuicAcceptor {}

impl QuicAcceptor {
    /// 构建一个Quic连接接受器
    pub fn new(end_point: Rc<UnsafeCell<Endpoint>>,
               router: Vec<Sender<QuicEvent>>,
               clock: Instant) -> Self {
        QuicAcceptor {
            end_point,
            router,
            clock,
        }
    }

    /// 接受Quic连接请求，并将Quic连接路由到指定的连接池中
    /// 如果不是Quic连接请求，则返回Quic报文
    pub fn accept(&self,
                  udp_handle: SocketHandle,
                  bin: Vec<u8>,
                  active_peer: Option<SocketAddr>,
                  readed_read_size_limit: usize,
                  readed_write_size_limit: usize) -> Option<(ConnectionHandle, ConnectionEvent)> {
        let bytes: BytesMut = bin.as_slice().into();
        unsafe {
            match (&mut *self.end_point.get()).handle(self.clock,
                                                      peer(active_peer, udp_handle.get_remote()),
                                                      Some(udp_handle.get_local().ip()),
                                                      None,
                                                      bytes) {
                None => {
                    //忽略无效的连接请求
                    None
                },
                Some((connection_handle, DatagramEvent::NewConnection(connection))) => {
                    //处理新连接请求
                    let sender = &self.router[connection_handle.0 % self.router.len()];
                    let _ = sender.send(QuicEvent::Accepted(QuicSocket::new(udp_handle,
                                                                    connection_handle,
                                                                    connection,
                                                                    readed_read_size_limit,
                                                                    readed_write_size_limit,
                                                                    self.clock)));

                    None
                },
                Some((connection_handle, DatagramEvent::ConnectionEvent(event))) => {
                    //处理Quic报文
                    Some((connection_handle, event))
                },
            }
        }
    }
}

// 获取对端地址
#[inline]
fn peer(active_peer: Option<SocketAddr>,
        binded_peer: Option<&SocketAddr>) -> SocketAddr {
    if let Some(peer) = active_peer {
        peer
    } else {
        if let Some(peer) = binded_peer {
            peer.clone()
        } else {
            panic!("Take peer address failed, reason: invalid peer");
        }
    }
}

