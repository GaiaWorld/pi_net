#[macro_use]
extern crate lazy_static;

use std::thread;
use std::pin::Pin;
use std::sync::Arc;
use std::str::FromStr;
use std::future::Future;
use std::net::SocketAddr;
use std::time::{Instant, Duration};
use std::task::{Context, Poll, Waker};
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::{FutureExt, BoxFuture};
use ws::{Builder, WebSocket, Settings, Factory, Request, Handshake, Handler, Sender, Message, Result as WsResult, Error as WsError, CloseCode};
use url::Url;
use parking_lot::RwLock;

use r#async::rt::{TaskId, AsyncTaskPool, AsyncTaskPoolExt, AsyncRuntime};

/*
* 异步Websocket连接构建器
*/
pub struct AsyncWebsocketBuilder {
    configure:  Settings,   //连接配置
    builder:    Builder,    //构建器
}

impl AsyncWebsocketBuilder {
    //构建异步Websocket连接构建器
    pub fn new() -> Self {
        let mut configure = Settings::default();
        configure.panic_on_internal = false; //不允许内部错误抛出异常
        configure.method_strict = true; //允许Websocket握手时必须使用Get方法

        AsyncWebsocketBuilder {
            configure,
            builder: Builder::new(),
        }
    }

    //是否允许严格的连接掩码处理，默认不允许
    pub fn enable_strict_client_masking(mut self, b: bool) -> Self {
        self.configure.masking_strict = b;
        self
    }

    //是否允许严格的验证服务器的密钥，默认不允许
    pub fn enable_strict_server_key(mut self, b: bool) -> Self {
        self.configure.key_strict = b;
        self
    }

    //设置连接事件队列容量，默认为5
    pub fn set_queue_size(mut self, size: usize) -> Self {
        if size * self.configure.max_connections > usize::MAX {
            //连接队列容量过大，则立即抛出异常
            panic!("Set queue size failed, size: {}, reason: out of queue size");
        }

        self.configure.queue_size = size;
        self
    }

    //设置初始的帧缓冲大小，默认为10
    pub fn set_frame_buffer_size(mut self, size: usize) -> Self {
        self.configure.fragments_capacity = size;
        self
    }

    //是否允许帧缓冲扩容，如果不允许，则在帧缓冲溢出时抛出错误，默认允许
    pub fn enable_expansion_frame_buffer(mut self, b: bool) -> Self {
        self.configure.fragments_grow = b;
        self
    }

    //设置单个发送帧的最大长度，超过长度的消息将被自动分帧，单位字节，默认64KB
    pub fn set_send_frame_limit(mut self, size: usize) -> Self {
        self.configure.fragment_size = size;
        self
    }

    //设置单个接收帧的最大长度，超过长度将拒绝接收，默认无限制
    pub fn set_receive_frame_limit(mut self, size: usize) -> Self {
        self.configure.max_fragment_size = size;
        self
    }

    //设置初始的接收缓冲区的大小，单位字节，默认2KB
    pub fn set_receive_buffer_size(mut self, size: usize) -> Self {
        self.configure.in_buffer_capacity = size;
        self
    }

    //是否允许接收缓冲区扩容，如果不允许，则在接收缓冲区溢出时抛出错误，默认允许
    pub fn enable_expansion_receive_buffer(mut self, b: bool) -> Self {
        self.configure.in_buffer_grow = b;
        self
    }

    //设置初始的发送缓冲的大小，单位字节，默认2KB
    pub fn set_send_buffer_size(mut self, size: usize) -> Self {
        self.configure.out_buffer_capacity = size;
        self
    }

    //是否允许发送缓冲区扩容，如果不允许，则在发送缓冲区溢出时抛出错误，默认允许
    pub fn enable_expansion_send_buffer(mut self, b: bool) -> Self {
        self.configure.out_buffer_grow = b;
        self
    }

    //是否允许立即刷新发送或接收缓冲区，不允许则会在200ms后刷新缓冲区，默认不允许
    pub fn enable_nodelay(mut self, b: bool) -> Self {
        self.configure.tcp_nodelay = b;
        self
    }

    //构建异步Websocket连接
    pub fn build<
        P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>
    >(mut self) -> Result<AsyncWebsocket<P>> {
        let configure = self.configure;
        let ws = AsyncWebsocket::new(AsyncWebsocketHandler::default());

        match self.builder.with_settings(configure).build(AsyncWebsocketHandlerFactory(ws.clone())) {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Build async websocket client failed, reason: {:?}", e)))
            },
            Ok(connect) => {
                //构建异步Websocket连接成功，则绑定并返回
                ws.bind(connect);
                Ok(ws)
            },
        }
    }
}

/*
* 异步Websocket连接
*/
pub struct AsyncWebsocket<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>,
>(Arc<RwLock<InnerWebsocket<P>>>)
    where AsyncWebsocketHandlerFactory<P>: Factory;

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncWebsocket<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncWebsocket<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
> Clone for AsyncWebsocket<P> {
    fn clone(&self) -> Self {
        AsyncWebsocket(self.0.clone())
    }
}

