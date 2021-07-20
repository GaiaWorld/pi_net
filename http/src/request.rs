use std::sync::Arc;
use std::str::FromStr;
use std::future::Future;
use std::io::{Error, Result, ErrorKind};

use url::Url;
use log::warn;
use bytes::BufMut;
use https::{StatusCode,
            method::Method,
            version::Version,
            header::{CONTENT_LENGTH, HeaderMap}};

use tcp::driver::{Socket, SocketHandle, AsyncIOWait, AsyncReadTask};

/*
* 默认的块大小，8KB
*/
const DEFAULT_BLOCK_SIZE: usize = 8192;

/*
* 最大块大小限制，16MB
*/
const MAX_BLOCK_LIMIT: usize = 16 * 1024 * 1024;

/*
* Http请求启始行
*/
struct StartLine {
    method:     Method,     //请求方法
    url:        Url,        //请求资源的全局位置符
    version:    Version,    //请求协议版本
}

impl StartLine {
    //使用指定的方法、Uri和协议版本，构建Http请求启始行
    pub fn new(method: &str, url: &str, version: Version) -> Option<Self> {
        match Method::from_str(method) {
            Err(e) => {
                warn!("!!!> Create Http Start Line Failed, reason: {:?}", e);
                None
            },
            Ok(method) => {
                match Url::from_str(url) {
                    Err(e) => {
                        warn!("!!!> Create Http Start Line Failed, reason: {:?}", e);
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

/*
* Http请求
*/
pub struct HttpRequest<S: Socket, W: AsyncIOWait> {
    handle:         SocketHandle<S>,        //Http连接句柄
    waits:          W,                      //异步任务等待队列
    start:          StartLine,              //Http请求启始行
    headers:        Arc<HeaderMap>,         //Http请求头
    content_len:    Option<usize>,          //Http请求体的实际长度，如果为空，表示本次请求体以流方式传输，否则表示请求体以块方式传输
    body:           Vec<u8>,                //Http请求体
    body_len:       usize,                  //Http请求体的当前长度
}

/*
* Http请求同步方法
*/
impl<S: Socket, W: AsyncIOWait> HttpRequest<S, W> {
    //构建指定的Http请求
    pub fn new(handle: SocketHandle<S>,
               waits: W,
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
            waits,
            start: start.unwrap(),
            headers: Arc::new(headers),
            content_len,
            body: Vec::from(preffix),
            body_len,
        })
    }

    //获取当前Http连接的Tcp连接句柄
    pub fn get_handle(&self) -> &SocketHandle<S> {
        &self.handle
    }

    //获取当前Http连接的异步任务等待队列
    pub fn get_waits(&self) -> &W {
        &self.waits
    }

    //获取Http请求的方法
    pub fn method(&self) -> &Method {
        &self.start.method
    }

    //获取Http请求的Url
    pub fn url(&self) -> &Url {
        &self.start.url
    }

    //获取Http请求Url的可写引用
    pub fn url_mut(&mut self) -> &mut Url {
        &mut self.start.url
    }

    //获取Http请求的方法
    pub fn version(&self) -> &Version {
        &self.start.version
    }

    //获取Http请求头
    pub fn headers(&self) -> &HeaderMap {
        self.headers.as_ref()
    }

    //获取Http请求头的共享引用
    pub fn share_headers(&self) -> Arc<HeaderMap> {
        self.headers.clone()
    }

    //设置Http请求体，只有根据CONTENT_LENGTH头将所有数据读取完以后，才允许设置Http请求体
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
impl<S: Socket, W: AsyncIOWait> HttpRequest<S, W> {
    //获取Http请求体块的所有数据，块数据会缓存，可以多次读取
    pub async fn body(&mut self) -> Option<&[u8]> {
        match self.content_len {
            Some(0) => {
                //没有剩余的Http块数据需要读取
                Some(&self.body[..])
            },
            Some(len) => {
                //本次Http请求，有请求体，还有Http块数据需要读取，则异步读取
                match AsyncReadTask::async_read(self.handle.clone(), self.waits.clone(), len).await {
                    Err(e) => {
                        //读Http体的块数据错误，则立即关闭当前Http连接
                        self.handle.close(Err(Error::new(ErrorKind::InvalidInput, e)));
                        None
                    },
                    Ok(bin) => {
                        //读Http体的块数据成功，则更新Http体的当前长度
                        self.body.put(bin);
                        self.body_len += len;
                        Some(&self.body[..])
                    },
                }
            },
            _ => {
                //本次Http请求，没有请求体或是请求体流
                None
            },
        }
    }

    //根据指定的块大小，获取Http请求体，可以用于读取大的块数据或没有指定请求长度的流数据，这种读取不会缓存数据，只允许读取一次
    pub async fn next_body(&mut self, mut block_size: usize) -> Option<&[u8]> {
        if block_size == 0 {
            //块大小等于0，则设置为默认块大小
            block_size = DEFAULT_BLOCK_SIZE;
        } else if block_size > MAX_BLOCK_LIMIT {
            //块大小大于最大块大小限制，则设置为最大块大小
            block_size = MAX_BLOCK_LIMIT;
        }

        //检查是否需要返回已读取的到缓冲区内的请求体
        if self.body_len > 0 {
            //当前有部分Http体的流数据，则返回
            let offset = self.body.len()
                .checked_sub(self.body_len)
                .unwrap_or(0); //获取本次从缓冲区内读数据的偏移

            let len;
            if self.body.len() <= block_size {
                //当前请求体缓冲区内的数据长度小于等于块大小
                len = self.body_len; //本次从缓冲区内读取的数据长度
                self.body_len = 0; //置空Http体的当前长度，防止下次重复读取当前体数据
            } else {
                //当前请求体缓冲区内的数据长度大于块大小
                len = block_size; //本次从缓冲区内读取的数据长度
                self.body_len = body_len
                    .checked_sub(block_size)
                    .unwrap_or(0); //减去块长度，防止下次重复读取当前体数据
            }

            return Some(&self.body[offset..len]);
        }

        //开始异步分块读取块或流数据
        let read_size;
        if let Some(content_len) = self.content_len {
            //读取块请求体
            if content_len == 0 {
                //没有待读取的请求体，则立即返回空
                return None;
            } else if content_len > block_size {
                //待读取的请求体长度大于块大小
                read_size = block_size;
                self.content_len = Some(content_len
                    .checked_sub(block_size)
                    .unwrap_or(0)
                );
            } else {
                //待读取的请求体长度小于块大小
                read_size = content_len;
                self.content_len = Some(0);
            }
        } else {
            //读取流请求体
            read_size = block_size;
        }

        match AsyncReadTask::async_read(self.handle.clone(), self.waits.clone(), read_size).await {
            Err(e) => {
                //读Http体的流数据错误，则立即关闭当前Http连接
                self.handle.close(Err(Error::new(ErrorKind::InvalidInput, e)));
                None
            },
            Ok(bin) => {
                Some(bin)
            }
        }
    }
}
