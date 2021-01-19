use std::sync::Arc;
use std::net::SocketAddr;
use std::result::Result as GenResult;
use std::io::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use log::warn;

use atom::Atom;
use gray::GrayVersion;
use handler::{Args, Handler};

use tcp::util::{ContextHandle, SocketContext};
use mqtt::server::MqttBrokerProtocol;
use mqtt::broker::{MQTT_RESPONSE_SYS_TOPIC, MqttBrokerListener, MqttBrokerService};
use mqtt::session::{MqttSession, MqttConnect};
use mqtt::util::{AsyncResult, BrokerSession};

/*
* Mqtt事件
*/
pub enum MqttEvent {
    Connect(usize, String, String, u16, bool, Option<String>, Option<String>, AsyncResult), //建立连接
    Disconnect(usize, String, String, Result<()>),                                          //关闭连接
    Sub(usize, String, String, Vec<(String, u8)>, AsyncResult),                             //订阅主题
    Unsub(usize, String, String, Vec<String>),                                              //退订主题
    Publish(usize, String, String, Option<SocketAddr>, String, Arc<Vec<u8>>),               //发布主题
}

/*
* Mqtt连接句柄
*/
pub struct MqttConnectHandle {
    gray:       AtomicIsize,            //灰度，负数代表无灰度
    client_id:  String,                 //Mqtt客户端id
    protocol:   MqttBrokerProtocol,     //Mqtt代理
    connect:    Arc<dyn MqttConnect>,   //Mqtt连接
    is_closed:  AtomicBool,             //Mqtt连接是否已关闭
}

unsafe impl Send for MqttConnectHandle {}
unsafe impl Sync for MqttConnectHandle {}

impl GrayVersion for MqttConnectHandle {
    fn get_gray(&self) -> &Option<usize> {
        let gray = self.gray.load(Ordering::Relaxed);
        if gray < 0 {
            return &None;
        }

        &None //TODO 修改GrayVersion后再实现...
    }

    fn set_gray(&mut self, gray: Option<usize>) {
        if let Some(n) = gray {
            self.gray.store(n as isize, Ordering::SeqCst);
        } else {
            self.gray.store(-1, Ordering::SeqCst);
        }
    }

    //获取连接唯一id
    fn get_id(&self) -> usize {
        if let Some(uid) = self.connect.get_uid() {
            return uid;
        }

        0
    }
}

impl MqttConnectHandle {
    //获取连接的连接令牌
    pub fn get_token(&self) -> Option<usize> {
        self.connect.get_token()
    }

    //获取连接的本地地址
    pub fn get_local_addr(&self) -> Option<SocketAddr> {
        if self.is_closed.load(Ordering::Relaxed) {
            return None;
        }

        self.connect.get_local_addr()
    }

    //获取连接的本地ip
    pub fn get_local_ip(&self) -> Option<String> {
        if self.is_closed.load(Ordering::Relaxed) {
            return None;
        }

        if let Some(addr) = self.get_local_addr() {
            return Some(addr.ip().to_string());
        }

        None
    }

    //获取连接的本地端口
    pub fn get_local_port(&self) -> Option<u16> {
        if self.is_closed.load(Ordering::Relaxed) {
            return None;
        }

        if let Some(addr) = self.get_local_addr() {
            return Some(addr.port());
        }

        None
    }

    //获取连接的对端地址
    pub fn get_remote_addr(&self) -> Option<SocketAddr> {
        if self.is_closed.load(Ordering::Relaxed) {
            return None;
        }

        self.connect.get_remote_addr()
    }

    //获取连接的对端ip
    pub fn get_remote_ip(&self) -> Option<String> {
        if self.is_closed.load(Ordering::Relaxed) {
            return None;
        }

        if let Some(addr) = self.get_remote_addr() {
            return Some(addr.ip().to_string());
        }

        None
    }

    //获取连接的对端端口
    pub fn get_remote_port(&self) -> Option<u16> {
        if self.is_closed.load(Ordering::Relaxed) {
            return None;
        }

        if let Some(addr) = self.get_remote_addr() {
            return Some(addr.port());
        }

        None
    }

    //判断是否是安全的Mqtt连接
    pub fn is_security(&self) -> bool {
        self.connect.is_security()
    }

    //判断是否是被动接收消息
    pub fn is_passive(&self) -> bool {
        self.connect.is_passive_receive()
    }

    //设置是否被动接收消息
    pub fn set_passive(&self, b: bool) {
        self.connect.passive_receive(b);
    }

    //唤醒连接
    pub fn wakeup(&self) -> Result<()> {
        self.connect.wakeup()
    }

    //为当前的Mqtt客户端订阅指定的主题
    pub fn sub(&self, topic: String) {
        if self.is_security() {
            //安全的会话
            if let MqttBrokerProtocol::WssMqtt311(broker) = &self.protocol {
                if let Some(session) = broker.get_broker().get_session(&self.client_id) {
                    //客户端的会话存在，则订阅主题
                    let _ = broker.get_broker().subscribe(session.clone(), topic);
                }
            }
        } else {
            //非安全的会主知
            if let MqttBrokerProtocol::WsMqtt311(broker) = &self.protocol {
                if let Some(session) = broker.get_broker().get_session(&self.client_id) {
                    //客户端的会话存在，则订阅主题
                    let _ = broker.get_broker().subscribe(session.clone(), topic);
                }
            }
        }
    }

