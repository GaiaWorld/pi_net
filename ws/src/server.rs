use std::mem;
use std::sync::Arc;
use std::cell::RefCell;
use std::net::Shutdown;
use std::str::from_utf8;
use std::collections::HashMap;
use std::result::Result as GenResult;
use std::io::{ErrorKind, Result, Error};

use fnv::FnvBuildHasher;
use http::{HttpTryFrom, Response};
use httparse::{EMPTY_HEADER, Request};
use futures::future::{FutureExt, BoxFuture};

use tcp::util::{IoBytes, SocketContext};
use tcp::driver::{Socket, AsyncIOWait, AsyncServiceName, AsyncService, SocketStatus, SocketHandle, AsyncReadTask, AsyncWriteTask};

use crate::{acceptor::{MAX_HANDSHAKE_HTTP_HEADER_LIMIT, WsAcceptor},
            connect::WsSocket,
            frame::{WsHead, WsFrame},
            util::{ChildProtocol, WsStatus, WsContext}};
use futures::Future;

/*
* Websocket连接监听器
*/
pub struct WebsocketListener<S: Socket, H: AsyncIOWait> {
    acceptor:   WsAcceptor<S, H>,                       //连接接受器
    protocol:   Option<Arc<dyn ChildProtocol<S, H>>>,   //连接监听器支持的子协议
}

impl<S: Socket, H: AsyncIOWait> Default for WebsocketListener<S, H> {
    fn default() -> Self {
        WebsocketListener {
            acceptor: WsAcceptor::default(),
            protocol: None,
        }
    }
}

impl<S: Socket, H: AsyncIOWait> AsyncServiceName for WebsocketListener<S, H> {
    fn service_name() -> String {
        "WebsocketListener".to_string()
    }
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
        let future = async move {
            if let SocketStatus::Closed(result) = status {
                WsSocket::handle_closed(handle, waits, result).await;
            }
        };
        future.boxed()
    }
}
