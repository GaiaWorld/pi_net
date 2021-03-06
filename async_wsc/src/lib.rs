#[macro_use]
extern crate lazy_static;

use std::thread;
use std::rc::Rc;
use std::pin::Pin;
use std::sync::Arc;
use std::str::FromStr;
use std::cell::{UnsafeCell, RefCell};
use std::future::Future;
use std::net::SocketAddr;
use std::time::{Instant, Duration};
use std::task::{Context, Poll, Waker};
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::{FutureExt, BoxFuture};
use futures_util::{sink::SinkExt, stream::StreamExt};
use url::Url;
use actix_rt::{System, Arbiter};
use actix_codec::Framed;
use actix_http::ws::{Codec, Item};
use awc::{Client, BoxedSocket, ws::{self, Frame, Message, CloseCode, CloseReason}};
use crossbeam_channel::{Sender, Receiver, unbounded, bounded};
use parking_lot::RwLock;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::{info, error};

use r#async::rt::{TaskId, AsyncTaskPool, AsyncTaskPoolExt, AsyncRuntime, AsyncWaitResult};

/*
* Websocket连接
*/
thread_local! {
    static ASYNC_WEBSOCKET_CONNECTION: Arc<UnsafeCell<Option<Framed<BoxedSocket, Codec>>>> = Arc::new(UnsafeCell::new(None));
}

/*
* 异步Websocket客户端，客户端同时只允许存在一个Websocket连接，创建新连接时会关闭旧连接
*/
pub struct AsyncWebsocketClient<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> {
    rt:             AsyncRuntime<(), P>,   //外部异步运行时
    arbiter:        Arc<Arbiter>,       //客户端所在异步运行时
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncWebsocketClient<P> {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncWebsocketClient<P> {}

/*
* 异步Websocket客户端同步方法
*/
impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> AsyncWebsocketClient<P> {
    //构建异步Websocket客户端
    pub fn new(rt: AsyncRuntime<(), P>,
               name: String) -> Result<AsyncWebsocketClient<P>> {
        //打开新的线程，来运行异步Websocket客户端
        let (send, recv) = bounded(1);
        thread::spawn(move || {
            let mut runner = System::new(name.clone());
            send.send(Arc::new(System::current().arbiter().clone()));
            info!("Build websocket client ok, name: {:?}", name);
            runner.run();
        });

        match recv.recv() {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Build websocket client failed, reason: {:?}", e)))
            },
            Ok(arbiter) => {
                Ok(AsyncWebsocketClient {
                    rt,
                    arbiter,
                })
            },
        }
    }

    //构建异步Websocket连接
    pub fn build(&self,
                 url: &str,
                 protocols: Vec<String>,
                 handler: AsyncWebsocketHandler) -> Result<AsyncWebsocket<P>> {
        match Url::from_str(url) {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Open websocket failed, url: {}, reason: {:?}", url, e)))
            },
            Ok(url) => {
                let mut ws = AsyncWebsocket::new(self.rt.clone(),
                                                 self.arbiter.clone(),
                                                 url,
                                                 protocols,
                                                 handler);
                ws.set_task_id(self.rt.alloc());

                Ok(ws)
            },
        }
    }

    //关闭客户端，同时关闭连接
    pub fn close(&self) {
        self.arbiter.send(Box::pin(async move {
            Arbiter::spawn(async move {
                match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                    shared.clone()
                }) {
                    Err(e) => {
                        error!("Close websocket client failed, reason: {:?}", e);
                    },
                    Ok(shared) => {
                        //当前客户端有连接，则立即关闭连接
                        if let Some(ws_con) = unsafe { (&mut *shared.get()) } {
                            ws_con.send(ws::Message::Close(None)).await;
                        }
                    },
                }

                System::current().arbiter().stop();
            });
        }));
    }
}

/*
* 异步Websocket连接状态
*/
#[derive(Debug)]
pub enum AsyncWebsocketStatus {
    Connecting(Url, Vec<String>),                               //连接中
    Connected(Url, Vec<String>),                                //已连接
    Closing(Url, Vec<String>, Option<CloseCode>),               //关闭中
    Closed(Url, Vec<String>, Option<CloseCode>),                //已关闭
    Error(Url, Vec<String>, Option<CloseCode>, Option<Error>),  //错误
}

unsafe impl Send for AsyncWebsocketStatus {}
unsafe impl Sync for AsyncWebsocketStatus {}

impl AsyncWebsocketStatus {
    //是否正在连接中
    pub fn is_connecting(&self) -> bool {
        if let AsyncWebsocketStatus::Connecting(_, _) = self {
            true
        } else {
            false
        }
    }

    //是否已连接
    pub fn is_connected(&self) -> bool {
        if let AsyncWebsocketStatus::Connected(_, _) = self {
            true
        } else {
            false
        }
    }

    //是否正在关闭中
    pub fn is_closing(&self) -> bool {
        if let AsyncWebsocketStatus::Closing(_, _, _) = self {
            true
        } else {
            false
        }
    }

    //是否已关闭
    pub fn is_closed(&self) -> bool {
        if let AsyncWebsocketStatus::Closed(_, _, _) = self {
            true
        } else {
            false
        }
    }

