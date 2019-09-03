use std::sync::Arc;
use std::io::Result;
use std::cell::RefCell;
use std::net::Shutdown;
use std::str::from_utf8;
use std::collections::HashMap;
use std::result::Result as GenResult;

use bytes::BufMut;
use fnv::FnvBuildHasher;
use http::{HttpTryFrom, Response};
use httparse::{EMPTY_HEADER, Request};
use futures::future::{FutureExt, BoxFuture};

use tcp::util::IoBytes;
use tcp::driver::{Socket, AsyncIOWait, AsyncServiceName, AsyncService, SocketStatus, SocketHandle, AsyncReadTask, AsyncWriteTask};

use crate::{acceptor::{MAX_HANDSHAKE_HTTP_HEADER_LIMIT, WsAcceptor},
            connect::{WsContext, WsSocket},
            frame::{WsHead, WsFrame},
            util::ChildProtocol};
use crate::connect::WsStatus;
use futures::Future;

/*
* Websocket握手请求的响应序列化缓冲长度
*/
const HANDSHAKE_RESP_BUFFER_SIZE: usize = 256;

/*
* Websocket连接表默认容量
*/
const WEBSOCKET_CONNECT_SIZE: usize = 0xffff;

/*
* Websocket连接监听器
*/
pub struct WebsocketListener {
    acceptor:   WsAcceptor,                     //连接接受器
    protocol:   Option<Arc<dyn ChildProtocol>>, //连接监听器支持的子协议
}

impl Default for WebsocketListener {
    fn default() -> Self {
        WebsocketListener {
            acceptor: WsAcceptor::default(),
            protocol: None,
        }
    }
}

impl AsyncServiceName for WebsocketListener {
    fn service_name() -> String {
        "WebsocketListener".to_string()
    }
}

impl<S: Socket, H: AsyncIOWait> AsyncService<S, H> for WebsocketListener {
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn handle_connected(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let support_protocol = self.protocol.clone();
        let acceptor = self.acceptor.clone();

        let future = async move {
            if let SocketStatus::Connected(Err(e)) = status {
                //Tcp连接失败
                println!("!!!> Websocket Connect Failed, reason: {:?}", e);
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                return;
            }

            let mut headers = [EMPTY_HEADER; MAX_HANDSHAKE_HTTP_HEADER_LIMIT];
            let mut req = Request::new(&mut headers);
            receive_handshake(&handle, &waits, &mut req).await;

            handle_handshake(handle, waits, acceptor, req, support_protocol).await;
        };
        future.boxed()
    }

    fn handle_readed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let window_bits = self.acceptor.window_bits();
        let protocol = self.protocol.clone();

        let future = async move {
            if let SocketStatus::Readed(Err(e)) = status {
                //Tcp读数据失败
                println!("!!!> Websocket Read Failed, reason: {:?}", e);
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                return;

            }

            WsSocket::<S, H>::read_head(&handle, &waits, window_bits, protocol).await;
        };
        future.boxed()
    }

    fn handle_writed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Writed(Err(e)) = status {
                //Tcp写数据失败
                println!("!!!> Websocket Write Failed, reason: {:?}", e);
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                return;
            }

            if let Some(mut context) = handle.as_handle().unwrap().as_ref().borrow_mut().get_context().get::<WsContext>() {
                if let Some(c) = Arc::get_mut(&mut context) {
                    //因为同时只有一个线程同步获取当前连接的上下文，所以可以获取到可写上下文
                    if c.is_handshaked() {
                        //当前连接已握手，则忽略写成功事件
                        return;
                    } else {
                        //当前连接正在握手，则修改当前连接状态为已握手，并立即释放可写上下文
                        c.set_status(WsStatus::HandShaked);
                    }
                } else {
                    //无法获取可写上下文，则表示有异常，立即关闭连接
                    println!("!!!> Websocket Write Failed, reason: invalid websocket context");
                    handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                    return;
                }
            } else {
                //连接上下文为空，则立即关闭连接
                println!("!!!> Websocket Write Failed, reason: websocket context empty");
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                return;
            }

            //握手完成，准备异步接收客户端发送的Websocket数据帧
            if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                //准备读失败，则立即关闭连接
                println!("!!!> Websocket Handshanke Ok, But Read Ready Error, reason: {:?}", e);
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
            }
        };
        future.boxed()
    }

    fn handle_closed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            return;
        };
        future.boxed()
    }
}

//异步接收握手请求
async fn receive_handshake<'h, 'b, S: Socket, H: AsyncIOWait>(handle: &SocketHandle<S>,
                                                              waits: &H,
                                                              req: &mut Request<'h, 'b>) {
    loop {
        match AsyncReadTask::async_read(handle.clone(), waits.clone(), 0).await {
            Err(e) => {
                println!("!!!> Websocket Handshake by Read Failed, reason: {:?}", e);
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                return;
            },
            Ok(bin) => {
                match req.parse(bin) {
                    Err(e) => {
                        //解析握手时的Http头错误
                        println!("!!!> Websocket Handshake by Http Parse Failed, reason: {:?}", e);
                        handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                        return;
                    },
                    Ok(ref status) if status.is_partial() => {
                        //部分握手数据已到达
                        match req.version {
                            Some(ver) if ver != 1 => {
                                //不合法的Http版本号
                                println!("!!!> Websocket Handshake by Http Parse Failed, version: {}, reason: invalid http version", ver);
                                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                                return;
                            },
                            _ => {
                                //握手数据不完整，继续读
                                continue;
                            }
                        }
                    },
                    Ok(status) => {
                        //全部握手数据已到达
                        break;
                    }
                }
            },
        }
    }
}

//异步处理握手请求
async fn handle_handshake<'h, 'b, S: Socket, H: AsyncIOWait>(handle: SocketHandle<S>,
                                                             waits: H,
                                                             acceptor: WsAcceptor,
                                                             req: Request<'h, 'b>,
                                                             support_protocol: Option<Arc<dyn ChildProtocol>>) {
    match acceptor.handshake(support_protocol, req) {
        (_, Err(e), _) => {
            //握手异常
            println!("!!!> Websocket Handshake Failed, reason: {:?}", e);
            handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
        },
        (is_ok, Ok(resp), protocol) => {
            //握手请求已完成，则返回
            if is_ok {
                //握手成功，则绑定连接上下文
                handle.as_handle().unwrap().as_ref().borrow_mut().get_context_mut().set(WsContext::default());
            }

            let mut buf = handle.as_handle().as_ref().unwrap().borrow().get_write_buffer().alloc().ok().unwrap().unwrap();
            buf.get_iolist_mut().push_back(resp_to_vec(resp).into());

            if let Some(buf_handle) = buf.finish() {
                if let Err(e) = AsyncWriteTask::async_write(handle.clone(), waits, buf_handle).await {
                    println!("!!!> WebSocket Handshake Write Error, reason: {:?}", e);
                    handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                }
            }
        },
    }
}

//将握手请求的响应序列化为Vec<u8>
fn resp_to_vec(resp: Response<()>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HANDSHAKE_RESP_BUFFER_SIZE);
    buf.put(format!("{:?}", resp.version()));
    buf.put(" ");
    let status = resp.status();
    buf.put(status.as_str());
    buf.put(" ");
    buf.put(status.canonical_reason().unwrap());
    buf.put("\r\n");
    for (key, value) in resp.headers() {
        buf.put(key.as_str());
        buf.put(":");
        buf.put(value.as_bytes());
        buf.put("\r\n");
    }
    buf.put("\r\n");

    buf
}