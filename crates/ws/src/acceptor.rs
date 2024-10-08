use std::error;
use std::sync::Arc;
use std::net::Shutdown;
use std::str::from_utf8;
use std::time::SystemTime;
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};
use std::fmt::{Display, Formatter, Result as FmtResult, Debug};

use bytes::{Buf, BufMut, BytesMut};
use httparse::{EMPTY_HEADER, Status as HttpHeadrStatus, Request};
use http::{Result as HttpResult,
           Response,
           Version,
           status::StatusCode,
           header::{HOST, CONNECTION, UPGRADE,
                    ORIGIN, USER_AGENT,
                    CONTENT_TYPE, CONTENT_LENGTH,
                    SEC_WEBSOCKET_VERSION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_EXTENSIONS,
                    SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_ACCEPT, HeaderValue}};
use pi_rand::xor_encrypt_confusion;
use base64;
use log::warn;

use pi_async_buffer::PartBuffer;
use pi_atom::Atom;
use pi_crypto::digest::{DigestAlgorithm, digest};

use tcp::{Socket, SocketHandle};
use crate::connect::WsSocket;
use crate::utils::{ChildProtocol, WsFrameType, WsSession};

///
/// Websocket握手请求的响应序列化缓冲长度
///
const HANDSHAKE_RESP_BUFFER_SIZE: usize = 256;

///
/// 支持的Websocket握手请求时必须的Http头数量
///
const HANDSHAKE_REQUEST_HEADER_COUNT: u8 = 5;

///
/// 支持的Websocket扩展协议，每消息Deflate压缩，并在握手返回时确定客户端的压缩窗口大小
///
const WEBSOCKET_EXTENSIONS: &str = "permessage-deflate; client_max_window_bits";

///
/// 支持的Websocket协议版本号
///
const WEBSOCKET_PROTOCOL_VERSION: u8 = 13;
const WEBSOCKET_PROTOCOL_VERSION_STR: &str = "13";

///
/// Websocket服务器端Guid
///
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

///
/// Websocket握手时允许的最大Http头数量
///
pub const MAX_HANDSHAKE_HTTP_HEADER_LIMIT: usize = 56;

///
/// 支持的Websocket连接升级
///
pub const CONNECT_UPGRADE: &str = "websocket";

// 用于加密随机数种子的密钥
// 注意如需修改，则必须同时修改客户端
const SAFE_SEED_KEY: u64 = 0xffabcdef0fedcba0;

///
/// Websocket握手状态
///
#[derive(Debug, Clone)]
pub enum Status {
    Failed(u8),                     //握手失败
    Succeeded(u8, String, String),  //握手成功，服务器端指定的客户端压缩窗口大小、客户端需要的子协议和服务器端指定的密钥
}

///
/// Websocket连接接受器
///
pub struct WsAcceptor<S: Socket> {
    window_bits:    u8,             //客户端的压缩窗口大小，等于0表示，不支持每消息Deflate压缩的扩展协议
    marker:         PhantomData<S>,
}

unsafe impl<S: Socket> Send for WsAcceptor<S> {}
unsafe impl<S: Socket> Sync for WsAcceptor<S> {}

impl<S: Socket> Clone for WsAcceptor<S> {
    fn clone(&self) -> Self {
        WsAcceptor {
            window_bits: self.window_bits,
            marker: PhantomData,
        }
    }
}

impl<S: Socket> Default for WsAcceptor<S> {
    // 默认构建准备握手的连接接受器
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
impl<S: Socket> WsAcceptor<S> {
    /// 构建指定客户端的压缩窗口大小的连接接受器
    pub fn with_window_bits(window_bits: u8) -> Self {
        let mut handler = WsAcceptor::default();
        handler.window_bits = window_bits;
        handler
    }

    /// 获取连接接受器指定的压缩窗口大小
    pub fn window_bits(&self) -> u8 {
        self.window_bits
    }

