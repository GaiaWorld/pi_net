use std::sync::Arc;
use std::net::SocketAddr;
use std::result::Result as GenResult;
use std::io::{Error, ErrorKind, Result};

use log::warn;

use atom::Atom;
use gray::GrayVersion;
use handler::{Args, Handler};

use mqtt::session::MqttConnect;
use mqtt::util::BrokerSession;
use base::connect::{BaseInnerListener, BaseInnerService, BaseConnect};

use crate::connect::{decode, RpcConnect};

/*
* 已连接事件名
*/
pub const CONNECTED_EVENT_NAME: &'static str = "rpc_net_connect";

/*
* 已关闭事件名
*/
pub const CLOSED_EVENT_NAME: &'static str = "rpc_net_connect_close";

/*
* Rpc监听器
*/
pub struct RpcListener {
    connected_handler:  Option<Arc<dyn Handler<
        A = usize,
        B = (),
        C = (),
        D = (),
        E = (),
        F = (),
        G = (),
        H = (),
        HandleResult = GenResult<(), String>>
    >>,                              //已连接异步处理器
    closed_handler:     Option<Arc<dyn Handler<
        A = usize,
        B = (),
        C = (),
        D = (),
        E = (),
        F = (),
        G = (),
        H = (),
        HandleResult = GenResult<(), String>>
    >>,                              //已关闭异步处理器
}

unsafe impl Send for RpcListener {}

impl BaseInnerListener for RpcListener {
    fn connected(&self, connect: &mut BaseConnect) -> Result<()> {
        //基础协议已连接，则初始化Rpc的连接，并保存在基础协议连接的上下文
        let rpc_connect = Arc::new(RpcConnect::new(connect.get_handle()));
        if connect.get_context_mut().set::<Arc<RpcConnect>>(rpc_connect.clone()) {
            //异步处理Rpc已连接
            if let Some(handler) = &self.connected_handler {
                let connect_uid = rpc_connect.get_id();
                handler.handle(rpc_connect, Atom::from(CONNECTED_EVENT_NAME), Args::OneArgs(connect_uid));
                return Ok(());
            }
        }

        Err(Error::new(ErrorKind::Other, format!("rpc connect error, connect: {:?}, reason: init rpc service context failed", connect.get_connect())))
    }

    fn closed(&self, connect: &mut BaseConnect, reason: Result<()>) {
        //连接已关闭
        if let Err(e) = reason {
            warn!("!!!> Rpc Connect Close by Error, reason: {:?}", e);
        }

        //立即释放基础协议连接的上下文
        match connect.get_context_mut().remove::<Arc<RpcConnect>>() {
            Err(e) => {
                warn!("!!!> Free Context Failed of Rpc Close, reason: {:?}", e);
            },
            Ok(opt) => {
                if let Some(rpc_connect) = opt {
                    //异步处理Rpc连接关闭
                    if let Some(handler) = &self.closed_handler {
                        let connect_uid = rpc_connect.get_id();
                        handler.handle(rpc_connect, Atom::from(CLOSED_EVENT_NAME), Args::OneArgs(connect_uid));
                    }
                }
            },
        }
    }
}

impl RpcListener {
    //构建Rpc监听器
    pub fn new() -> Self {
        RpcListener {
            connected_handler: None,
            closed_handler: None,
        }
    }

    //构建指定处理器的Rpc监听器
    pub fn with_handler(connected_handler: Arc<dyn Handler<
                            A = usize,
                            B = (),
                            C = (),
                            D = (),
                            E = (),
                            F = (),
                            G = (),
                            H = (),
                            HandleResult = GenResult<(), String>>
                        >,
                        closed_handler: Arc<dyn Handler<
                            A = usize,
                            B = (),
                            C = (),
                            D = (),
                            E = (),
                            F = (),
                            G = (),
                            H = (),
                            HandleResult = GenResult<(), String>>
                        >) -> Self {
        RpcListener {
            connected_handler: Some(connected_handler),
            closed_handler: Some(closed_handler),
        }
    }

    //设置Rpc监听器的已连接事件处理器
    pub fn set_connected_handler(&mut self, handler: Arc<dyn Handler<
        A = usize,
        B = (),
        C = (),
        D = (),
        E = (),
        F = (),
        G = (),
        H = (),
        HandleResult = GenResult<(), String>>>) {
        self.connected_handler = Some(handler);
    }

    //设置Rpc监听器的已关闭事件处理器
    pub fn set_closed_handler(&mut self, handler: Arc<dyn Handler<
        A = usize,
        B = (),
        C = (),
        D = (),
        E = (),
        F = (),
        G = (),
        H = (),
        HandleResult = GenResult<(), String>>>) {
        self.closed_handler = Some(handler);
    }
}

/*
* Rpc服务
*/
pub struct RpcService {
    request_handler:    Option<Arc<dyn Handler<
        A = u8,
        B = Option<SocketAddr>,
        C = u32,
        D = Arc<Vec<u8>>,
        E = (),
        F = (),
        G = (),
        H = (),
        HandleResult = ()>
    >>,                              //请求服务异步处理器
}

unsafe impl Send for RpcService {}

impl BaseInnerService for RpcService {
    fn request(&self, connect: &BaseConnect, topic: String, payload: Vec<u8>) -> Result<()> {
        if let Some(h) = connect.get_context().get::<Arc<RpcConnect>>() {
            match decode(h.as_ref().as_ref(), &payload[..]) {
                Err(e) => {
                    //解码请求失败，则立即返回错误原因
                    return Err(Error::new(ErrorKind::Other, format!("rpc request error, connect: {:?}, reason: {:?}", connect.get_connect(), e)));
                }
                Ok((rid, bin)) => {
                    //解码请求成功，则异步处理请求
                    let rpc_connect = h.as_ref();
                    if let Some(handler) = &self.request_handler {
                        handler.handle(rpc_connect.clone(),
                                       Atom::from(topic),
                                       Args::FourArgs(0, rpc_connect.get_remote_addr(), rid, Arc::new(bin)));
                        return Ok(());
                    }
                },
            }
        }

        Err(Error::new(ErrorKind::Other, format!("rpc connect error, connect: {:?}, reason: invalid rpc request", connect.get_connect())))
    }
}

impl RpcService {
    //构建Rpc服务
    pub fn new() -> Self {
        RpcService {
            request_handler: None,
        }
    }

    //构建指定处理器的Rpc服务
    pub fn with_handler(request_handler: Arc<dyn Handler<
                   A = u8,
                   B = Option<SocketAddr>,
                   C = u32,
                   D = Arc<Vec<u8>>,
                   E = (),
                   F = (),
                   G = (),
                   H = (),
                   HandleResult = ()>
               >) -> Self {
        RpcService {
            request_handler: Some(request_handler),
        }
    }

    //设置Rpc的请求服务处理器
    pub fn set_request_handler(&mut self, handler: Arc<dyn Handler<
        A = u8,
        B = Option<SocketAddr>,
        C = u32,
        D = Arc<Vec<u8>>,
        E = (),
        F = (),
        G = (),
        H = (),
        HandleResult = ()>>) {
        self.request_handler = Some(handler);
    }
}