    //是否错误
    pub fn is_error(&self) -> bool {
        if let AsyncWebsocketStatus::Error(_, _, _, error) = self {
            true
        } else {
            false
        }
    }

    //获取错误
    pub fn get_error(&mut self) -> Option<Error> {
        if let AsyncWebsocketStatus::Error(_, _, _, error) = self {
            error.take()
        } else {
            None
        }
    }

    //获取连接的url
    pub fn get_url(&self) -> &Url {
        match self {
            AsyncWebsocketStatus::Connecting(url, _) => url,
            AsyncWebsocketStatus::Connected(url, _) => url,
            AsyncWebsocketStatus::Closing(url, _, _) => url,
            AsyncWebsocketStatus::Closed(url, _, _) => url,
            AsyncWebsocketStatus::Error(url, _, _, _) => url,
        }
    }

    //获取连接的子协议
    pub fn get_protocols(&self) -> &[String] {
        match self {
            AsyncWebsocketStatus::Connecting(_, protocols) => protocols.as_slice(),
            AsyncWebsocketStatus::Connected(_, protocols) => protocols.as_slice(),
            AsyncWebsocketStatus::Closing(_, protocols, _) => protocols.as_slice(),
            AsyncWebsocketStatus::Closed(_, protocols, _) => protocols.as_slice(),
            AsyncWebsocketStatus::Error(_, protocols, _, _) => protocols.as_slice(),
        }
    }

    //将状态转换为数值
    pub fn to_number(&self) -> isize {
        match self {
            AsyncWebsocketStatus::Connecting(_, _) => 0,
            AsyncWebsocketStatus::Connected(_, _) => 1,
            AsyncWebsocketStatus::Closing(_, _, _) => 2,
            AsyncWebsocketStatus::Closed(_, _, _) => 3,
            AsyncWebsocketStatus::Error(_, _, _, _) => -1,
        }
    }
}

