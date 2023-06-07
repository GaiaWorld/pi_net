use std::sync::Arc;
use std::str::FromStr;
use std::future::Future;
use std::io::{Error, Result, ErrorKind};

use url::Url;
use log::warn;
use bytes::{Buf, BufMut, Bytes};
use crossbeam_channel::internal::SelectHandle;
use https::{StatusCode,
            method::Method,
            version::Version,
            header::{CONTENT_LENGTH, HeaderMap}};

use tcp::{Socket, SocketHandle};

///
/// 默认的块大小，8KB
///
const DEFAULT_BLOCK_SIZE: usize = 8192;

///
/// 最大块大小限制，16MB
///
const MAX_BLOCK_LIMIT: usize = 16 * 1024 * 1024;

///
/// Http请求启始行
///
struct StartLine {
    method:     Method,     //请求方法
    url:        Url,        //请求资源的全局位置符
    version:    Version,    //请求协议版本
}

impl StartLine {
    /// 使用指定的方法、Uri和协议版本，构建Http请求启始行
    pub fn new(method: &str,
               url: &str,
               version: Version) -> Option<Self> {
        match Method::from_str(method) {
            Err(e) => {
                warn!("Create Http Start Line Failed, reason: {:?}", e);
                None
            },
            Ok(method) => {
                match Url::from_str(url) {
                    Err(e) => {
                        warn!("Create Http Start Line Failed, reason: {:?}", e);
                        None
                    },
                    Ok(url) => {
                        Some(StartLine {
                            method,
                            url,
                            version,
                        })
                    },
                }
            },
        }
    }
}

///
/// Http请求
///
pub struct HttpRequest<S: Socket> {
    handle:         SocketHandle<S>,        //Http连接句柄
    start:          StartLine,              //Http请求启始行
    headers:        Arc<HeaderMap>,         //Http请求头
    content_len:    Option<usize>,          //Http请求体的实际长度，如果为空，表示本次请求体以流方式传输，否则表示请求体以块方式传输
    body:           Vec<u8>,                //Http请求体
    body_len:       usize,                  //Http请求体的当前长度
}

/*
* Http请求同步方法
*/
impl<S: Socket> HttpRequest<S> {
    /// 构建指定的Http请求
    pub fn new(handle: SocketHandle<S>,
               method: &str,
               url: &str,
               version: Version,
               headers: HeaderMap,
               preffix: &[u8]) -> Option<Self> {
        let start = StartLine::new(method, url, version);
        if start.is_none() {
            return None;
        }

        //初始化本次Http请求的请求体长度
        let mut content_len = None;
        let body_len = preffix.len(); //已读取到的请求体长度
        if let Some(value) = headers.get(CONTENT_LENGTH) {
            if let Ok(val_str) = value.to_str() {
                if let Ok(len) = val_str.parse::<usize>() {
                    if len == 0 {
                        //本次Http请求，没有Http请求体
                        content_len = Some(0);
                    } else {
                        //设置本次请求的未读取到的请求体长度
                        content_len = Some(len
                            .checked_sub(body_len)
                            .unwrap_or(0));
                    }
                }
            }
        }

        Some(HttpRequest {
            handle,
            start: start.unwrap(),
            headers: Arc::new(headers),
            content_len,
            body: Vec::from(preffix),
            body_len,
        })
    }

    /// 获取当前Http连接的Tcp连接句柄
    pub fn get_handle(&self) -> &SocketHandle<S> {
        &self.handle
    }

    /// 获取Http请求的方法
    pub fn method(&self) -> &Method {
        &self.start.method
    }

    /// 获取Http请求的Url
    pub fn url(&self) -> &Url {
        &self.start.url
    }

    /// 获取Http请求Url的可写引用
    pub fn url_mut(&mut self) -> &mut Url {
        &mut self.start.url
    }

    /// 获取Http请求的方法
    pub fn version(&self) -> &Version {
        &self.start.version
    }

    /// 获取Http请求的Content-Length
    pub fn content_len(&self) -> Option<usize> {
        self.content_len
    }

    /// 获取Http请求头
    pub fn headers(&self) -> &HeaderMap {
        self.headers.as_ref()
    }

    /// 获取Http请求头的共享引用
    pub fn share_headers(&self) -> Arc<HeaderMap> {
        self.headers.clone()
    }

    /// 设置Http请求体，只有根据CONTENT_LENGTH头将所有数据读取完以后，才允许设置Http请求体
    pub fn set_body(&mut self, body: &[u8]) -> bool {
        if let Some(content_len) = self.content_len {
            //本次Http请求有实际长度的请求体
            if content_len > 0 {
                //请求体未读取完成，则返回设置失败
                return false;
            }

            //本次Http请求没有请求体或请求体已读取完成，则可以设置请求体
            self.body.clear();
            self.body.put_slice(body);
        }

        true
    }
}