    //为当前的Mqtt客户端退订指定的主题
    pub fn unsub(&self, topic: String) {
        if self.is_security() {
            //安全的会话
            if let MqttBrokerProtocol::WssMqtt311(broker) = &self.protocol {
                if let Some(session) = broker.get_broker().get_session(&self.client_id) {
                    //客户端的会话存在，则退订主题
                    let _ = broker.get_broker().unsubscribe(&session, topic);
                }
            }
        } else {
            //非安全的会主知
            if let MqttBrokerProtocol::WsMqtt311(broker) = &self.protocol {
                if let Some(session) = broker.get_broker().get_session(&self.client_id) {
                    //客户端的会话存在，则退订主题
                    let _ = broker.get_broker().unsubscribe(&session, topic);
                }
            }
        }
    }

    //获取连接会话上下文的只读引用
    pub fn get_session(&self) -> Option<ContextHandle<BrokerSession>> {
        if self.is_closed.load(Ordering::Relaxed) {
            return None;
        }

        self.connect.get_session()
    }

    //发送指定主题的数据
    pub fn send(&self, topic: &String, bin: Vec<u8>) {
        self.connect.send(topic, Arc::new(bin));
    }

    //回应指定请求
    pub fn reply(&self, bin: Vec<u8>) {
        self.send(&MQTT_RESPONSE_SYS_TOPIC, bin);
        if self.is_passive() {
            //是被动接收消息
            self.wakeup();
        }
    }

    //关闭当前连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        if self.is_closed.load(Ordering::Relaxed) {
            return Ok(());
        }

        self.connect.close(reason)
    }
}

/*
* Mqtt代理监听器
*/
pub struct MqttProxyListener {
    connect_handler:    Option<Arc<dyn Handler<
                            A = MqttEvent,
                            B = (),
                            C = (),
                            D = (),
                            E = (),
                            F = (),
                            G = (),
                            H = (),
                            HandleResult = ()>
                        >>,                                                 //连接异步处理器
}

unsafe impl Send for MqttProxyListener {}

impl MqttBrokerListener for MqttProxyListener {
    fn connected(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect>) -> AsyncResult {
        //Mqtt已连接
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                if let Some(handler) = &self.connect_handler {
                    let connect_handle = MqttConnectHandle {
                        gray: AtomicIsize::new(-1),
                        client_id: session.get_client_id().clone(),
                        protocol,
                        connect,
                        is_closed: AtomicBool::new(false),
                    };

                    //异步处理Mqtt连接
                    let result = AsyncResult::new();
                    let event = MqttEvent::Connect(connect_handle.get_id(),
                                                   connect_handle.protocol.get_broker_name().to_string(),
                                                   session.get_client_id().clone(),
                                                   session.get_keep_alive(),
                                                   session.is_clean_session(),
                                                   session.get_user().cloned(),
                                                   session.get_pwd().cloned(),
                                                   result.clone());
                    handler.handle(Arc::new(connect_handle), Atom::from(""), Args::OneArgs(event));

                    return result;
                }
            }
        }

        AsyncResult::with(Err(Error::new(ErrorKind::Other, format!("Mqtt proxy connect failed, connect: {:?}, reason: handle connect error", connect))))
    }

    fn closed(&self,
              protocol: MqttBrokerProtocol,
              connect: Arc<dyn MqttConnect>,
              mut context: BrokerSession,
              reason: Result<()>) {
        //Mqtt连接已关闭
        if let Err(e) = &reason {
            warn!("!!!> Mqtt proxy connect close by error, reason: {:?}", e);
        }

        if let Some(handler) = &self.connect_handler {
            let connect_handle = MqttConnectHandle {
                gray: AtomicIsize::new(-1),
                client_id: context.get_client_id().clone(),
                protocol,
                connect,
                is_closed: AtomicBool::new(true),
            };

            //异步处理Mqtt连接关闭
            let event = MqttEvent::Disconnect(connect_handle.get_id(),
                                              connect_handle.protocol.get_broker_name().to_string(),
                                              context.get_client_id().clone(),
                                              reason);
            handler.handle(Arc::new(connect_handle), Atom::from(""), Args::OneArgs(event));
        }
    }
}

impl MqttProxyListener {
    //构建Mqtt代理监听器
    pub fn new() -> Self {
        MqttProxyListener {
            connect_handler: None,
        }
    }

    //构建指定处理器的Mqtt代理监听器
    pub fn with_handler(connect_handler: Option<Arc<dyn Handler<
            A = MqttEvent,
            B = (),
            C = (),
            D = (),
            E = (),
            F = (),
            G = (),
            H = (),
            HandleResult = ()>>>) -> Self {
        MqttProxyListener {
            connect_handler,
        }
    }