    /// 握手，返回握手是否成功、握手请求的响应和子协议
    pub fn handshake(&self,
                     handle: SocketHandle<S>,
                     protocol: &Arc<dyn ChildProtocol<S>>,
                     mut req: Request) -> (bool, HttpResult<Response<Vec<u8>>>) {
        match check_handshake_request(&mut req, self.window_bits) {
            Err(e) => {
                //非标准握手请求
                match protocol.non_standard_handshake_protocol(&req) {
                    Err(err) => {
                        //处理非标准握手请求失败
                        warn!("Ws Check Handshake Failed, token: {:?}, remote: {:?}, local: {:?}, check failed reason: {:?}, non-standard check failed reason: {:?}",
                            handle.get_token(),
                            handle.get_remote(),
                            handle.get_local(),
                            e,
                            err);
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
                                let resp = if let Err(e) = protocol.handshake_protocol(handle.clone(), &req, &protocols) {
                                    //子协议处理握手失败，则立即中止握手
                                    warn!("Ws Handshake Failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                        handle.get_token(),
                                        handle.get_remote(),
                                        handle.get_local(),
                                        e);
                                    reply_handshake(Err(StatusCode::BAD_REQUEST))
                                } else {
                                    //子协议处理握手成功
                                    reply_handshake(Ok((ws_ext, None, ws_accept.as_str())))
                                };
                                return (resp.is_ok(), resp);
                            } else if protocol.protocol_name() == (*p).to_lowercase().as_str() {
                                //客户端需要的子协议中有服务器端支持的子协议，则握手成功，将客户端需要，且服务器端支持的子协议名原样返回
                                let resp = if let Err(e) = protocol.handshake_protocol(handle.clone(), &req, &protocols) {
                                    //子协议处理握手失败，则立即中止握手
                                    warn!("Ws Handshake Failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                        handle.get_token(),
                                        handle.get_remote(),
                                        handle.get_local(),
                                        e);
                                    reply_handshake(Err(StatusCode::BAD_REQUEST))
                                } else {
                                    //子协议处理握手成功
                                    reply_handshake(Ok((ws_ext, Some((*p)), ws_accept.as_str())))
                                };
                                return (resp.is_ok(), resp);
                            }
                        }

                        //客户端指定了需要的子协议，且服务器端不支持客户端需要的任何子协议，则握手失败
                        warn!("Ws Handshake Failed, token: {:?}, remote: {:?}, local: {:?}, protocols: {:?}, reason: may not support client protocol",
                            handle.get_token(),
                            handle.get_remote(),
                            handle.get_local(),
                            protocols);
                        let resp = reply_handshake(Err(StatusCode::BAD_REQUEST));
                        (resp.is_ok(), resp)
                    },
                    _ => {
                        //握手请求冲突
                        warn!("Ws Handshake Failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid status",
                            handle.get_token(),
                            handle.get_remote(),
                            handle.get_local());
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
impl<S: Socket> WsAcceptor<S> {
    /// 异步接受握手请求
    pub async fn accept<'h, 'b>(handle: SocketHandle<S>,
                                window_bits: u8,
                                acceptor: WsAcceptor<S>,
                                support_protocol: Arc<dyn ChildProtocol<S>>) {
        let mut last_bin_len = 0; //初始化本地缓冲区上次长度
        let mut parse_count = 0; //初始化分析次数
        let mut buf: &[u8] = &[]; //初始化本地缓冲区
        loop {
            parse_count += 1; //更新分析次数
            if parse_count > 16 {
                //过多的分析次数，则立即返回错误原因
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket handshake by http parse failed, token: {:?}, remote: {:?}, local: {:?}, buf_len: {:?}, buf: {:?}, reason: out of parse",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    buf.len(),
                                                    buf))));
                return;
            }

            if let Some(bin) = unsafe { (&mut *handle.get_read_buffer().get()) } {
                let remaining = bin.remaining();
                if remaining == 0 {
                    //当前缓冲区还没有握手请求的数据，则异步准备读取后，继续尝试接收握手数据
                    if let Ok(value) = handle.read_ready(0) {
                        if value.await == 0 {
                            //当前连接已关闭，则立即退出
                            return;
                        }
                    }

                    continue;
                } else if remaining == last_bin_len {
                    //当前缓冲区的数据还没有更新，则异步准备读取后，继续尝试接收握手数据
                    if let Ok(value) = handle.read_ready(remaining + 1) {
                        let value = value.await;
                        if value == 0 {
                            //当前连接已关闭，则立即退出
                            return;
                        }
                    }

                    continue;
                } else {
                    //当前缓冲区有请求的数据或当前缓冲区的数据已更新，则更新本地缓冲区上次长度
                    last_bin_len = remaining;
                }
            } else {
                //Tcp读缓冲区不存在
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket handshake by http parse failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid read buffer",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local()))));
                return;
            }

            let mut headers = [EMPTY_HEADER; MAX_HANDSHAKE_HTTP_HEADER_LIMIT];
            let mut req = Request::new(&mut headers);

            buf = unsafe { (&*handle.get_read_buffer().get()).as_ref().unwrap().as_ref() }; //填充本地缓冲区
            match req.parse(buf) {
                Err(e) => {
                    //解析握手时的Http头错误
                    handle.close(Err(Error::new(ErrorKind::Other,
                                                format!("Websocket handshake by http parse failed, token: {:?}, remote: {:?}, local: {:?}, buf_len: {:?}, buf: {:?}, reason: {:?}",
                                                        handle.get_token(),
                                                        handle.get_remote(),
                                                        handle.get_local(),
                                                        buf.len(),
                                                        buf,
                                                        e))));
                    return;
                },
                Ok(ref status) if status.is_partial() => {
                    //部分握手数据已到达
                    match req.version {
                        Some(ver) if ver != 1 => {
                            //不合法的Http版本号
                            handle.close(Err(Error::new(ErrorKind::Other,
                                                        format!("Websocket handshake by http parse failed, token: {:?}, remote: {:?}, local: {:?}, version: {}, reason: invalid http version",
                                                                handle.get_token(),
                                                                handle.get_remote(),
                                                                handle.get_local(),
                                                                ver))));
                            return;
                        },
                        _ => {
                            //握手数据不完整，继续尝试接收握手数据
                            continue;
                        }
                    }
                },
                Ok(status) => {
                    //全部握手数据已到达
                    if let HttpHeadrStatus::Complete(len) = status {
                        let _ = unsafe {
                            (&mut *handle.get_read_buffer().get())
                                .as_mut()
                                .unwrap()
                                .copy_to_bytes(len) //消耗握手的请求数据
                        };
                    }
                    let ws_session = if support_protocol.is_strict() {
                        let seed = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_micros() as u64;
                        WsSession::with_seed(seed)
                    } else {
                        WsSession::default()
                    };
                    let seed = ws_session.get_seed();
                    unsafe { (&mut *handle.get_context().get()).set(ws_session); } //握手前绑定Tcp连接上下文
                    match acceptor.handshake(handle.clone(), &support_protocol, req) {
                        (_, Err(e)) => {
                            //握手异常
                            handle.close(Err(Error::new(ErrorKind::Other,
                                                        format!("Websocket handshake failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                                handle.get_token(),
                                                                handle.get_remote(),
                                                                handle.get_local(),
                                                                e))));
                        },
                        (_, Ok(resp)) => {
                            //握手请求已完成，则返回
                            if let Err(e) = handle.write_ready(resp_to_vec(resp)) {
                                handle.close(Err(Error::new(ErrorKind::Other,
                                                            format!("WebSocket handshake write error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                                    handle.get_token(),
                                                                    handle.get_remote(),
                                                                    handle.get_local(),
                                                                    e))));
                            }

                            if support_protocol.is_strict() {
                                //在握手回应后，向对端发送加密后的当前连接会话的随机数种子
                                let safe_seed_bytes = match xor_encrypt_confusion(seed.to_le_bytes(), SAFE_SEED_KEY.to_le_bytes()) {
                                    Err(e) => {
                                        handle.close(Err(Error::new(ErrorKind::Other,
                                                                    format!("WebSocket strict handshake error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                                            handle.get_token(),
                                                                            handle.get_remote(),
                                                                            handle.get_local(),
                                                                            e))));
                                        return;
                                    },
                                    Ok(safe_seed) => safe_seed,
                                };

                                let ws_connect = WsSocket::new(handle, window_bits);
                                if let Err(e) = ws_connect.send(WsFrameType::Binary, safe_seed_bytes) {
                                    ws_connect.close(Err(Error::new(ErrorKind::Other,
                                                                format!("WebSocket strict handshake write error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                                        ws_connect.get_token(),
                                                                        ws_connect.get_remote(),
                                                                        ws_connect.get_local(),
                                                                        e))));
                                    return;
                                }
                            }
                        },
                    }

                    return;
                }
            }
        }
    }
}

// 检查握手请求是否合法
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

// 检查Http头的值是否和指定值匹配，返回匹配成功的值
fn match_header_value(key: &str, value: &[u8], rhs: &str) -> Result<String> {
    match from_utf8(value) {
        Err(e) => {
            Err(Error::new(ErrorKind::Other, format!("key: {}, reason: {}", key, e)))
        },
        Ok(val) => {
            let lower = val.to_lowercase();
            let lower_str = lower.as_str();
            if lower_str == rhs {
                Ok(lower)
            } else {
                //如果快速匹配失败，则进行模糊匹配
                for part in lower_str.split(",") {
                    if part.trim() == rhs {
                        //匹配则退出模糊匹配，并返回匹配成功的值
                        return Ok(lower);
                    }
                }

                Err(Error::new(ErrorKind::Other,
                               format!("match failed, key: {}, val: {}, reason: invalid connect header",
                                       key,
                                       val)))
            }
        },
    }
}

// 检查Http头的值是否不为空，返回匹配成功的值
fn unmatch_header_value(key: &str, value: &[u8]) -> Result<String> {
    match from_utf8(value) {
        Err(e) => {
            Err(Error::new(ErrorKind::Other,
                           format!("key: {}, reason: {}",
                                   key,
                                   e)))
        },
        Ok(val) => {
            if val != "" {
                Ok(val.to_string())
            } else {
                Err(Error::new(ErrorKind::Other,
                               format!("unmatch failed, key: {}, val: {}, reason: invalid connect header",
                                       key,
                                       val)))
            }
        },
    }
}

// 创建握手请求响应
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
            resp = resp.status(StatusCode::SWITCHING_PROTOCOLS)
                .version(Version::HTTP_11)
                .header(CONNECTION, UPGRADE)
                .header(UPGRADE, CONNECT_UPGRADE);