/*
* 异步Websocket连接
*/
pub struct AsyncWebsocket<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>>(Arc<RwLock<InnerWebsocket<P>>>);

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncWebsocket<P> {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncWebsocket<P> {}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> Clone for AsyncWebsocket<P> {
    fn clone(&self) -> Self {
        AsyncWebsocket(self.0.clone())
    }
}

/*
* 异步Websocket连接同步方法
*/
impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> AsyncWebsocket<P> {
    //构建异步Websocket连接
    fn new(rt: AsyncRuntime<(), P>,
           arbiter: Arc<Arbiter>,
           url: Url,
           protocols: Vec<String>,
           handler: AsyncWebsocketHandler) -> Self {
        let inner = InnerWebsocket {
            rt,
            arbiter,
            task_id: None,
            handler,
            status: AsyncWebsocketStatus::Connecting(url, protocols),
            origin: None,
            send_size: None,
            recv_size: None,
            masking_strict: false,
            is_nodelay: false,
        };

        AsyncWebsocket(Arc::new(RwLock::new(inner)))
    }

    //设置Websocket握手时的Origin，默认没有Origin
    pub fn set_origin(&self, origin: String) {
        self.0.write().origin = Some(origin);
    }

    //设置发送帧的最大长度，超过长度的消息在发送时将被自动分帧，单位字节，最小3B，默认64KB
    pub fn set_send_frame_limit(&self, mut size: usize) {
        if size < 3 {
            //发送帧过小
            size = 3;
        }

        //减去Websocket帧头长度
        let real_size = if size < 126 {
            size - 2
        } else if size <= 65535 {
            size - 4
        } else {
            size - 10
        };
        self.0.write().send_size = Some(real_size);
    }

    //设置发送帧的最大长度，超过长度的消息在发送时将被自动分帧，单位字节，最小7B，默认64KB，且允许严格的掩码处理，默认不允许
    pub fn set_send_frame_limit_and_enable_strict_masking(&self, mut size: usize) {
        if size < 7 {
            //发送帧过小
            size = 7;
        }

        //减去Websocket帧头长度，包括了掩码长度
        let real_size = if size < 126 {
            size - 6
        } else if size <= 65535 {
            size - 10
        } else {
            size - 14
        };
        self.0.write().send_size = Some(real_size);
    }

    //设置接收帧的最大长度，超过长度的消息在接收时将返回错误，单位字节，最小127B，默认64KB
    pub fn set_receive_frame_limit(&self, mut size: usize) {
        if size < 127 {
            //接收帧过小
            size = 127;
        }

        self.0.write().recv_size = Some(size);
    }

    //允许立即刷新连接缓冲区，不允许则会由缓冲区自动刷新，默认不允许
    pub fn enable_nodelay(&self) {
        self.0.write().is_nodelay = true;
    }

    //设置异步任务id
    pub fn set_task_id(&self, task_id: TaskId) {
        self.0.write().task_id = Some(task_id);
    }

    //获取异步Websocket连接的状态
    pub fn get_status(&self) -> isize {
        self.0.read().status.to_number()
    }

    //获取异步Websocket连接的处理器
    pub fn get_handler(&self) -> AsyncWebsocketHandler {
        self.0.read().handler.clone()
    }
}

/*
* 异步Websocket连接异步方法
*/
impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> AsyncWebsocket<P> {
    //打开异步Websocket连接
    pub async fn open(&self, timeout: u64) -> Result<()> {
        let rt = self.0.read().rt.clone();
        let url = self.0.read().status.get_url().clone();
        let protocols = self.0.read().status.get_protocols().to_vec();
        let task_id = self.0.write().task_id.take().unwrap();
        let ws = self.clone();

        rt.clone().wait_any(vec![
            (rt.clone(),
             AsyncOpenWebsocket::new(rt.clone(),
                                     task_id,
                                     ws).boxed()),
            (rt.clone(),
             async move {
                 rt.wait_timeout(timeout as usize).await;
                 Err(Error::new(ErrorKind::TimedOut, format!("Open websocket failed, url: {}, protocols: {:?}, reason: connect timeout", url, protocols)))
             }.boxed())]).await
    }

    //发送消息
    pub async fn send(&self, msg: AsyncWebsocketMessage) -> Result<()> {
        let rt = self.0.read().rt.clone();
        let ws = self.clone();
        let task_id = rt.alloc();

        AsyncWebsocketSend::new(rt, task_id, ws, msg).await
    }

    //接收一次消息，可以限制一次最多接收多少消息，None表示将接收缓冲区中所有的消息
    pub async fn receive_once(&self, mut limit: Option<usize>) -> Result<()> {
        limit = match limit {
            Some(0) => {
                //至少需要接收一个消息
                Some(1)
            },
            any => any,
        };

        let rt = self.0.read().rt.clone();
        let ws = self.clone();
        let task_id = rt.alloc();

        AsyncWebsocketReceive::new(rt, task_id, ws, limit).await
    }

    //关闭异步WebSocket连接
    pub async fn close(&self, code: AsyncWebsocketCloseCode) -> Result<()> {
        let rt = self.0.read().rt.clone();
        let ws = self.clone();
        let task_id = rt.alloc();

        AsyncCloseWebsocket::new(rt, task_id, ws, Some(code)).await
    }
}

//内部Websocket连接
struct InnerWebsocket<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> {
    rt:             AsyncRuntime<(), P>,    //外部异步运行时
    arbiter:        Arc<Arbiter>,           //连接所在异步运行时
    task_id:        Option<TaskId>,         //异步任务id
    handler:        AsyncWebsocketHandler,  //处理器
    status:         AsyncWebsocketStatus,   //连接状态
    origin:         Option<String>,         //设置握手时的Origin
    send_size:      Option<usize>,          //最大发送帧大小，超过则分帧，单位字节
    recv_size:      Option<usize>,          //最大接收帧大小，超过则错误，单位字节
    masking_strict: bool,                   //是否严格的掩码处理
    is_nodelay:     bool,                   //是否立即刷新连接
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for InnerWebsocket<P> {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for InnerWebsocket<P> {}

//发送消息帧
#[inline]
fn send_frames(msg: Message, fragment_size: Option<usize>) -> Vec<Message> {
    let mut msgs = Vec::new();
    match msg {
        Message::Text(str) => {
            //文本消息
            if let Some(size) = fragment_size {
                //指定了帧最大长度
                if str.as_bytes().len() > size {
                    //需要分帧
                    let mut is_first_frame = true;
                    let parts = split_parts(Bytes::from(str.into_bytes()), size);
                    let mut parts_len = parts.len();
                    for part in parts {
                        if is_first_frame {
                            //首帧
                            msgs.push(Message::Continuation(Item::FirstText(part)));
                            is_first_frame = false; //已处理首帧
                            parts_len -= 1;
                            continue;
                        }

                        if parts_len == 1 {
                            //尾帧
                            msgs.push(Message::Continuation(Item::Last(part)));
                            break;
                        }

                        //后续帧
                        msgs.push(Message::Continuation(Item::Continue(part)));
                        parts_len -= 1;
                    }

                    return msgs;
                }
            }

            //不需要分帧
            msgs.push(Message::Text(str));
            msgs
        },
        Message::Binary(bin) => {
            //二进制消息
            if let Some(size) = fragment_size {
                //指定了帧最大长度
                if bin.len() > size {
                    //需要分帧
                    let mut is_first_frame = true;
                    let mut msgs = Vec::new();
                    let parts = split_parts(bin, size);
                    let mut parts_len = parts.len();
                    for part in parts {
                        if is_first_frame {
                            //首帧
                            msgs.push(Message::Continuation(Item::FirstBinary(part)));
                            is_first_frame = false; //已处理首帧
                            parts_len -= 1;
                            continue;
                        }

                        if parts_len == 1 {
                            //尾帧
                            msgs.push(Message::Continuation(Item::Last(part)));
                            break;
                        }

                        //后续帧
                        msgs.push(Message::Continuation(Item::Continue(part)));
                        parts_len -= 1;
                    }

                    return msgs
                }
            }

            //不需要分帧
            msgs.push(Message::Binary(bin));
            msgs
        },
        msg => {
            //单帧消息
            msgs.push(msg);
            msgs
        }
    }
}

//分帧
fn split_parts(bin: Bytes, fragment_size: usize) -> Vec<Bytes> {
    let mut parts = Vec::new();

    let mut offset = 0;
    let mut len = offset + fragment_size;
    let mut top = bin.len(); //缓冲区当前长度
    loop {
        top -= (len - offset); //更新缓冲区当前长度
        let part = bin.slice(offset..len);
        if top == 0 {
            //分帧完成，则立即返回
            parts.push(part);
            return parts;
        }

        //加入分帧列表，并更新偏移
        parts.push(part);
        offset = len;
        if top <= fragment_size {
            //剩余的缓冲区当前长度不足，则设置为剩余长度
            len = offset + top;
        } else {
            //剩余的缓冲区当前长度足够，则设置为单帧最大长度
            len = offset + fragment_size;
        }
    }
}

//已接收的消息
enum ReceivedMessage {
    Discomplete(bool, BytesMut),    //不完整的消息，标记是否为二进制消息
    Completed(Message),             //完整的消息
    Err(Error),                     //错误
}

//接收消息帧
#[inline]
fn receive_frames(received: Option<ReceivedMessage>,
                  frame: Frame) -> ReceivedMessage {
    match frame {
        Frame::Continuation(item) => {
            //接收到消息的部分帧
            match item {
                Item::Continue(bytes) => {
                    //接收到消息的后续帧
                    if let Some(ReceivedMessage::Discomplete(is_binary, mut buf)) = received {
                        //已接收到前继帧，则可以继续接收后续帧，并返回合并后的不完整的已接收消息
                        buf.put(bytes);
                        ReceivedMessage::Discomplete(is_binary, buf)
                    } else {
                        //未接收到前继帧，则忽略接收，并立即返回错误
                        ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket next frame failed, reason: require received message")))
                    }
                },
                Item::FirstText(bytes) => {
                    //接收到文本消息的头帧
                    let mut buf = BytesMut::new();
                    buf.put(bytes);
                    ReceivedMessage::Discomplete(false, buf)
                },
                Item::FirstBinary(bytes) => {
                    //接收到二进制消息的头帧
                    let mut buf = BytesMut::new();
                    buf.put(bytes);
                    ReceivedMessage::Discomplete(true, buf)
                },
                Item::Last(bytes) => {
                    //接收到消息的尾帧
                    if let Some(ReceivedMessage::Discomplete(is_binary, mut buf)) = received {
                        //已接收到前继帧，则可以继续接收尾帧，并返回合并后的完整的已接收消息
                        buf.put(bytes);
                        if is_binary {
                            //二进制消息
                            ReceivedMessage::Completed(Message::Binary(buf.to_bytes()))
                        } else {
                            //文本消息
                            match String::from_utf8(buf.to_vec()) {
                                Err(e) => {
                                    //消息不符合utf8编码，则立即返回错误
                                    ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket tail frame failed, reason: {:?}", e)))
                                },
                                Ok(str) => {
                                    ReceivedMessage::Completed(Message::Text(str))
                                }
                            }
                        }
                    } else {
                        //未接收到前继帧，则忽略接收，并立即返回错误
                        ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket tail frame failed, reason: require received message")))
                    }
                },
            }
        },
        Frame::Text(bytes) => {
            //接收到完整的文本消息
            match String::from_utf8(bytes.to_vec()) {
                Err(e) => {
                    //消息不符合utf8编码，则立即返回错误
                    ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket single text frame failed, reason: {:?}", e)))
                },
                Ok(str) => {
                    ReceivedMessage::Completed(Message::Text(str))
                }
            }
        },
        Frame::Binary(bytes) => {
            //接收到完整的二进制消息
            ReceivedMessage::Completed(Message::Binary(bytes))
        },
        Frame::Ping(bytes) => {
            //接收到服务器发送的Ping消息，客户端暂时不处理由服务器发送的Ping消息
            ReceivedMessage::Completed(Message::Ping(bytes))
        },
        Frame::Pong(bytes) => {
            //接收到Pong消息
            ReceivedMessage::Completed(Message::Pong(bytes))
        },
        Frame::Close(reason) => {
            //接收到服务器发送的关闭消息
            ReceivedMessage::Completed(Message::Close(reason))
        },
    }
}

//异步打开Websocket连接
pub struct AsyncOpenWebsocket<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> {
    rt:         AsyncRuntime<(), P>,    //异步运行时
    task_id:    TaskId,                 //异步任务id
    ws:         AsyncWebsocket<P>,      //连接
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncOpenWebsocket<P> {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncOpenWebsocket<P> {}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> Future for AsyncOpenWebsocket<P> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.ws.0.read().status.is_connected() {
            //已打开Websocket连接，则返回
            return Poll::Ready(Ok(()));
        } else if self.ws.0.read().status.is_error() {
            //打开Websocket连接错误，则返回
            if let Some(e) = self.ws.0.write().status.get_error() {
                return Poll::Ready(Err(e));
            }
        }

        let task_id = self.task_id.clone();
        let arbiter = &self.ws.0.read().arbiter;
        let url = self.ws.0.read().status.get_url().clone();
        let protocols = self.ws.0.read().status.get_protocols().to_vec();
        let ws = self.ws.clone();

        arbiter.send(Box::pin(async move {
            Arbiter::spawn(async move {
                //建立指定url的连接
                let mut ws_req = Client::new()
                    .ws(url.as_str())
                    .protocols(protocols.clone());

                if let Some(origin) = &ws.0.read().origin {
                    //设置了Origin
                    ws_req = ws_req.origin(origin.as_str());
                }
                if let Some(size) = &ws.0.read().recv_size {
                    //设置了最大接收帧大小
                    ws_req = ws_req.max_frame_size(*size);
                }
                if ws.0.read().masking_strict {
                    //设置了严格的掩码处理
                    ws_req = ws_req.server_mode();
                }

                match ws_req.connect().await {
                    Err(e) => {
                        let reason = format!("Open websocket failed, url: {}, protocols: {:?}, reason: {:?}", url, protocols, e);
                        ws.0.write().status = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                        ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                        ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步打开连接的任务
                        error!("{}", reason);
                    },
                    Ok((resp, connection)) => {
                        //打开Websocket连接成功
                        match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                            let mut last_ws_con = None;
                            if unsafe { (&mut *shared.get()) }.is_some() {
                                //当前客户端有连接
                                last_ws_con = unsafe { (&mut *shared.get()) }.take();
                            }

                            //重置当前客户端的连接
                            *unsafe { (&mut *shared.get()) } = Some(connection);
                            last_ws_con
                        }) {
                            Err(e) => {
                                //设置当前客户端的连接失败
                                let reason = format!("Open websocket failed, url: {}, protocols: {:?}, reason: {:?}", url, protocols, e);
                                ws.0.write().status = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                                ws.0.read().rt.wakeup(&task_id); //唤醒打开连接的异步任务
                                error!("{}", reason);
                            },
                            Ok(last_ws_con) => {
                                if let Some(mut last_ws_con) = last_ws_con {
                                    //立即关闭旧连接
                                    last_ws_con.send(ws::Message::Close(None)).await;
                                }

                                ws.0.write().status = AsyncWebsocketStatus::Connected(url.clone(), protocols.clone()); //设置连接状态
                                ws.0.read().handler.on_open(); //通知处理器连接成功
                                ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步打开连接的任务
                                info!("Open websocket ok, url: {}, protocols: {:?}, resp: {:?}", url, protocols, resp);
                            },
                        }
                    }
                }
            });
        }));

        //挂起打开连接的异步任务
        self.ws.0.read().rt.pending(&self.task_id, cx.waker().clone())
    }
}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> AsyncOpenWebsocket<P> {
    //创建异步打开Websocket连接
    pub fn new(rt: AsyncRuntime<(), P>,
               task_id: TaskId,
               ws: AsyncWebsocket<P>) -> Self {
        AsyncOpenWebsocket {
            rt,
            task_id,
            ws,
        }
    }
}

//Websocket异步发送消息
pub struct AsyncWebsocketSend<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> {
    rt:         AsyncRuntime<(), P>,    //异步运行时
    task_id:    TaskId,                 //异步任务id
    ws:         AsyncWebsocket<P>,      //连接
    msg:        Option<Message>,        //消息
    result:     AsyncWaitResult<()>,    //异步发送消息的结果
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncWebsocketSend<P> {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncWebsocketSend<P> {}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> Future for AsyncWebsocketSend<P> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.result.0.borrow_mut().take() {
            //消息已发送，则返回
            return Poll::Ready(result);
        }

        let msg = self.msg.take().unwrap();
        let task_id = self.task_id.clone();
        let arbiter = &self.ws.0.read().arbiter;
        let ws = self.ws.clone();
        let result = self.result.clone();

        arbiter.send(Box::pin(async move {
            Arbiter::spawn(async move {
                match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                    shared.clone()
                }) {
                    Err(e) => {
                        let url = ws.0.read().status.get_url().clone();
                        let reason = format!("Websocket send failed, url: {}, reason: {:?}", url, e);
                        let error = Error::new(ErrorKind::Other, reason.clone());
                        *result.0.borrow_mut() = Some(Err(error)); //设置发送消息的结果
                        ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                        ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步发送消息的任务
                        error!("{}", reason);
                    },
                    Ok(shared) => {
                        //当前客户端有连接，则立即发送消息
                        if let Some(ws_con) = unsafe { (&mut *shared.get()) } {
                            for msg in send_frames(msg, ws.0.read().send_size) {
                                //发送单帧或多帧消息
                                if let Err(e) = ws_con.send(msg).await {
                                    //发送消息失败
                                    let url = ws.0.read().status.get_url().clone();
                                    let reason = format!("Websocket send failed, url: {}, reason: {:?}", url, e);
                                    let error = Error::new(ErrorKind::Other, reason.clone());
                                    *result.0.borrow_mut() = Some(Err(error)); //设置发送消息的结果
                                    ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                                    ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步发送消息的任务
                                    return;
                                }

                                if ws.0.read().is_nodelay {
                                    //TODO 立即刷新连接...
                                }
                            }

                            //发送消息成功
                            *result.0.borrow_mut() = Some(Ok(())); //设置发送消息的结果
                            ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步发送消息的任务
                        }
                    },
                }
            });
        }));

        //挂起发送消息的异步任务
        self.ws.0.read().rt.pending(&self.task_id, cx.waker().clone())
    }
}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> AsyncWebsocketSend<P> {
    //创建异步打开Websocket连接
    pub fn new(rt: AsyncRuntime<(), P>,
               task_id: TaskId,
               ws: AsyncWebsocket<P>,
               msg: Message) -> Self {
        AsyncWebsocketSend {
            rt,
            task_id,
            ws,
            msg: Some(msg),
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        }
    }
}