/*
* 异步Websocket连接同步方法
*/
impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
> AsyncWebsocket<P> {
    //构建异步Websocket连接
    fn new(handler: AsyncWebsocketHandler) -> Self {
        let inner = InnerWebsocket {
            rt: None,
            task_id: None,
            handler,
            connect: None,
            sender: None,
        };

        AsyncWebsocket(Arc::new(RwLock::new(inner)))
    }

    //设置异步运行时
    pub fn set_runtime(&self, rt: AsyncRuntime<(), P>) {
        self.0.write().rt = Some(rt);
    }

    //设置异步任务id
    pub fn set_task_id(&self, task_id: TaskId) {
        self.0.write().task_id = Some(task_id);
    }

    //绑定异步Websocket连接
    fn bind(&self, connect: WebSocket<AsyncWebsocketHandlerFactory<P>>) {
        self.0.write().connect = Some(connect);
    }

    //获取异步Websocket连接的处理器
    pub fn get_handler(&self) -> AsyncWebsocketHandler {
        self.0.read().handler.clone()
    }
}

/*
* 异步Websocket连接异步方法
*/
impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>
> AsyncWebsocket<P> {
    //打开连接
    pub async fn open(&self,
                      url: &str,
                      timeout: u64) -> Result<AsyncWebsocketSender> {
        match Url::from_str(url) {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Open websocket failed, url: {}, reason: {:?}", url, e)))
            },
            Ok(url) => {
                let rt = self.0.read().rt.as_ref().unwrap().clone();
                let task_id = self.0.read().task_id.as_ref().unwrap().clone();

                match rt.clone() {
                    AsyncRuntime::Local(r) => {
                        r.clone().wait_any(vec![
                            (rt.clone(),
                             AsyncOpenWebsocket::new(rt.clone(),
                                                     task_id,
                                                     self.clone(),
                                                     url.clone()).boxed()),
                            (rt.clone(),
                             async move {
                                 r.wait_timeout(timeout as usize).await;
                                 Err(Error::new(ErrorKind::TimedOut, format!("Open websocket failed, url: {:?}, reason: connect timeout", url)))
                             }.boxed())]).await
                    },
                    AsyncRuntime::Multi(r) => {
                        r.clone().wait_any(vec![
                            (rt.clone(),
                             AsyncOpenWebsocket::new(rt.clone(),
                                                     task_id,
                                                     self.clone(),
                                                     url.clone()).boxed()),
                            (rt.clone(),
                             async move {
                                 r.wait_timeout(timeout as usize).await;
                                 Err(Error::new(ErrorKind::TimedOut, format!("Open websocket failed, url: {:?}, reason: connect timeout", url)))
                             }.boxed())]).await
                    },
                    AsyncRuntime::Worker(_, _, r) => {
                        r.clone().wait_any(vec![
                            (rt.clone(),
                             AsyncOpenWebsocket::new(rt.clone(),
                                                     task_id,
                                                     self.clone(),
                                                     url.clone()).boxed()),
                            (rt.clone(),
                             async move {
                                 r.wait_timeout(timeout as usize).await;
                                 Err(Error::new(ErrorKind::TimedOut, format!("Open websocket failed, url: {:?}, reason: connect timeout", url)))
                             }.boxed())]).await
                    },
                }
            },
        }
    }
}

//内部Websocket连接
struct InnerWebsocket<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>>
    where AsyncWebsocketHandlerFactory<P>: Factory {
    rt:         Option<AsyncRuntime<(), P>>,                        //异步运行时
    task_id:    Option<TaskId>,                                     //异步任务id
    handler:    AsyncWebsocketHandler,                              //处理器
    connect:    Option<WebSocket<AsyncWebsocketHandlerFactory<P>>>, //连接
    sender:     Option<Sender>,                                     //发送器
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for InnerWebsocket<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for InnerWebsocket<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {}

//异步打开Websocket连接
pub struct AsyncOpenWebsocket<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>>
    where AsyncWebsocketHandlerFactory<P>: Factory {
    rt:         AsyncRuntime<(), P>,    //异步运行时
    task_id:    TaskId,                 //异步任务id
    ws:         AsyncWebsocket<P>,      //异步Websocket连接
    url:        Url,                    //url
}

unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Send for AsyncOpenWebsocket<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {}
unsafe impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> Sync for AsyncOpenWebsocket<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>> Future for AsyncOpenWebsocket<P> {
    type Output = Result<Sender>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(sender) = self.ws.0.write().sender.take() {
            //已打开指定Websocket连接，则返回
            return Poll::Ready(Ok(sender));
        }

        if let Some(mut connect) = self.ws.0.write().connect.take() {
            //连接存在，则打开
            let url = self.url.clone();
            thread::spawn(move || {
                if let Err(e) = connect.connect(url.clone()) {
                    panic!("Open websocket failed, url: {:?}, reason: {:?}", url, e);
                }
                let _ = connect.run();
            });
        }

        match &self.rt {
            AsyncRuntime::Local(rt) => rt.pending(&self.task_id, cx.waker().clone()),
            AsyncRuntime::Multi(rt) => rt.pending(&self.task_id, cx.waker().clone()),
            AsyncRuntime::Worker(_, _, rt) => rt.pending(&self.task_id, cx.waker().clone()),
        }
    }
}