            if window_bits > 0 {
                //服务器端支持客户端的压缩扩展协议，则设置服务端指定的客户端压缩窗口大小
                resp = resp.header(SEC_WEBSOCKET_EXTENSIONS,
                                   WEBSOCKET_EXTENSIONS.to_string() + "=" + &window_bits.to_string());
            }

            if let Some(protocol) = p {
                //服务器端支持客户端需要的子协议，则设置服务端指定的子协议
                resp = resp.header(SEC_WEBSOCKET_PROTOCOL, protocol);
            }

            //设置服务端指定的密钥，并返回握手成功响应
            resp.header(SEC_WEBSOCKET_ACCEPT, accept)
                .body(vec![])
        },
    }
}

// 创建非标准握手请求响应
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
            resp = resp.status(StatusCode::OK)
                .version(Version::HTTP_11)
                .header(CONTENT_TYPE, HeaderValue::from_str(mime.as_str()).unwrap())
                .header(CONTENT_LENGTH, body.len());

            //设置服务端指定的密钥，并返回握手成功响应
            resp.body(body)
        },
    }
}

// 计算握手请求相应的服务器端密钥
fn accept(key: String) -> String {
    let bin = digest(DigestAlgorithm::SHA1, (key + WEBSOCKET_GUID).as_bytes());
    base64::encode(&bin)
}

// 将握手请求的响应序列化为Vec<u8>
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