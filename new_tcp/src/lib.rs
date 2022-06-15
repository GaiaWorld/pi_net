#![allow(warnings)]
use std::future::Future;
use std::io::{Error, Result, ErrorKind};

use futures::future::BoxFuture;

#[macro_use]
extern crate lazy_static;

pub mod server;

///
/// Tcp连接适配器工厂
///
pub trait SocketAdapterFactory {
    type Connect: Socket;
    type Adapter: SocketAdapter<Connect = Self::Connect>;

    //获取Tcp连接适配器实例
    fn get_instance(&self) -> Self::Adapter;
}

///
/// Tcp连接状态
///
#[derive(Debug)]
pub enum SocketStatus {
    Connected(Result<()>),  //已连接
    Readed(Result<()>),     //已读
    Writed(Result<()>),     //已写
    Closed(Result<()>),     //已关闭
    Timeout(SocketEvent),   //已超时
}

///
/// Tcp连接的事件
///
#[derive(Debug)]
pub struct SocketEvent {
    inner: *mut (), //内部事件
}

unsafe impl Send for SocketEvent {}

///
/// Tcp连接异步服务
///
pub trait AsyncService<S: Socket>: 'static {
    //异步处理已连接
    fn handle_connected(&self, handle: SocketHandle<S>, status: SocketStatus) -> BoxFuture<'static, ()>;

    //异步处理已读
    fn handle_readed(&self, handle: SocketHandle<S>, status: SocketStatus) -> BoxFuture<'static, ()>;

    //异步处理已写
    fn handle_writed(&self, handle: SocketHandle<S>, status: SocketStatus) -> BoxFuture<'static, ()>;

    //异步处理已关闭
    fn handle_closed(&self, handle: SocketHandle<S>, status: SocketStatus) -> BoxFuture<'static, ()>;

    //异步处理已超时
    fn handle_timeouted(&self, handle: SocketHandle<S>, status: SocketStatus) -> BoxFuture<'static, ()>;
}