//Websocket异步接收消息
pub struct AsyncWebsocketReceive<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> {
    rt:         AsyncRuntime<(), P>,    //异步运行时
    task_id:    TaskId,                 //异步任务id
    ws:         AsyncWebsocket<P>,      //连接
    limit:      Option<usize>,          //一次最多可以接收多少消息
    result:     AsyncWaitResult<()>,    //异步接收消息的结果
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncWebsocketReceive<P> {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncWebsocketReceive<P> {}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> Future for AsyncWebsocketReceive<P> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.result.0.borrow_mut().take() {
            //消息已接收，则返回
            return Poll::Ready(result);
        }

        let task_id = self.task_id.clone();
        let arbiter = self.ws.0.read().arbiter.clone();
        let ws = self.ws.clone();
        let limit = self.limit.clone();
        let result = self.result.clone();

        receive_message(task_id, arbiter, ws, limit, 0, result, None);

        //挂起接收消息的异步任务
        self.ws.0.read().rt.pending(&self.task_id, cx.waker().clone())
    }
}

//接收Webscoket消息
fn receive_message<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>>(task_id: TaskId,
                                                                          arbiter: Arc<Arbiter>,
                                                                          ws: AsyncWebsocket<P>,
                                                                          require_count: Option<usize>,
                                                                          received_count: usize,
                                                                          result: AsyncWaitResult<()>,
                                                                          mut received_message: Option<ReceivedMessage>) {
    let arbiter_copy = arbiter.clone();
    arbiter.send(Box::pin(async move {
        Arbiter::spawn(async move {
            match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                shared.clone()
            }) {
                Err(e) => {
                    let url = ws.0.read().status.get_url().clone();
                    let reason = format!("Websocket receive failed, url: {}, reason: {:?}", url, e);
                    let error = Error::new(ErrorKind::Other, reason.clone());
                    *result.0.borrow_mut() = Some(Err(error)); //设置接收消息的结果
                    ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                    ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步接收消息的任务
                    error!("{}", reason);
                },
                Ok(shared) => {
                    //当前客户端有连接，则立即接收消息，如果当前缓冲区为空，则会挂起接收的异步任务
                    if let Some(ws_con) = unsafe { (&mut *shared.get()) } {
                        if let Some(0) = require_count {
                            //有本次接收的最大消息数限制，且已接收指定数量的消息，则退出本次消息接收
                            *result.0.borrow_mut() = Some(Ok(())); //设置接收消息的结果
                            ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步接收消息的任务
                            return;
                        }

                        //已接收至少一条消息，检查缓冲区是否还有未接收的消息
                        if received_count > 0 && ws_con.is_read_buf_empty() {
                            if require_count.is_some() {
                                //有本次接收的最大消息数限制，且当前缓冲区没有可接收的消息帧，但还有未接收的指定数量的消息，则继续异步接收剩余消息，并立即结束当前接收
                                let ws_copy = ws.clone();
                                ws.0.read().rt.spawn_timing(ws.0.read().rt.alloc(), async move {
                                    //通过其它运行时投递继续接收的任务，避免独占Websocket连接的运行时
                                    receive_message(task_id,
                                                    arbiter_copy,
                                                    ws_copy,
                                                    require_count,
                                                    received_count,
                                                    result,
                                                    received_message);
                                }, 10);
                                return;
                            } else {
                                //无本次接收的最大消息数限制，且当前缓冲区没有可接收的消息帧，则立即退出本次消息接收
                                *result.0.borrow_mut() = Some(Ok(())); //设置接收消息的结果
                                ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步接收消息的任务
                                return;
                            }
                        }

                        let r = ws_con.next().await;
                        if let Some(respone) = r {
                            match respone {
                                Err(e) => {
                                    //接收消息帧失败，则立即退出本次消息接收
                                    let url = ws.0.read().status.get_url().clone();
                                    ws.0.read().handler.on_error(format!("Websocket receive failed, url: {}, reason: {:?}", url, e));
                                    let reason = format!("Websocket receive failed, url: {}, reason: {:?}", url, e);
                                    let error = Error::new(ErrorKind::Other, reason.clone());
                                    *result.0.borrow_mut() = Some(Err(error)); //设置接收消息的结果
                                    ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                                    ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步接收消息的任务
                                    error!("{}", reason);
                                    return;
                                },
                                Ok(frame) => {
                                    //接收消息帧成功
                                    received_message = Some(receive_frames(received_message, frame));
                                    if let Some(ReceivedMessage::Discomplete(_, _)) = &received_message {
                                        //接收消息不完整，则继续接收消息的后续帧，并立即结束当前结束
                                        let ws_copy = ws.clone();
                                        ws.0.read().rt.spawn_timing(ws.0.read().rt.alloc(), async move {
                                            //通过其它运行时投递继续接收的任务，避免独占Websocket连接的运行时
                                            receive_message(task_id,
                                                            arbiter_copy,
                                                            ws_copy,
                                                            require_count,
                                                            received_count,
                                                            result,
                                                            received_message);
                                        }, 10);
                                        return;
                                    }

                                    match received_message {
                                        Some(ReceivedMessage::Err(e)) => {
                                            //接收消息帧失败，则立即退出本次消息接收
                                            let url = ws.0.read().status.get_url().clone();
                                            let reason = format!("Websocket receive failed, url: {}, reason: {:?}", url, e);
                                            let error = Error::new(ErrorKind::Other, reason.clone());
                                            *result.0.borrow_mut() = Some(Err(error)); //设置接收消息的结果
                                            ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                                            ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步接收消息的任务
                                            error!("{}", reason);
                                            return;
                                        },
                                        Some(ReceivedMessage::Completed(msg)) => {
                                            //接收消息完整
                                            match msg {
                                                Message::Close(None) => {
                                                    //接收到关闭消息
                                                    ws.0.read().handler.on_close(CloseCode::Normal, "");
                                                },
                                                Message::Close(Some(reason)) => {
                                                    //接收到关闭消息
                                                    let url = ws.0.read().status.get_url().clone();
                                                    let protocols = ws.0.read().status.get_protocols().to_vec();
                                                    ws.0.write().status = AsyncWebsocketStatus::Closed(url, protocols, Some(reason.code)); //设置连接状态
                                                    if let Some(desc) = reason.description {
                                                        //有关闭的原因
                                                        ws.0.read().handler.on_close(reason.code, desc.as_str());
                                                    } else {
                                                        ws.0.read().handler.on_close(reason.code, "");
                                                    }
                                                },
                                                msg => {
                                                    //接收到其它消息
                                                    ws.0.read().handler.on_message(msg);
                                                },
                                            }

                                            //接收到一条消息，则继续异步接收剩余消息，并立即结束当前接收
                                            let ws_copy = ws.clone();
                                            if let Some(count) = require_count {
                                                ws.0.read().rt.spawn_timing(ws.0.read().rt.alloc(), async move {
                                                    //通过其它运行时投递继续接收的任务，避免独占Websocket连接的运行时
                                                    receive_message(task_id,
                                                                    arbiter_copy,
                                                                    ws_copy,
                                                                    Some(count - 1),
                                                                    received_count + 1,
                                                                    result,
                                                                    None);
                                                }, 10);
                                            } else {
                                                ws.0.read().rt.spawn_timing(ws.0.read().rt.alloc(), async move {
                                                    //通过其它运行时投递继续接收的任务，避免独占Websocket连接的运行时
                                                    receive_message(task_id,
                                                                    arbiter_copy,
                                                                    ws_copy,
                                                                    None,
                                                                    received_count + 1,
                                                                    result,
                                                                    None);
                                                }, 10);
                                            }
                                            return;
                                        },
                                        _ => {
                                            //接收消息帧失败，不应该进入此分支，则立即退出本次消息接收
                                            let url = ws.0.read().status.get_url().clone();
                                            let reason = format!("Websocket receive failed, url: {}, reason: unknow", url);
                                            let error = Error::new(ErrorKind::Other, reason.clone());
                                            *result.0.borrow_mut() = Some(Err(error)); //设置接收消息的结果
                                            ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                                            ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步接收消息的任务
                                            error!("{}", reason);
                                            return;
                                        },
                                    }
                                },
                            }
                        } else {
                            //有本次接收的最大消息数限制或无本次接收的最大消息限制，当连接流结束时立即退出本次消息接收
                            *result.0.borrow_mut() = Some(Ok(())); //设置接收消息的结果
                            ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步接收消息的任务
                            return;
                        }
                    }
                },
            }
        });
    }));
}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> AsyncWebsocketReceive<P> {
    //创建异步打开Websocket连接
    pub fn new(rt: AsyncRuntime<(), P>,
               task_id: TaskId,
               ws: AsyncWebsocket<P>,
               limit: Option<usize>) -> Self {
        AsyncWebsocketReceive {
            rt,
            task_id,
            ws,
            limit,
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        }
    }
}

