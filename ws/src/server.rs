use std::mem;
use std::sync::Arc;
use std::cell::RefCell;
use std::net::Shutdown;
use std::str::from_utf8;
use std::marker::PhantomData;
use std::collections::HashMap;
use std::result::Result as GenResult;
use std::io::{ErrorKind, Result, Error};

use fnv::FnvBuildHasher;
use http::Response;
use httparse::{EMPTY_HEADER, Request};
use futures::future::{FutureExt, LocalBoxFuture};

use tcp::{Socket, AsyncService, SocketStatus, SocketHandle,
          utils::SocketContext};

use crate::{acceptor::{MAX_HANDSHAKE_HTTP_HEADER_LIMIT, WsAcceptor},
            connect::WsSocket,
            frame::{WsHead, WsFrame},
            utils::{ChildProtocol, WsStatus, WsSession}};

///
/// Websocket连接监听器
///
pub struct WebsocketListener<S: Socket> {
    acceptor:   WsAcceptor<S>,               //连接接受器
    protocol:   Arc<dyn ChildProtocol<S>>,   //连接监听器支持的子协议
}

impl<S: Socket> AsyncService<S> for WebsocketListener<S> {
    fn handle_connected(&self,
                        handle: SocketHandle<S>,
                        status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async move {
            if let SocketStatus::Connected(Err(e)) = status {
                //Tcp连接失败
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket connect failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    e))));
                return;
            }
        }.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        if let SocketStatus::Readed(Err(e)) = status {
            //Tcp读数据失败
            return async move {
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket read failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    e))));
            }.boxed_local();
        }

        if unsafe { (&*handle.get_context().get()).is_empty() } {
            //当前Websocket连接还未握手，则开始Websocket连接握手
            let support_protocol = self.protocol.clone();
            let acceptor = self.acceptor.clone();

            async move {
                WsAcceptor::<S>::accept(handle.clone(),
                                        acceptor,
                                        support_protocol).await;
            }.boxed_local()
        } else {
            //当前Websocket连接已完成握手
            let window_bits = self.acceptor.window_bits();
            let protocol = self.protocol.clone();
            async move {
                WsSocket::<S>::handle_readed(&handle,
                                             window_bits,
                                             protocol).await;
            }.boxed_local()
        }
    }

    fn handle_writed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async move {
            if let SocketStatus::Writed(Err(e)) = status {
                //Tcp写数据失败
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket write failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    e))));
                return;
            }

            WsSocket::handle_writed(handle).await;
        }.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let window_bits = self.acceptor.window_bits();
        let protocol = self.protocol.clone();

        async move {
            if let SocketStatus::Closed(result) = status {
                WsSocket::handle_closed(handle,
                                        window_bits,
                                        protocol,
                                        result).await;
            }
        }.boxed_local()
    }

    fn handle_timeouted(&self,
                        handle: SocketHandle<S>,
                        status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let window_bits = self.acceptor.window_bits();
        let protocol = self.protocol.clone();

        async move {
            if let SocketStatus::Timeout(event) = status {
                WsSocket::handle_timeouted(handle,
                                           window_bits,
                                           protocol,
                                           event).await;
            }
        }.boxed_local()
    }
}

impl<S: Socket> WebsocketListener<S> {
    /// 构建指定子协议的Websocket连接监听器
    pub fn with_protocol(protocol: Arc<dyn ChildProtocol<S>>) -> Self {
        WebsocketListener {
            acceptor: WsAcceptor::default(),
            protocol: protocol,
        }
    }
}