use std::net::Shutdown;
use std::rc::{Weak, Rc};
use std::str::from_utf8;
use std::collections::HashMap;

use bytes::BufMut;
use fnv::FnvBuildHasher;
use http::{HttpTryFrom, Response};
use httparse::{EMPTY_HEADER, Request};
use futures::future::{FutureExt, BoxFuture};

use tcp::util::IoBytes;
use tcp::driver::{Socket, AsyncIOWait, AsyncServiceName, AsyncService, SocketStatus, SocketHandle, AsyncReadTask, AsyncWriteTask};

use crate::{ChildPorotocl, acceptor::{MAX_HANDSHAKE_HTTP_HEADER_LIMIT, WsAcceptor}};

/*
* Websocket握手请求的响应序列化缓冲长度
*/
const HANDSHAKE_RESP_BUFFER_SIZE: usize = 256;

/*
* Websocket子协议组，表示Websocket支持的所有子协议，但一个连接只允许最多支持一个子协议
*/
pub struct ChildProtocolGroup {
    protocols: HashMap<String, Rc<Box<dyn ChildPorotocl>>, FnvBuildHasher>, //子协议表
}

impl ChildProtocolGroup {
    //构建一个Websocket子协议组
    pub fn new() -> Self {
        ChildProtocolGroup {
            protocols: HashMap::with_hasher(FnvBuildHasher::default()),
        }
    }

    //判断是否支持指定名称的子协议
    pub fn is_support_by_name(&self, name: &str) -> bool {
        self.protocols.contains_key(name)
    }

    //获取指定名称的子协议
    pub fn get_by_name(&self, name: &str) -> Option<Weak<Box<dyn ChildPorotocl>>> {
        if let Some(protocol) = self.protocols.get(name) {
            return Some(Rc::downgrade(protocol));
        }

        None
    }

    //设置支持的子协议
    pub fn set<P: ChildPorotocl>(&mut self, protocol: P) {
        self.protocols.insert(protocol.protocol_name().to_string(), Rc::new(Box::new(protocol)));
    }
}

/*
* Websocket连接监听器
*/
pub struct WebsocketListener {
    group:      ChildProtocolGroup, //子协议组
    acceptor:   WsAcceptor,         //连接接受器
}

impl Default for WebsocketListener {
    fn default() -> Self {
        WebsocketListener {
            group: ChildProtocolGroup::new(), //TODO 在Mqtt模块里为子协议组实现一个支持Mqtt子协议的Default
            acceptor: WsAcceptor::default(),
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
        let acceptor = self.acceptor.clone();
        let future = async move {
            if let SocketStatus::Connected(Err(e)) = status {
                println!("!!!> Websocket Connect Failed, reason: {:?}", e);
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                return;
            }

            let mut headers = [EMPTY_HEADER; MAX_HANDSHAKE_HTTP_HEADER_LIMIT];
            let mut req = Request::new(&mut headers);
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
                                //解析握手时的http头错误
                                println!("!!!> Websocket Handshake by Http Parse Failed, reason: {:?}", e);
                                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                                return;
                            },
                            Ok(ref status) if status.is_partial() => {
                                //部分握手数据已到达
                                match req.version {
                                    Some(ver) if ver != 1 => {
                                        //不合法的版本号
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

            match acceptor.handshake(req) {
                Err(e) => {
                    //握手异常
                    println!("!!!> Websocket Handshake Failed, reason: {:?}", e);
                    handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                },
                Ok(resp) => {
                    //握手失败或成功，则返回
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
        };
        future.boxed()
    }

    fn handle_readed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            return;
        };
        future.boxed()
    }

    fn handle_writed(&self, handle: SocketHandle<S>, waits: H, status: SocketStatus) -> Self::Future {
        let future = async move {
            return;
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