//异步关闭Websocket连接
pub struct AsyncCloseWebsocket<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> {
    rt:         AsyncRuntime<(), P>,    //异步运行时
    task_id:    TaskId,                 //异步任务id
    ws:         AsyncWebsocket<P>,      //连接
    close_code: Option<CloseCode>,      //关闭状态码
    result:     AsyncWaitResult<()>,    //异步关闭连接的结果
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncCloseWebsocket<P> {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncCloseWebsocket<P> {}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> Future for AsyncCloseWebsocket<P> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.result.0.borrow_mut().take() {
            //消息已接收，则返回
            return Poll::Ready(result);
        }

        let close_code = self.close_code.take().unwrap();
        let task_id = self.task_id.clone();
        let arbiter = &self.ws.0.read().arbiter;
        let ws = self.ws.clone();
        let result = self.result.clone();

        arbiter.send(Box::pin(async move {
            Arbiter::spawn(async move {
                match ASYNC_WEBSOCKET_CONNECTION.try_with(move |shared| {
                    shared.clone()
                }) {
                    Err(e) => {
                        let url = ws.0.read().status.get_url().clone();
                        let reason = format!("Close websocket failed, url: {}, reason: {:?}", url, e);
                        let error = Error::new(ErrorKind::Other, reason.clone());
                        *result.0.borrow_mut() = Some(Err(error)); //设置关闭连接的结果
                        ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                        ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步关闭连接的任务
                        error!("{}", reason);
                    },
                    Ok(shared) => {
                        //当前客户端有连接，则立即关闭连接
                        if let Some(ws_con) = unsafe { (&mut *shared.get()) } {
                            if let Err(e) = ws_con.send(ws::Message::Close(Some(CloseReason::from(close_code.clone())))).await {
                                //发送关闭消息失败
                                let url = ws.0.read().status.get_url().clone();
                                let reason = format!("Close websocket failed, url: {}, reason: {:?}", url, e);
                                let error = Error::new(ErrorKind::Other, reason.clone());
                                *result.0.borrow_mut() = Some(Err(error)); //设置关闭连接的结果
                                ws.0.read().handler.on_error(reason.clone()); //通知处理器连接错误
                                ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步关闭连接的任务
                                error!("{}", reason);
                            } else {
                                //发送关闭消息成功
                                let url = ws.0.read().status.get_url().clone();
                                let protocols = ws.0.read().status.get_protocols().to_vec();
                                ws.0.write().status = AsyncWebsocketStatus::Closing(url, protocols, Some(close_code)); //设置连接状态
                                *result.0.borrow_mut() = Some(Ok(())); //设置关闭连接的结果
                                ws.0.read().rt.wakeup(&task_id); //唤醒外部运行时的异步关闭连接的任务
                            }
                        }
                    },
                }
            });
        }));

        //挂起关闭连接的异步任务
        self.ws.0.read().rt.pending(&self.task_id, cx.waker().clone())
    }
}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> AsyncCloseWebsocket<P> {
    //创建异步打开Websocket连接
    pub fn new(rt: AsyncRuntime<(), P>,
               task_id: TaskId,
               ws: AsyncWebsocket<P>,
               close_code: Option<CloseCode>) -> Self {
        AsyncCloseWebsocket {
            rt,
            task_id,
            ws,
            close_code,
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        }
    }
}

