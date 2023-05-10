#![allow(unused)]

use std::io::Result;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

#[macro_use]
extern crate lazy_static;

use futures::future::{FutureExt, LocalBoxFuture};
use crossbeam_channel::Sender;
use bytes::Bytes;

use pi_async::rt::serial_local_thread::LocalTaskRuntime;

pub mod connect;
pub mod terminal;
// pub mod client;
pub mod utils;

use crate::{connect::BlockingUdpSocket,
            utils::{UdpMultiInterface, UdpSocketStatus, SocketContext}};

///
/// Udp连接
///
pub trait Socket: Send + Sync + 'static {
    /// 线程安全的判断是否已关闭Udp连接
    fn is_closed(&self) -> bool;

    /// 获取连接唯一id
    fn get_uid(&self) -> usize;

    /// 获取连接本地地址
    fn get_local(&self) -> &SocketAddr;

    /// 获取连接远端地址
    fn get_remote(&self) -> Option<&SocketAddr>;

    /// 获取连接状态
    fn get_status(&self) -> UdpSocketStatus;

    /// 设置非多播报文的生存周期
    fn set_ttl(&self, ttl: u32) -> Result<()>;

    /// 加入指定的多播组
    /// 指定多播报文的生存周期，默认为1
    /// 指定多播消息是否会发回源地址，
    fn join_multicast_group(&self,
                            address: SocketAddr,
                            interface: UdpMultiInterface,
                            ttl: u32,
                            is_loop: bool) -> Result<()>;

    /// 离开指定的多播组
    fn leave_multicast_group(&self,
                             address: SocketAddr,
                             interface: UdpMultiInterface) -> Result<()>;

    /// 允许当前连接在局域网内广播
    fn enable_broadcast(&self) -> Result<()>;

    /// 禁止当前连接在局域网内广播
    fn disable_boradcast(&self) -> Result<()>;

    /// 获取连接上下文只读引用
    fn get_context(&self) -> &SocketContext;

    /// 获取读取的块大小
    fn get_read_block_len(&self) -> usize;

    /// 设置读取的块大小
    fn set_read_block_len(&self, len: usize);

    /// 获取写入的块大小
    fn get_write_block_len(&self) -> usize;

    /// 设置写入的块大小
    fn set_write_block_len(&self, len: usize);

    /// 向连接所在运行时派发指定的异步任务
    fn spawn(&self, task: LocalBoxFuture<'static, TaskResult>);

    /// 连接指定的对端地址
    fn connect(&self, to: SocketAddr) -> Result<()>;

    /// 写指定数据到连接缓冲区
    fn write(&self,
             buf: Vec<u8>,
             peer: Option<SocketAddr>) -> Result<()>;

    /// 接收流中的数据，返回成功，则表示本次接收了需要的字节数，并返回本次接收的字节数，否则返回接收错误
    fn recv(&self) -> Result<(Vec<u8>, Option<SocketAddr>)>;

    /// 发送数据到流
    fn send(&self,
            buf: Bytes,
            peer: Option<SocketAddr>) -> Result<usize>;

    /// 关闭Udp连接
    fn close(&self, reason: Result<()>) -> Result<()>;
}

///
/// Udp连接句柄
///
pub type SocketHandle = Arc<dyn Socket>;

///
/// Udp连接句柄引用
///
pub type SocketHandleRef<'a> = &'a dyn Socket;

///
/// Udp连接异步服务
///
pub trait AsyncService: Send + Sync + 'static {
    /// 绑定异步服务所在的运行时
    fn bind_runtime(&mut self, rt: LocalTaskRuntime<()>);

    /// 异步处理已绑定本地端口的Udp连接
    fn handle_binded(&self,
                     handle: SocketHandle,
                     result: Result<()>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已接收
    fn handle_received(&self,
                       handle: SocketHandle,
                       result: Result<(Vec<u8>, Option<SocketAddr>)>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已发送
    fn handle_sended(&self,
                     handle: SocketHandle,
                     result: Result<usize>) -> LocalBoxFuture<'static, ()>;

    /// 异步处理已关闭
    fn handle_closed(&self,
                     handle: SocketHandle,
                     result: Result<()>) -> LocalBoxFuture<'static, ()>;
}

///
/// Udp事件
///
pub enum UdpEvent {
    Recv(usize, Vec<u8>, Option<SocketAddr>),   //接收事件
    Sent(usize, Vec<u8>, Option<SocketAddr>),   //发送事件
    Task(LocalBoxFuture<'static, TaskResult>),  //任务事件
    CloseConnection(usize, Result<()>),         //关闭连接事件
    CloseListener(Result<()>),                  //关闭监听器事件
}

unsafe impl Send for UdpEvent {}

///
/// Udp任务结果
///
#[derive(Debug, Clone)]
pub enum TaskResult {
    Continue,   //继续
    ExitReady,  //退出事件处理循环已就绪
}
