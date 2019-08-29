use std::error;
use std::str::from_utf8;
use std::fmt::{Display, Formatter, Result as FmtResult, Debug};
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};

use httparse::Request;
use http::{Result as HttpResult,
           Response,
           Version,
           status::StatusCode,
           header::{HOST, CONNECTION, UPGRADE,
                    ORIGIN, USER_AGENT,
                    SEC_WEBSOCKET_VERSION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_EXTENSIONS,
                    SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_ACCEPT}};
use base64;

use atom::Atom;
use pi_crypto::digest::{DigestAlgorithm, digest};

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
    Failed(u8),                 //握手失败
    Succeeded(String, String),  //握手成功，子协议和服务器端密钥
}

/*
* Websocket连接接受器
*/
#[derive(Clone)]
pub struct WsAcceptor {
    window_bits:    u8,     //客户端的压缩窗口大小，等于0表示，不支持每消息Deflate压缩的扩展协议
}

unsafe impl Send for WsAcceptor {}

impl Default for WsAcceptor {
    //默认构建准备握手的连接接受器
    fn default() -> Self {
        WsAcceptor {
            window_bits: 0, //默认不支持每消息Deflate压缩的扩展协议
        }
    }
}

impl WsAcceptor {
    //构建指定客户端的压缩窗口大小的连接接受器
    pub fn with_window_bits(window_bits: u8) -> Self {
        let mut handler = WsAcceptor::default();
        handler.window_bits = window_bits;
        handler
    }
}

impl WsAcceptor {
    //握手，返回握手响应
    pub fn handshake(&self, mut req: Request) -> HttpResult<Response<()>> {
        match check_handshake_request(&mut req, self.window_bits) {
            Err(e) => {
                //握手请求失败
                println!("!!!> Check Handshake Failed, reason: {:?}", e);
                reply_handshake(req, Err(StatusCode::BAD_REQUEST))
            },
            Ok(success) => {
                match success {
                    Status::Succeeded(_ws_protocol, ws_accept) => {
                        //握手请求成功，则更新握手状态，并返回握手请求的响应
                        reply_handshake(req, Ok(ws_accept.as_str()))
                    },
                    _ => {
                        //握手请求冲突
                        println!("!!!> Check Handshake Failed, reason: invalid status");
                        reply_handshake(req, Err(StatusCode::BAD_REQUEST))
                    },
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
        Err(Error::new(ErrorKind::Other, format!("invalid request header")))
    } else {
        //握手请求检查成功
        Ok(Status::Succeeded(ws_protocol, accept(ws_key)))
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
fn reply_handshake<'a>(req: Request, result: GenResult<&'a str, StatusCode>) -> HttpResult<Response<()>> {
    match result {
        Err(code) => {
            //握手失败，返回相应的响应
            Response::builder()
                .status(code)
                .version(Version::HTTP_11)
                .body(())
        },
        Ok(accept) => {
            //握手成功
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .version(Version::HTTP_11)
                .header(CONNECTION, UPGRADE)
                .header(UPGRADE, CONNECT_UPGRADE)
                .header(SEC_WEBSOCKET_ACCEPT, accept)
                .body(())
        },
    }
}

//计算握手请求相应的服务器端密钥
fn accept(key: String) -> String {
    let bin = digest(DigestAlgorithm::SHA1, (key + WEBSOCKET_GUID).as_bytes());
    base64::encode(&bin)
}
