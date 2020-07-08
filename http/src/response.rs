use std::sync::Arc;
use std::str::FromStr;
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicU16, AtomicIsize, Ordering};

use bytes::BufMut;
use https::{status::StatusCode,
            version::Version,
            header::{HeaderMap, HeaderName, HeaderValue}};
use parking_lot::Mutex;

use tcp::driver::{Socket, SocketHandle, AsyncIOWait};

use crate::util::{HttpSender, HttpRecvResult, HttpReceiver, channel};

/*
* Http响应体，默认最大缓冲区
*/
const MAX_HTTP_RESP_BODY_BUFFER_LEN: usize = 0xffff;

/*
* Http响应启始行
*/
pub struct StartLine {
    status:     Arc<AtomicU16>, //响应状态码
    version:    Version,        //响应协议版本
}

/*
* Http响应体
*/
pub struct RespBody<S: Socket, W: AsyncIOWait> {
    consumer:   HttpReceiver<S, W, (u64, Vec<u8>)>, //Http响应体消费者
    buf:        Option<Vec<u8>>,                    //Http响应体缓冲区，不为空表示响应体已准备好
}

/*
* Http响应体同步方法
*/
impl<S: Socket, W: AsyncIOWait> RespBody<S, W> {
    //初始化Http响应体的缓冲区
    pub fn init(&mut self) -> bool {
        if self.buf.is_some() {
            //不允许重复初始化
            return false;
        }

        self.buf = Some(Vec::new());
        true
    }

    //判断是否已初始化Http响应体的缓冲区
    pub fn check_init(&self) -> bool {
        self.buf.is_some()
    }

    //获取响应体缓冲区长度
    pub fn len(&self) -> Option<usize> {
        if let Some(buf) = &self.buf {
            return Some(buf.len());
        }

        None
    }

    //获取响应体缓冲区的只读引用
    pub fn as_slice(&self) -> Option<&[u8]> {
        if let Some(buf) = &self.buf {
            return Some(buf.as_slice());
        }

        None
    }

    //获取响应体缓冲区的可写引用
    pub fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        if let Some(buf) = &mut self.buf {
            return Some(buf.as_mut_slice());
        }

        None
    }

    //在响应体缓冲区尾部，增加数据
    pub fn push(&mut self, bin: &[u8]) {
        if let Some(buf) = &mut self.buf {
            buf.put(bin);
        }
    }

    //重置响应体缓冲区，将清除当前响应体缓冲区，并用新的数据填充当前响应体缓冲区
    pub fn reset(&mut self, bin: &[u8]) {
        if let Some(buf) = &mut self.buf {
            buf.truncate(0);
            buf.put(bin);
        }
    }

    //将响应体序列化为二进制数据
    pub fn into_bin(self) -> Option<Vec<u8>> {
        self.buf
    }
}

/*
* Http响应体异步方法
*/
impl<S: Socket, W: AsyncIOWait> RespBody<S, W> {
    //获取Http响应体
    pub async fn body(&self) -> HttpRecvResult<Vec<(u64, Vec<u8>)>> {
        if self.buf.is_some() {
            return HttpRecvResult::Err(Error::new(ErrorKind::Other, "receive body failed, reason: invalid body consumer"));
        }

        self.consumer.recv().await
    }
}

/*
* Http响应句柄
*/
pub struct ResponseHandler<S: Socket> {
    status:     Arc<AtomicU16>,                 //Http响应状态码
    headers:    Arc<Mutex<HeaderMap>>,          //Http响应头
    producor:   HttpSender<S, (u64, Vec<u8>)>,  //Http响应体生产者
}

unsafe impl<S: Socket> Send for ResponseHandler<S> {}
unsafe impl<S: Socket> Sync for ResponseHandler<S> {}

impl<S: Socket> Clone for ResponseHandler<S> {
    fn clone(&self) -> Self {
        ResponseHandler {
            status: self.status.clone(),
            headers: self.headers.clone(),
            producor: self.producor.clone(),
        }
    }
}

impl<S: Socket> ResponseHandler<S> {
    //构建Http响应句柄
    pub fn new(status: Arc<AtomicU16>,
               headers: Arc<Mutex<HeaderMap>>,
               producor: HttpSender<S, (u64, Vec<u8>)>) -> Self {
        ResponseHandler {
            status,
            headers,
            producor,
        }
    }

    //线程安全的设置Http状态码
    pub fn status(&self, status: u16) {
        self.status.store(status, Ordering::Relaxed);
    }

    //线程安全的增加Http请求头
    pub fn header(&self, key: &str, value: &str) {
        if let Ok(key) = HeaderName::from_str(key) {
            if let Ok(value) = HeaderValue::from_str(value) {
                self.headers.lock().append(key, value);
            }
        }
    }

    //线程安全的写入Http响应体，默认序号为0
    pub fn write(&self, body: Vec<u8>) -> Result<()> {
        self.producor.send(Some((0, body)))
    }

    //线程安全的写入序号和Http响应体，用于按指定顺序写入响应体块
    pub fn write_index(&self, index: u64, body: Vec<u8>) -> Result<()> {
        self.producor.send(Some((index, body)))
    }

    //线程安全的结束Http响应句柄的写入
    pub fn finish(&self) -> Result<()> {
        self.producor.send(None)
    }
}