/*
* 异步Websocket消息
*/
pub type AsyncWebsocketMessage = Message;

/*
* 异步Websocket关闭状态码
*/
pub type AsyncWebsocketCloseCode = CloseCode;

/*
* 异步Websocket连接处理器
*/
#[derive(Clone)]
pub struct AsyncWebsocketHandler(Arc<RwLock<InnerHandler>>);

unsafe impl Send for AsyncWebsocketHandler {}
unsafe impl Sync for AsyncWebsocketHandler {}

impl Default for AsyncWebsocketHandler {
    fn default() -> Self {
        let inner = InnerHandler {
            on_open_handler: None,
            on_message_handler: None,
            on_close_handler: None,
            on_error_handler: None,
        };

        AsyncWebsocketHandler(Arc::new(RwLock::new(inner)))
    }
}

impl AsyncWebsocketHandler {
    //设置握手成功的回调函数
    pub fn set_on_open(&self, callback: Arc<dyn Fn()>) {
        self.0.write().on_open_handler = Some(callback);
    }

    //设置接收到消息的回调函数
    pub fn set_on_message(&self, callback: Arc<dyn Fn(AsyncWebsocketMessage)>) {
        self.0.write().on_message_handler = Some(callback);
    }

    //设置开始关闭的回调函数
    pub fn set_on_close(&self, callback: Arc<dyn Fn(u16, String)>) {
        self.0.write().on_close_handler = Some(callback);
    }

