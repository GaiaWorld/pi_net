use std::sync::Arc;
use std::net::SocketAddr;
use std::result::Result as GenResult;
use std::io::{Error, ErrorKind, Result};

use atom::Atom;
use gray::GrayVersion;
use handler::{Args, Handler};

use mqtt::{broker::MqttBrokerService, session::MqttConnect, util::BrokerSession};

use crate::connect::{decode, encode, RpcConnect};

/*
* 已连接事件名
*/
pub const CONNECTED_EVENT_NAME: &'static str = "net_connect";

/*
* 已关闭事件名
*/
pub const CLOSED_EVENT_NAME: &'static str = "net_connect_close";

/*
* Rpc服务
*/
pub struct RpcService {
    connected_handler:  Arc<
        dyn Handler<
            A = usize,
            B = (),
            C = (),
            D = (),
            E = (),
            F = (),
            G = (),
            H = (),
            HandleResult = GenResult<(), String>>
    >,                              //已连接异步处理器
    request_handler:    Arc<
        dyn Handler<
            A = u8,
            B = Option<SocketAddr>,
            C = Arc<Vec<u8>>,
            D = (),
            E = (),
            F = (),
            G = (),
            H = (),
            HandleResult = ()>
    >,                              //请求服务异步处理器
    closed_handler:     Arc<
        dyn Handler<
            A = usize,
            B = (),
            C = (),
            D = (),
            E = (),
            F = (),
            G = (),
            H = (),
            HandleResult = GenResult<(), String>>
    >,                              //已关闭异步处理器
}

unsafe impl Send for RpcService {}

impl MqttBrokerService for RpcService {
    fn connected(&self, connect: Arc<dyn MqttConnect>) -> Result<()> {
        //Mqtt已连接，则初始化Rpc服务的连接，并保存在Mqtt连接的上下文
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                let rpc_connect = Arc::new(RpcConnect::new(connect.clone()));
                if session.get_context_mut().set::<Arc<RpcConnect>>(rpc_connect.clone()) {
                    //异步处理Rpc已连接
                    let connect_uid = rpc_connect.get_id();
                    self.connected_handler.handle(rpc_connect, Atom::from(CONNECTED_EVENT_NAME), Args::OneArgs(connect_uid));
                    return Ok(());
                }
            }
        }

        Err(Error::new(ErrorKind::Other, format!("rpc connect error, connect: {:?}, reason: init rpc service context failed", connect)))
    }

    fn request(&self, connect: Arc<dyn MqttConnect>, topic: String, payload: Arc<Vec<u8>>) -> Result<()> {
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                if let Some(mut h) = session.get_context().get::<Arc<RpcConnect>>() {
                    match decode(h.as_ref().as_ref(), payload.as_ref()) {
                        Err(e) => {
                            //解码请求失败，则立即返回错误原因
                            return Err(Error::new(ErrorKind::Other, format!("rpc request error, connect: {:?}, reason: {:?}", connect, e)));
                        }
                        Ok(bin) => {
                            //解码请求成功，则异步处理请求
                            let rpc_connect = h.as_ref();
                            self.request_handler.handle(rpc_connect.clone(),
                                                        Atom::from(topic),
                                                        Args::ThreeArgs(0, rpc_connect.get_remote_addr(), Arc::new(bin)));
                            return Ok(());
                        },
                    }
                }
            }
        }

        Err(Error::new(ErrorKind::Other, format!("rpc connect error, connect: {:?}, reason: invalid rpc request", connect)))
    }

    fn closed(&self, connect: Arc<dyn MqttConnect>, mut context: BrokerSession, reason: Result<()>) {
        //连接已关闭，则立即释放Mqtt会话的上下文
        match context.get_context_mut().remove::<Arc<RpcConnect>>() {
            Err(e) => {
                println!("!!!> Free Context Failed of Rpc Close, reason: {:?}", e);
            },
            Ok(opt) => {
                if let Some(rpc_connect) = opt {
                    //异步处理Rpc连接关闭
                    let connect_uid = rpc_connect.get_id();
                    self.connected_handler.handle(rpc_connect, Atom::from(CLOSED_EVENT_NAME), Args::OneArgs(connect_uid));
                }
            },
        }
    }
}

impl RpcService {
    //构建Rpc服务
    pub fn new(connected_handler: Arc<
                   dyn Handler<
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
               request_handler: Arc<
                   dyn Handler<
                       A = u8,
                       B = Option<SocketAddr>,
                       C = Arc<Vec<u8>>,
                       D = (),
                       E = (),
                       F = (),
                       G = (),
                       H = (),
                       HandleResult = ()>
               >,
               closed_handler: Arc<
                   dyn Handler<
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
        RpcService {
            connected_handler,
            request_handler,
            closed_handler,
        }
    }
}