/*
* Http响应
*/
pub struct HttpResponse<S: Socket, W: AsyncIOWait> {
    handle:     SocketHandle<S>,                    //Http连接句柄
    waits:      W,                                  //异步任务等待队列
    start:      Option<StartLine>,                  //Http响应启始行, 为空表示当前Http响应为数据流响应，否则表示当前Http响应为数据块响应
    headers:    Arc<Mutex<HeaderMap>>,             //Http响应头
    body:       Option<RespBody<S, W>>,             //Http响应体
    handler:    Option<ResponseHandler<S>>,         //Http响应句柄，用于线程安全的跨运行时写响应头和响应体
}

impl<S: Socket, W: AsyncIOWait> From<HttpResponse<S, W>> for Vec<u8> {
    //Http响应序列化为二进制数据
    fn from(resp: HttpResponse<S, W>) -> Self {
        let mut buf = Vec::new();

        if let Some(start) = &resp.start {
            //当前Http响应为数据块响应，则序列化Http响应启始行
            buf.put(format!("{:?} {}\r\n", &start.version, &start.status.load(Ordering::Relaxed)).as_bytes());
        }

        //序列化Http响应头
        for (key, value) in resp.headers.lock().iter() {
            let slice: &[u8] = key.as_ref();
            buf.put_slice(&[slice, b":", value.as_bytes(), b"\r\n"].concat());
        }
        buf.put_slice(b"\r\n");

        //序列化Http响应体
        if let Some(body) = resp.body {
            if let Some(bin) = body.as_slice() {
                buf.put(bin);
            }
        }

        buf
    }
}

/*
* Http响应同步方法
*/
impl<S: Socket, W: AsyncIOWait> HttpResponse<S, W> {
    //构建空响应体的Http响应
    pub fn empty(handle: SocketHandle<S>, waits: W) -> Self {
        let start = Some(StartLine {
            status: Arc::new(AtomicU16::new(StatusCode::default().as_u16())),
            version: Version::HTTP_11,
        });

        HttpResponse {
            handle,
            waits,
            start,
            headers: Arc::new(Mutex::new(HeaderMap::new())),
            body: None,
            handler: None,
        }
    }

    //构建Http响应
    pub fn new(handle: SocketHandle<S>, waits: W, size: usize) -> Self {
        if size > MAX_HTTP_RESP_BODY_BUFFER_LEN {
            panic!("Invalid HttpResponse, reason: invalid buffer len of response body, len: {:?}", size);
        }

        let status = Arc::new(AtomicU16::new(StatusCode::default().as_u16()));
        let start = Some(StartLine {
            status: status.clone(),
            version: Version::HTTP_11,
        });
        let headers = Arc::new(Mutex::new(HeaderMap::new()));
        let (producor, consumer) = channel::<S, W, (u64, Vec<u8>)>(handle.clone(), waits.clone(), size);
        let body = RespBody {
            consumer,
            buf: None,
        };
        let handler = ResponseHandler::new(status, headers.clone(), producor);

        HttpResponse {
            handle,
            waits,
            start,
            headers,
            body: Some(body),
            handler: Some(handler),
        }
    }

    //构建基于流的Http后续响应，一般通过流方式返回，首先会返回一个空响应体的响应，然后返回后续的流响应
    pub fn stream(handle: SocketHandle<S>, waits: W, size: usize) -> Self {
        let status = Arc::new(AtomicU16::new(StatusCode::default().as_u16()));
        let headers = Arc::new(Mutex::new(HeaderMap::new()));
        let (producor, consumer) = channel::<S, W, (u64, Vec<u8>)>(handle.clone(), waits.clone(), size);
        let body = RespBody {
            consumer,
            buf: None,
        };
        let handler = ResponseHandler::new(status, headers.clone(), producor);

        HttpResponse {
            handle,
            waits,
            start: None,
            headers,
            body: Some(body),
            handler: Some(handler),
        }
    }

    //获取当前Http连接的Tcp连接句柄
    pub fn get_handle(&self) -> &SocketHandle<S> {
        &self.handle
    }

    //获取Http启始行
    pub fn start_line(&self) -> Option<&StartLine> {
        self.start.as_ref()
    }

    //设置Http响应状态码
    pub fn status(&mut self, status_code: u16) -> &mut Self {
        if let Some(start) = &mut self.start {
            start.status.store(status_code, Ordering::Relaxed);
        }

        self
    }

    //检查是否有指定的Http响应头
    pub fn contains_header(&self, key: HeaderName) -> bool {
        self.headers.lock().contains_key(key)
    }

    //增加Http响应头
    pub fn header(&mut self, key: &str, value: &str) -> &mut Self {
        if let Ok(key) = HeaderName::from_str(key) {
            if let Ok(value) = HeaderValue::from_str(value) {
                self.headers.lock().append(key, value);
            }
        }

        self
    }

    //获取Http响应体的只读引用
    pub fn as_body(&self) -> Option<&RespBody<S, W>> {
        if let Some(body) = &self.body {
            return Some(body);
        }

        None
    }

    //获取Http响应体的可写引用
    pub fn as_mut_body(&mut self) -> Option<&mut RespBody<S, W>> {
        if let Some(body) = &mut self.body {
            return Some(body);
        }

        None
    }

    //获取Http的响应句柄
    pub fn get_response_handler(&self) -> Option<ResponseHandler<S>> {
        if let Some(handler) = &self.handler {
            return Some(handler.clone());
        }

        None
    }
}