impl<P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>> AsyncOpenWebsocket<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {
    //创建异步打开Websocket连接
    pub fn new(rt: AsyncRuntime<(), P>,
               task_id: TaskId,
               ws: AsyncWebsocket<P>,
               url: Url) -> Self {
        AsyncOpenWebsocket {
            rt,
            task_id,
            ws,
            url,
        }
    }
}

//异步Websocket连接处理器工厂
pub struct AsyncWebsocketHandlerFactory<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>,
>(AsyncWebsocket<P>)
    where AsyncWebsocketHandlerFactory<P>: Factory;

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<()>,
> Send for AsyncWebsocketHandlerFactory<P>
    where AsyncWebsocketHandlerFactory<P>: Factory {}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
> Factory for AsyncWebsocketHandlerFactory<P> {
    type Handler = AsyncWebsocketHandler;

    fn connection_made(&mut self, sender: Sender) -> Self::Handler {
        //正在建立Tcp连接，则设置异步Websocket连接的发送器，并通知打开Tcp连接完成
        (self.0).0.write().sender = Some(sender); //
        let rt = (self.0).0.read().rt.as_ref().unwrap().clone();
        match rt {
            AsyncRuntime::Local(rt) => rt.wakeup((self.0).0.read().task_id.as_ref().unwrap()),
            AsyncRuntime::Multi(rt) => rt.wakeup((self.0).0.read().task_id.as_ref().unwrap()),
            AsyncRuntime::Worker(_, _, rt) => rt.wakeup((self.0).0.read().task_id.as_ref().unwrap()),
        }

        //返回异步Websocket连接上的处理器
        (self.0).0.read().handler.clone()
    }
}

/*
* 异步Websokcet发送器
*/
pub type AsyncWebsocketSender = Sender;

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
            protocols: vec![],
            on_open: None,
            on_message: None,
            on_close: None,
            on_error: None,
        };

        AsyncWebsocketHandler(Arc::new(RwLock::new(inner)))
    }
}

impl Handler for AsyncWebsocketHandler {
    //重载握手成功的回调
    fn on_open(&mut self, shake: Handshake) -> WsResult<()> {
        if let Some(callback) = &self.0.read().on_open {
            callback(shake.peer_addr, shake.local_addr);
        }

        Ok(())
    }

    //重载接收到消息的回调
    fn on_message(&mut self, msg: Message) -> WsResult<()> {
        if let Some(callback) = &self.0.read().on_message {
            callback(msg);
        }

        Ok(())
    }

    //重载请求连接关闭的回调
    fn on_close(&mut self, code: CloseCode, reason: &str) {
        if let Some(callback) = &self.0.read().on_close {
            callback(code.into(), reason.to_string());
        }
    }

    //重载错误的回调
    fn on_error(&mut self, err: WsError) {
        if let Some(callback) = &self.0.read().on_error {
            callback(format!("{}", err));
        }
    }

    //重载连接握手的回调
    fn build_request(&mut self, url: &Url) -> WsResult<Request> {
        let mut req = Request::from_url(url)?;
        for protocol in &self.0.read().protocols {
            req.add_protocol(protocol);
        }

        Ok(req)
    }
}

impl AsyncWebsocketHandler {
    //增加子协议
    pub fn add_protocol(&self, protocol: &str) {
        self.0.write().protocols.push(protocol.to_string());
    }

    //设置握手成功的回调函数
    pub fn set_on_open(&self, callback: Arc<dyn Fn(Option<SocketAddr>, Option<SocketAddr>)>) {
        self.0.write().on_open = Some(callback);
    }

    //设置接收到消息的回调函数
    pub fn set_on_message(&self, callback: Arc<dyn Fn(AsyncWebsocketMessage)>) {
        self.0.write().on_message = Some(callback);
    }

    //设置开始关闭的回调函数
    pub fn set_on_close(&self, callback: Arc<dyn Fn(u16, String)>) {
        self.0.write().on_close = Some(callback);
    }

    //设置错误的回调函数
    pub fn set_on_error(&self, callback: Arc<dyn Fn(String)>) {
        self.0.write().on_error = Some(callback);
    }
}

//内部连接处理器
struct InnerHandler {
    protocols:      Vec<String>,                                                    //子协议列表
    on_open:        Option<Arc<dyn Fn(Option<SocketAddr>, Option<SocketAddr>)>>,    //握手成功的回调函数
    on_message:     Option<Arc<dyn Fn(AsyncWebsocketMessage)>>,                     //接收到消息的回调函数
    on_close:       Option<Arc<dyn Fn(u16, String)>>,                               //关闭的回调函数
    on_error:       Option<Arc<dyn Fn(String)>>,                                    //错误的回调函数
}