    //设置Mqtt代理监听器的连接处理器
    pub fn set_connect_handler(&mut self,
                               handler: Option<Arc<dyn Handler<
                                A = MqttEvent,
                                B = (),
                                C = (),
                                D = (),
                                E = (),
                                F = (),
                                G = (),
                                H = (),
                                HandleResult = ()>>>) {
        self.connect_handler = handler;
    }
}

/*
* Mqtt代理服务
*/
pub struct MqttProxyService {
    request_handler:    Option<Arc<dyn Handler<
                            A = MqttEvent,
                            B = (),
                            C = (),
                            D = (),
                            E = (),
                            F = (),
                            G = (),
                            H = (),
                            HandleResult = ()>
                        >>,                                                 //请求服务异步处理器
}

unsafe impl Send for MqttProxyService {}

impl MqttBrokerService for MqttProxyService {
    fn subscribe(&self,
                 protocol: MqttBrokerProtocol,
                 connect: Arc<dyn MqttConnect>,
                 topics: Vec<(String, u8)>) -> AsyncResult {
        //Mqtt订阅主题
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                if let Some(handler) = &self.request_handler {
                    let connect_handle = MqttConnectHandle {
                        gray: AtomicIsize::new(-1),
                        client_id: session.get_client_id().clone(),
                        protocol,
                        connect,
                        is_closed: AtomicBool::new(false),
                    };

                    //异步处理Mqtt订阅主题
                    let result = AsyncResult::new();
                    let event = MqttEvent::Sub(connect_handle.get_id(),
                                               connect_handle.protocol.get_broker_name().to_string(),
                                               session.get_client_id().clone(),
                                               topics,
                                               result.clone());
                    handler.handle(Arc::new(connect_handle), Atom::from(""), Args::OneArgs(event));

                    return result;
                }
            }
        }

        AsyncResult::with(Err(Error::new(ErrorKind::Other, format!("Mqtt proxy subscribe failed, connect: {:?}, reason: handle subscribe error", connect))))
    }

    fn unsubscribe(&self,
                   protocol: MqttBrokerProtocol,
                   connect: Arc<dyn MqttConnect>,
                   topics: Vec<String>) -> Result<()> {
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                if let Some(handler) = &self.request_handler {
                    let connect_handle = MqttConnectHandle {
                        gray: AtomicIsize::new(-1),
                        client_id: session.get_client_id().clone(),
                        protocol,
                        connect,
                        is_closed: AtomicBool::new(false),
                    };

                    //异步处理Mqtt连接关闭
                    let event = MqttEvent::Unsub(connect_handle.get_id(),
                                                 connect_handle.protocol.get_broker_name().to_string(),
                                                 session.get_client_id().clone(),
                                                 topics);
                    handler.handle(Arc::new(connect_handle), Atom::from(""), Args::OneArgs(event));

                    return Ok(());
                }
            }
        }

        Err(Error::new(ErrorKind::Other, format!("Mqtt proxy request failed, connect: {:?}, reason: handle unsubscribe error", connect)))
    }

    fn publish(&self,
               protocol: MqttBrokerProtocol,
               connect: Arc<dyn MqttConnect>,
               topic: String,
               payload: Arc<Vec<u8>>) -> Result<()> {
        if let Some(mut handle) = connect.get_session() {
            if let Some(session) = handle.as_mut() {
                if let Some(handler) = &self.request_handler {
                    let connect_handle = MqttConnectHandle {
                        gray: AtomicIsize::new(-1),
                        client_id: session.get_client_id().clone(),
                        protocol,
                        connect,
                        is_closed: AtomicBool::new(false),
                    };

                    //异步处理Mqtt连接关闭
                    let event = MqttEvent::Publish(connect_handle.get_id(),
                                                   connect_handle.protocol.get_broker_name().to_string(),
                                                   session.get_client_id().clone(),
                                                   connect_handle.get_remote_addr(),
                                                   topic,
                                                   payload);
                    handler.handle(Arc::new(connect_handle), Atom::from(""), Args::OneArgs(event));

                    return Ok(());
                }
            }
        }

        Err(Error::new(ErrorKind::Other, format!("Mqtt proxy publish failed, connect: {:?}, reason: handle publish error", connect)))
    }
}

impl MqttProxyService {
    //构建Mqtt代理服务
    pub fn new() -> Self {
        MqttProxyService {
            request_handler: None,
        }
    }

    //构建指定处理器的Mqtt代理服务
    pub fn with_handler(request_handler: Option<Arc<dyn Handler<
            A = MqttEvent,
            B = (),
            C = (),
            D = (),
            E = (),
            F = (),
            G = (),
            H = (),
            HandleResult = ()>>>) -> Self {
        MqttProxyService {
            request_handler,
        }
    }

    //设置Mqtt代理监听器的请求处理器
    pub fn set_handler(&mut self,
                       handler: Option<Arc<dyn Handler<
                       A = MqttEvent,
                       B = (),
                       C = (),
                       D = (),
                       E = (),
                       F = (),
                       G = (),
                       H = (),
                       HandleResult = ()>>>) {
        self.request_handler = handler;
    }
}