    //设置错误的回调函数
    pub fn set_on_error(&self, callback: Arc<dyn Fn(String)>) {
        self.0.write().on_error_handler = Some(callback);
    }

    //握手成功的回调
    pub fn on_open(&self) {
        if let Some(callback) = &self.0.read().on_open_handler {
            callback();
        }
    }

    //接收到消息的回调
    fn on_message(&self, msg: Message) {
        if let Some(callback) = &self.0.read().on_message_handler {
            callback(msg);
        }
    }

    //连接关闭的回调
    fn on_close(&self, code: CloseCode, reason: &str) {
        if let Some(callback) = &self.0.read().on_close_handler {
            callback(code.into(), reason.to_string());
        }
    }

    //错误的回调
    fn on_error(&self, e: String) {
        if let Some(callback) = &self.0.read().on_error_handler {
            callback(e);
        }
    }
}

//内部连接处理器
struct InnerHandler {
    on_open_handler:    Option<Arc<dyn Fn()>>,                      //握手成功的回调函数
    on_message_handler: Option<Arc<dyn Fn(AsyncWebsocketMessage)>>, //接收到消息的回调函数
    on_close_handler:   Option<Arc<dyn Fn(u16, String)>>,           //关闭的回调函数
    on_error_handler:   Option<Arc<dyn Fn(String)>>,                //错误的回调函数
}