/*
* Http请求异步方法
*/
impl<S: Socket> HttpRequest<S> {
    /// 获取Http请求体块的所有数据，块数据会缓存，可以多次读取
    pub async fn body(&mut self) -> Option<&[u8]> {
        match self.content_len {
            Some(0) => {
                //没有剩余的Http块数据需要读取
                Some(&[])
            },
            Some(len) => {
                //本次Http请求，有请求体，还有Http块数据需要读取，则异步读取
                loop {
                    let current_body_len = unsafe {
                        (&mut *self.handle.get_read_buffer().get())
                            .as_ref()
                            .unwrap()
                            .remaining()
                    };
                    if current_body_len < len {
                        //当前缓冲区还没有接收到指定长度的请求体，则异步准备读取后，继续尝试接收剩余请求体
                        let require_len = len
                            .checked_sub(current_body_len)
                            .unwrap_or(0); //需要继续接收的剩余请求体长度

                        if require_len > 0 {
                            //还有未接收到的剩余请求体
                            if let Ok(value) = self.handle.read_ready(require_len) {
                                if value.await == 0 {
                                    //当前连接已关闭，则立即返回空
                                    return None;
                                }
                            }
                        }

                        continue;
                    }

                    //读请求体成功，则更新Http体的当前长度，并退出本次异步读取请求体
                    self.body_len = unsafe {
                        (&*self.handle.get_read_buffer().get())
                            .as_ref()
                            .unwrap()
                            .remaining()
                    };
                    break;
                }

                Some(unsafe {
                    (&*self.handle.get_read_buffer().get()).as_ref().unwrap().as_ref()
                })
            },
            _ => {
                //本次Http请求，没有请求体或是请求体流
                None
            },
        }
    }

    /// 获取Http请求体块的所有数据，块数据不会缓存，只允许读取一次
    pub async fn take_body(&mut self) -> Option<Bytes> {
        match self.content_len {
            Some(0) => {
                //没有剩余的Http块数据需要读取
                Some(Bytes::new())
            },
            Some(len) => {
                //本次Http请求，有请求体，还有Http块数据需要读取，则异步读取
                loop {
                    let current_body_len = unsafe {
                        (&mut *self.handle.get_read_buffer().get())
                            .as_ref()
                            .unwrap()
                            .remaining()
                    };
                    if current_body_len < len {
                        //当前缓冲区还没有接收到指定长度的请求体，则异步准备读取后，继续尝试接收剩余请求体
                        let require_len = len
                            .checked_sub(current_body_len)
                            .unwrap_or(0); //需要继续接收的剩余请求体长度

                        if require_len > 0 {
                            //还有未接收到的剩余请求体
                            if let Ok(value) = self.handle.read_ready(require_len) {
                                if value.await == 0 {
                                    //当前连接已关闭，则立即返回空
                                    return None;
                                }
                            }
                        }

                        continue;
                    }

                    //读请求体成功，则更新Http体的当前长度，并退出本次异步读取请求体
                    self.body_len = unsafe {
                        (&*self.handle.get_read_buffer().get())
                            .as_ref()
                            .unwrap()
                            .remaining()
                    };
                    break;
                }

                Some(unsafe {
                    (&mut *self.handle.get_read_buffer().get())
                        .as_mut()
                        .unwrap()
                        .copy_to_bytes(len)
                })
            },
            _ => {
                //本次Http请求，没有请求体或是请求体流
                None
            },
        }
    }

    /// 根据指定的块大小，获取Http请求体，
    /// 可以用于读取大的块数据或没有指定请求长度的流数据，这种读取不会缓存数据，只允许读取一次
    pub async fn next_body(&mut self,
                           mut block_size: usize) -> Option<Bytes> {
        loop {
            if unsafe { (&mut *self.handle.get_read_buffer().get()).as_ref().unwrap().remaining() } == 0 {
                //当前缓冲区还没有请求的数据，则异步准备读取后，继续尝试接收请求数据
                if let Ok(value) = self.handle.read_ready(0) {
                    if value.await == 0 {
                        //当前连接已关闭，则立即退出
                        return None;
                    }
                }

                continue;
            }

            let remaining = unsafe {
                (&mut *self.handle.get_read_buffer().get())
                    .as_ref()
                    .unwrap()
                    .remaining()
            };
            if remaining < block_size {
                //连接当前读缓冲区剩余可读字节数小于指定的块大小，则读取所有读缓冲区的剩余可读字节
                block_size = remaining;
            }

            let bytes = unsafe { (&mut *self.handle.get_read_buffer().get())
                .as_mut()
                .unwrap()
                .copy_to_bytes(block_size) };
            self.body_len += bytes.remaining(); //更新Http体的当前长度
            return Some(bytes);
        }
    }
}
