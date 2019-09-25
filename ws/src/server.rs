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
use http::{HttpTryFrom, Response};
use httparse::{EMPTY_HEADER, Request};
use futures::future::{FutureExt, BoxFuture};

use tcp::{server::AsyncWaitsHandle,
          driver::{Socket, AsyncIOWait,
                  AsyncService, AsyncServiceFactory, SocketStatus,
                  SocketHandle, AsyncReadTask, AsyncWriteTask},
          util::{IoBytes, SocketContext}};

use crate::{acceptor::{MAX_HANDSHAKE_HTTP_HEADER_LIMIT, WsAcceptor},
            connect::WsSocket,
            frame::{WsHead, WsFrame},
            util::{ChildProtocol, ChildProtocolFactory, WsStatus, WsSession}};
use futures::Future;

/*
* Websocket连接监听器
*/
pub struct WebsocketListener<S: Socket, H: AsyncIOWait> {
    acceptor:   WsAcceptor<S, H>,               //连接接受器
    protocol:   Arc<dyn ChildProtocol<S, H>>,   //连接监听器支持的子协议
}

impl<S: Socket, H: AsyncIOWait> AsyncService<S, H> for WebsocketListener<S, H> {
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn handle_connected(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let support_protocol = self.protocol.clone();
        let acceptor = self.acceptor.clone();

        let future = async move {
            if let SocketStatus::Connected(Err(e)) = status {
                //Tcp连接失败
                handle.close(Err(Error::new(ErrorKind::Other, format!("websocket connect failed, reason: {:?}", e))));
                return;
            }

            WsAcceptor::<S, H>::accept(handle.clone(), waits.clone(), acceptor, support_protocol).await;
        };
        future.boxed()
    }

    fn handle_readed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let window_bits = self.acceptor.window_bits();
        let protocol = self.protocol.clone();

        let future = async move {
            if let SocketStatus::Readed(Err(e)) = status {
                //Tcp读数据失败
                handle.close(Err(Error::new(ErrorKind::Other, format!("websocket read failed, reason: {:?}", e))));
                return;
            }

            WsSocket::<S, H>::handle_readed(&handle, &waits, window_bits, protocol).await;
        };
        future.boxed()
    }

    fn handle_writed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Writed(Err(e)) = status {
                //Tcp写数据失败
                handle.close(Err(Error::new(ErrorKind::Other, format!("websocket write failed, reason: {:?}", e))));
                return;
            }

            WsSocket::handle_writed(handle, waits).await;
        };
        future.boxed()
    }

    fn handle_closed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let window_bits = self.acceptor.window_bits();
        let protocol = self.protocol.clone();

        let future = async move {
            if let SocketStatus::Closed(result) = status {
                WsSocket::handle_closed(handle, waits, window_bits, protocol, result).await;
            }
        };
        future.boxed()
    }

    fn handle_timeouted(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let window_bits = self.acceptor.window_bits();
        let protocol = self.protocol.clone();

        let future = async move {
            if let SocketStatus::Timeout(event) = status {
                WsSocket::handle_timeouted(handle, waits, window_bits, protocol, event).await;
            }
        };
        future.boxed()
    }
}

impl<S: Socket, H: AsyncIOWait> WebsocketListener<S, H> {
    //构建指定子协议的Websocket连接监听器
    pub fn with_protocol(protocol: Arc<dyn ChildProtocol<S, H>>) -> Self {
        WebsocketListener {
            acceptor: WsAcceptor::default(),
            protocol: protocol,
        }
    }
}

/*
* Websocket连接监听器工厂
*/
pub struct WebsocketListenerFactory<S: Socket> {
    protocol_factory: Arc<dyn ChildProtocolFactory<Connect = S, Waits = AsyncWaitsHandle>>,
}

impl<S: Socket> AsyncServiceFactory for WebsocketListenerFactory<S> {
    type Connect = S;
    type Waits = AsyncWaitsHandle;
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn new_service(&self) -> Box<dyn AsyncService<Self::Connect, Self::Waits, Out = Self::Out, Future = Self::Future>> {
        Box::new(
            WebsocketListener::with_protocol(
                self.protocol_factory.new_protocol()))
    }
}

impl<S: Socket> WebsocketListenerFactory<S> {
    //构建指定子协议工厂的Websocket连接监听器工厂
    pub fn with_protocol_factory(protocol_factory: Arc<dyn ChildProtocolFactory<Connect = S, Waits = AsyncWaitsHandle>>) -> Self {
        WebsocketListenerFactory {
            protocol_factory,
        }
    }
}
