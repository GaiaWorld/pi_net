use std::error;
use std::sync::Arc;
use std::net::Shutdown;
use std::str::from_utf8;
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};
use std::fmt::{Display, Formatter, Result as FmtResult, Debug};

use bytes::BufMut;
use httparse::{EMPTY_HEADER, Request};
use http::{Result as HttpResult,
           Response,
           Version,
           status::StatusCode,
           header::{HOST, CONNECTION, UPGRADE,
                    ORIGIN, USER_AGENT,
                    CONTENT_TYPE, CONTENT_LENGTH,
                    SEC_WEBSOCKET_VERSION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_EXTENSIONS,
                    SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_ACCEPT, HeaderValue}};
use base64;
use log::warn;

use atom::Atom;
use pi_crypto::digest::{DigestAlgorithm, digest};

use tcp::driver::{Socket, AsyncIOWait, SocketHandle, AsyncReadTask, AsyncWriteTask};

use crate::util::{ChildProtocol, WsSession};

/*
* Websocket握手请求的响应序列化缓冲长度
*/
const HANDSHAKE_RESP_BUFFER_SIZE: usize = 256;

/*
* 支持的Websocket握手请求时必须的Http头数量
*/
const HANDSHAKE_REQUEST_HEADER_COUNT: u8 = 5;

/*
* 支持的Websocket扩展协议，每消息Deflate压缩，并在握手返回时确定客户端的压缩窗口大小
*/
const WEBSOCKET_EXTENSIONS: &str = "permessage-deflate; client_max_window_bits";

/*
* 支持的Websocket协议版本号
*/
const WEBSOCKET_PROTOCOL_VERSION: u8 = 13;
const WEBSOCKET_PROTOCOL_VERSION_STR: &str = "13";

/*
* Websocket服务器端Guid
*/
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/*
* Websocket握手时允许的最大Http头数量
*/
pub const MAX_HANDSHAKE_HTTP_HEADER_LIMIT: usize = 16;

/*
* 支持的Websocket连接升级
*/
pub const CONNECT_UPGRADE: &str = "websocket";

/*
* Websocket握手状态
*/
#[derive(Debug, Clone)]
pub enum Status {
    Failed(u8),                     //握手失败
    Succeeded(u8, String, String),  //握手成功，服务器端指定的客户端压缩窗口大小、客户端需要的子协议和服务器端指定的密钥
}

/*
* Websocket连接接受器
*/
pub struct WsAcceptor<S: Socket, H: AsyncIOWait> {
    window_bits:    u8,                     //客户端的压缩窗口大小，等于0表示，不支持每消息Deflate压缩的扩展协议
    marker:         PhantomData<(S, H)>,
}

unsafe impl<S: Socket, H: AsyncIOWait> Send for WsAcceptor<S, H> {}
unsafe impl<S: Socket, H: AsyncIOWait> Sync for WsAcceptor<S, H> {}

impl<S: Socket, H: AsyncIOWait> Clone for WsAcceptor<S, H> {
    fn clone(&self) -> Self {
        WsAcceptor {
            window_bits: self.window_bits,
            marker: PhantomData,
        }
    }
}

impl<S: Socket, H: AsyncIOWait> Default for WsAcceptor<S, H> {
    //默认构建准备握手的连接接受器
    fn default() -> Self {
        WsAcceptor {
            window_bits: 0, //默认不支持每消息Deflate压缩的扩展协议
            marker: PhantomData,
        }
    }
}

/*
* Websocket接受器同步方法
*/
impl<S: Socket, H: AsyncIOWait> WsAcceptor<S, H> {
    //构建指定客户端的压缩窗口大小的连接接受器
    pub fn with_window_bits(window_bits: u8) -> Self {
        let mut handler = WsAcceptor::default();
        handler.window_bits = window_bits;
        handler
    }

    //获取连接接受器指定的压缩窗口大小
    pub fn window_bits(&self) -> u8 {
        self.window_bits
    }

