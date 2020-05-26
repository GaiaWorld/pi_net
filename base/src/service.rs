use std::sync::Arc;
use std::result::Result as GenResult;
use std::io::{Error, ErrorKind, Result};

use log::warn;

use mqtt::broker::{MqttBrokerListener, MqttBrokerService};
use mqtt::session::{MqttSession, MqttConnect};
use mqtt::util::{AsyncResult, BrokerSession};

use crate::connect::{BaseInnerListener, BaseInnerService, decode, BaseConnect};

/*
* 基础协议监听器
*/
pub struct BaseListener {
    inner:  Arc<dyn BaseInnerListener>,    //上层监听器
}

unsafe impl Send for BaseListener {}

impl MqttBrokerListener for BaseListener {
    fn connected(&self, connect: Arc<dyn MqttConnect>) -> AsyncResult {
        //Mqtt已连接，则初始化基础协议的连接，保存在Mqtt连接的上下文，并继续通知上层协议
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                let mut base_connect = BaseConnect::new(connect.clone());
                let result = self.inner.connected(&mut base_connect);
                session.get_context_mut().set::<BaseConnect>(base_connect);
                return AsyncResult::with(result);
            }
        }

        AsyncResult::with(Err(Error::new(ErrorKind::Other, format!("base connect error, connect: {:?}, reason: init base service context failed", connect))))
    }

    fn closed(&self, connect: Arc<dyn MqttConnect>, mut context: BrokerSession, reason: Result<()>) {
        //连接已关闭，立即释放Mqtt会话的上下文，并继续通知上层协议
        match context.get_context_mut().remove::<BaseConnect>() {
            Err(e) => {
                warn!("!!!> Free Context Failed of Base Connect Close, reason: {:?}", e);
            },
            Ok(opt) => {
                if let Some(mut base_connect) = opt {
                    self.inner.closed(&mut base_connect, reason);
                }
            },
        }
    }
}

impl BaseListener {
    //构建指定上层监听器的基础协议监听器
    pub fn with_listener(listener: Arc<dyn BaseInnerListener>) -> Self {
        BaseListener {
            inner: listener,
        }
    }
}

/*
* 基础协议服务
*/
pub struct BaseService {
    inner:  Arc<dyn BaseInnerService>, //上层服务
}

unsafe impl Send for BaseService {}

impl MqttBrokerService for BaseService {
    fn subscribe(&self, connect: Arc<dyn MqttConnect>, topics: Vec<(String, u8)>) -> AsyncResult {
        AsyncResult::with(Ok(()))
    }

    fn unsubscribe(&self, connect: Arc<dyn MqttConnect>, topics: Vec<String>) -> Result<()> {
        Ok(())
    }

    fn publish(&self, connect: Arc<dyn MqttConnect>, topic: String, payload: Arc<Vec<u8>>) -> Result<()> {
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                if let Some(mut h) = session.get_context().get::<BaseConnect>() {
                    match decode(h.as_ref(), payload.as_ref()) {
                        Err(e) => {
                            //解码请求失败，则立即返回错误原因
                            return Err(Error::new(ErrorKind::Other, format!("base request error, connect: {:?}, reason: {:?}", connect, e)));
                        }
                        Ok(bin) => {
                            //解码请求成功，则继续上层协议的请求处理
                            let base_connect = h.as_ref();
                            return self.inner.request(base_connect, topic, bin);
                        },
                    }
                }
            }
        }

        Err(Error::new(ErrorKind::Other, format!("base request error, connect: {:?}, reason: invalid request", connect)))
    }
}

impl BaseService {
    //构建指定上层服务的基础协议服务
    pub fn with_service(service: Arc<dyn BaseInnerService>) -> Self {
        BaseService {
            inner: service,
        }
    }
}