    //握手，返回握手是否成功、握手请求的响应和子协议
    pub fn handshake(&self,
                     handle: SocketHandle<S>,
                     protocol: &Arc<dyn ChildProtocol<S, H>>,
                     mut req: Request) -> (bool, HttpResult<Response<Vec<u8>>>) {
        match check_handshake_request(&mut req, self.window_bits) {
            Err(e) => {
                //非标准握手请求
                match protocol.non_standard_handshake_protocol(&req) {
                    Err(err) => {
                        //处理非标准握手请求失败
                        warn!("!!!> Ws Check Handshake Failed, check failed reason: {:?}, non-standard check failed reason: {:?}", e, err);
                        let resp = reply_non_standard_handshake(Err(StatusCode::BAD_REQUEST));
                        (resp.is_ok(), resp)
                    },
                    Ok(successed) => {
                        //处理非标准握手请求成功
                        let resp = reply_non_standard_handshake(Ok(successed));
                        (resp.is_ok(), resp)
                    },
                }
            },
            Ok(success) => {
                match success {
                    Status::Succeeded(ws_ext, mut ws_protocol, ws_accept) => {
                        //握手请求成功，则更新握手状态，并返回握手请求的响应
                        let protocol_name: Option<&str> = None;
                        ws_protocol = ws_protocol.trim().to_string();
                        let protocols: Vec<&str> = ws_protocol.split(";").collect();
                        let protocols_len = protocols.len();

                        //匹配支持的任何一个子协议
                        for p in &protocols {
                            //将客户端需要的子协议名转换为全小写，并与服务器端支持的子协议进行对比
                            if protocols_len == 1 && (*p) == "" {
                                //客户端没有指定子协议
                                let resp = if let Err(e) = protocol.handshake_protocol(handle, &req) {
                                    //子协议处理握手失败，则立即中止握手
                                    warn!("!!!> Ws Handshake Failed, reason: {:?}", e);
                                    reply_handshake(Err(StatusCode::BAD_REQUEST))
                                } else {
                                    //子协议处理握手成功
                                    reply_handshake(Ok((ws_ext, None, ws_accept.as_str())))
                                };
                                return (resp.is_ok(), resp);
                            } else if protocol.protocol_name() == (*p).to_lowercase().as_str() {
                                //客户端需要的子协议中有服务器端支持的子协议，则握手成功，将客户端需要，且服务器端支持的子协议名原样返回
                                let resp = if let Err(e) = protocol.handshake_protocol(handle, &req) {
                                    //子协议处理握手失败，则立即中止握手
                                    warn!("!!!> Ws Handshake Failed, reason: {:?}", e);
                                    reply_handshake(Err(StatusCode::BAD_REQUEST))
                                } else {
                                    //子协议处理握手成功
                                    reply_handshake(Ok((ws_ext, Some((*p)), ws_accept.as_str())))
                                };
                                return (resp.is_ok(), resp);
                            }
                        }

                        //客户端指定了需要的子协议，且服务器端不支持客户端需要的任何子协议，则握手失败
                        warn!("!!!> Ws Handshake Failed, reason: may not support client protocol, protocols: {:?}", protocols);
                        let resp = reply_handshake(Err(StatusCode::BAD_REQUEST));
                        (resp.is_ok(), resp)
                    },
                    _ => {
                        //握手请求冲突
                        warn!("!!!> Ws Handshake Failed, reason: invalid status");
                        let resp = reply_handshake(Err(StatusCode::BAD_REQUEST));
                        (resp.is_ok(), resp)
                    },
                }
            },
        }
    }
}

/*
* Websocket接受器异步方法
*/
impl<S: Socket, H: AsyncIOWait> WsAcceptor<S, H> {
    //异步接受握手请求
    pub async fn accept<'h, 'b>(handle: SocketHandle<S>,
                                waits: H,
                                acceptor: WsAcceptor<S, H>,
                                support_protocol: Arc<dyn ChildProtocol<S, H>>) {
        let mut headers = [EMPTY_HEADER; MAX_HANDSHAKE_HTTP_HEADER_LIMIT];
        let mut req = Request::new(&mut headers);

        loop {
            match AsyncReadTask::async_read(handle.clone(), waits.clone(), 0).await {
                Err(e) => {
                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handshake by read failed, reason: {:?}", e))));
                    return;
                },
                Ok(bin) => {
                    match req.parse(bin) {
                        Err(e) => {
                            //解析握手时的Http头错误
                            handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handshake by http parse failed, reason: {:?}", e))));
                            return;
                        },
                        Ok(ref status) if status.is_partial() => {
                            //部分握手数据已到达
                            match req.version {
                                Some(ver) if ver != 1 => {
                                    //不合法的Http版本号
                                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handshake by http parse failed, version: {}, reason: invalid http version", ver))));
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

        WsAcceptor::<S, H>::handle_handshake(handle, waits, acceptor, req, support_protocol).await;
    }

    //异步处理握手请求
    async fn handle_handshake<'h, 'b>(handle: SocketHandle<S>,
                                      waits: H,
                                      acceptor: WsAcceptor<S, H>,
                                      req: Request<'h, 'b>,
                                      support_protocol: Arc<dyn ChildProtocol<S, H>>) {
        handle.get_context_mut().set(WsSession::default()); //握手前绑定Tcp连接上下文
        match acceptor.handshake(handle.clone(), &support_protocol, req) {
            (_, Err(e)) => {
                //握手异常
                handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handshake failed, reason: {:?}", e))));
            },
            (_, Ok(resp)) => {
                //握手请求已完成，则返回
                let mut buf = handle.alloc().ok().unwrap().unwrap();
                buf.get_iolist_mut().push_back(resp_to_vec(resp).into());

                if let Some(buf_handle) = buf.finish() {
                    if let Err(e) = AsyncWriteTask::async_write(handle.clone(), waits, buf_handle).await {
                        handle.close(Err(Error::new(ErrorKind::Other, format!("webSocket handshake write error, reason: {:?}", e))));
                    }
                }
            },
        }
    }
}

//检查握手请求是否合法
fn check_handshake_request(req: &mut Request, window_bits: u8) -> Result<Status> {
    let mut count = HANDSHAKE_REQUEST_HEADER_COUNT;
    let mut ws_key = String::default();
    let mut ws_ext = 0;
    let mut ws_protocol = String::default();

    for header in req.headers.iter() {
        match header.name.to_lowercase().as_str() {
            key if key == HOST.as_str() => {
                if let Err(e) = unmatch_header_value(key, header.value) {
                    return Err(e);
                }

                //检查主机成功，则保存
                count -= 1;
            },
            key if key == CONNECTION.as_str() => {
                if let Err(e) = match_header_value(key, header.value, UPGRADE.as_str()) {
                    return Err(e);
                }

                //检查连接升级成功
                count -= 1;
            },
            key if key == UPGRADE.as_str() => {
                if let Err(e) = match_header_value(key, header.value, CONNECT_UPGRADE) {
                    return Err(e);
                }

                //检查升级协议成功
                count -= 1;
            },
            key if key == SEC_WEBSOCKET_VERSION.as_str() => {
                if let Err(e) = match_header_value(key, header.value, WEBSOCKET_PROTOCOL_VERSION_STR) {
                    return Err(e);
                }

                //检查Websocket协议版本成功
                count -= 1;
            },
            key if key == SEC_WEBSOCKET_KEY.as_str() => {
                match unmatch_header_value(key, header.value) {
                    Err(e) => {
                        return Err(e);
                    },
                    Ok(r) => {
                        //检查Websocket密钥成功，则保存
                        count -= 1;
                        ws_key = r;
                    },
                }
            },
            key if key == SEC_WEBSOCKET_EXTENSIONS.as_str() => {
                //握手请求中有指定Websocket扩展协议
                if let Ok(_) = match_header_value(key, header.value, WEBSOCKET_EXTENSIONS) {
                    //已匹配唯一支持的扩展协议
                    if window_bits > 0 {
                        //当前支持每消息Deflate压缩的扩展协议，则保存
                        ws_ext = window_bits;
                    }
                }
            },
            key if key == SEC_WEBSOCKET_PROTOCOL.as_str() => {
                //握手请求中有指定Websocket子协议
                if let Ok(r) = unmatch_header_value(key, header.value) {
                    //已匹配子协议，则保存
                    ws_protocol = r;
                }
            },
            _ => (), //忽略其它Http头
        }
    }

    if count > 0 {
        //握手请求没有完整的Http头
        Err(Error::new(ErrorKind::Other, format!("invalid handshake header")))
    } else {
        //握手请求检查成功
        Ok(Status::Succeeded(ws_ext, ws_protocol, accept(ws_key)))
    }
}

//检查Http头的值是否和指定值匹配，返回匹配成功的值
fn match_header_value(key: &str, value: &[u8], rhs: &str) -> Result<String> {
    match from_utf8(value) {
        Err(e) => {
            Err(Error::new(ErrorKind::Other, format!("key: {}, reason: {}", key, e)))
        },
        Ok(val) => {
            let lower = val.to_lowercase();
            if lower.as_str() == rhs {
                Ok(lower)
            } else {
                Err(Error::new(ErrorKind::Other, format!("match failed, key: {}, val: {}, reason: invalid connect header", key, val)))
            }
        },
    }
}

//检查Http头的值是否不为空，返回匹配成功的值
fn unmatch_header_value(key: &str, value: &[u8]) -> Result<String> {
    match from_utf8(value) {
        Err(e) => {
            Err(Error::new(ErrorKind::Other, format!("key: {}, reason: {}", key, e)))
        },
        Ok(val) => {
            if val != "" {
                Ok(val.to_string())
            } else {
                Err(Error::new(ErrorKind::Other, format!("unmatch failed, key: {}, val: {}, reason: invalid connect header", key, val)))
            }
        },
    }
}

//创建握手请求响应
fn reply_handshake<'a>(result: GenResult<(u8, Option<&'a str>, &'a str), StatusCode>) -> HttpResult<Response<Vec<u8>>> {
    match result {
        Err(code) => {
            //握手失败，返回指定状态码的响应
            Response::builder()
                .status(code)
                .version(Version::HTTP_11)
                .body(vec![])
        },
        Ok((window_bits, p, accept)) => {
            //握手成功
            let mut resp = Response::builder();

            //初始化响应
            resp.status(StatusCode::SWITCHING_PROTOCOLS)
                .version(Version::HTTP_11)
                .header(CONNECTION, UPGRADE)
                .header(UPGRADE, CONNECT_UPGRADE);

            if window_bits > 0 {
                //服务器端支持客户端的压缩扩展协议，则设置服务端指定的客户端压缩窗口大小
                resp.header(SEC_WEBSOCKET_EXTENSIONS, WEBSOCKET_EXTENSIONS.to_string() + "=" + &window_bits.to_string());
            }

            if let Some(protocol) = p {
                //服务器端支持客户端需要的子协议，则设置服务端指定的子协议
                resp.header(SEC_WEBSOCKET_PROTOCOL, protocol);
            }

            //设置服务端指定的密钥，并返回握手成功响应
            resp.header(SEC_WEBSOCKET_ACCEPT, accept)
                .body(vec![])
        },
    }
}

//创建非标准握手请求响应
fn reply_non_standard_handshake(result: GenResult<(String, Vec<u8>), StatusCode>) -> HttpResult<Response<Vec<u8>>> {
    match result {
        Err(code) => {
            //处理非标准握手失败，返回指定状态码的响应
            Response::builder()
                .status(code)
                .version(Version::HTTP_11)
                .body(vec![])
        },
        Ok((mime, body)) => {
            //处理非标准握手成功
            let mut resp = Response::builder();

            //初始化响应
            resp.status(StatusCode::OK)
                .version(Version::HTTP_11)
                .header(CONTENT_TYPE, HeaderValue::from_str(mime.as_str()).unwrap())
                .header(CONTENT_LENGTH, body.len());

            //设置服务端指定的密钥，并返回握手成功响应
            resp.body(body)
        },
    }
}

//计算握手请求相应的服务器端密钥
fn accept(key: String) -> String {
    let bin = digest(DigestAlgorithm::SHA1, (key + WEBSOCKET_GUID).as_bytes());
    base64::encode(&bin)
}

//将握手请求的响应序列化为Vec<u8>
fn resp_to_vec(resp: Response<Vec<u8>>) -> Vec<u8> {
    let body_len = resp.body().len();
    let (mut buf, body) = if body_len > 0 {
        //非标准握手请求成功
        (Vec::with_capacity(HANDSHAKE_RESP_BUFFER_SIZE + body_len),
        resp.body().clone())
    } else {
        //握手成功
        (Vec::with_capacity(HANDSHAKE_RESP_BUFFER_SIZE), vec![])
    };

    buf.put(format!("{:?}", resp.version()).as_bytes());
    buf.put(" ".as_bytes());
    let status = resp.status();
    buf.put(status.as_str().as_bytes());
    buf.put(" ".as_bytes());
    buf.put(status.canonical_reason().unwrap().as_bytes());
    buf.put("\r\n".as_bytes());
    for (key, value) in resp.headers() {
        buf.put(key.as_str().as_bytes());
        buf.put(":".as_bytes());
        buf.put(value.as_bytes());
        buf.put("\r\n".as_bytes());
    }
    buf.put("\r\n".as_bytes());
    buf.put(body.as_slice());

    buf
}