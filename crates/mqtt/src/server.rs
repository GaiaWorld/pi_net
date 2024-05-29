use std::sync::Arc;
use std::io::{Error, Result, ErrorKind};

use mqtt311::{QoS, Packet, Publish};
use dashmap::{DashMap,
              mapref::entry::Entry};

use tcp::{Socket,
          connect::TcpSocket,
          tls_connect::TlsSocket};
use ws::{connect::WsSocket,
         utils::ChildProtocol};
use quic::{SocketHandle, AsyncService};

use crate::{v311::{self, WsMqtt311},
            tls_v311::{self, WssMqtt311},
            broker::{MqttBrokerListener, MqttBrokerService},
            session::MqttSession,
            quic_v311::{self, QuicMqtt311},
            quic_broker::{MqttBrokerListener as QuicMqttBrokerListener, MqttBrokerService as QuicMqttBrokerService},
            quic_session::MqttSession as QuicMqttSession};

///
/// Mqtt代理表和代理映射表
///
lazy_static! {
    static ref MQTT_BROKERS: DashMap<String, MqttBrokerProtocol> = DashMap::default();
    static ref MQTT_BROKERS_MAP: DashMap<u16, String> = DashMap::default();
}

///
/// Mqtt的Quic代理表和代理映射表
///
lazy_static! {
    static ref QUIC_MQTT_BROKERS: DashMap<String, MqttBrokerProtocol> = DashMap::default();
    static ref QUIC_MQTT_BROKERS_MAP: DashMap<u16, String> = DashMap::default();
}

///
/// 获取指定端口的Mqtt代理名称
///
pub fn get_broker_name(port: u16) -> Option<String> {
    if let Some(item) = MQTT_BROKERS_MAP.get(&port) {
        Some(item.value().clone())
    } else {
        None
    }
}

///
/// 获取指定端口的Mqtt的Quic代理名称
///
pub fn get_quic_broker_name(port: u16) -> Option<String> {
    if let Some(item) = QUIC_MQTT_BROKERS_MAP.get(&port) {
        Some(item.value().clone())
    } else {
        None
    }
}

/// 获取指定名称的Mqtt代理
pub fn get_broker(broker_name: &String) -> Option<MqttBrokerProtocol> {
    if let Some(item) = MQTT_BROKERS.get(broker_name) {
        Some(item.value().clone())
    } else {
        None
    }
}

/// 获取所有的Mqtt代理
pub fn all_broker() -> Vec<(String, MqttBrokerProtocol)> {
    let mut brokers = Vec::with_capacity(MQTT_BROKERS.len());
    for broker in MQTT_BROKERS.iter() {
        brokers.push((broker.key().clone(),
                      broker.value().clone()));
    }
    brokers
}

/// 获取指定名称的Quic Mqtt代理
pub fn get_quic_broker(broker_name: &String) -> Option<MqttBrokerProtocol> {
    if let Some(item) = QUIC_MQTT_BROKERS.get(broker_name) {
        Some(item.value().clone())
    } else {
        None
    }
}

/// 获取所有的Quic Mqtt代理
pub fn all_quic_broker() -> Vec<(String, MqttBrokerProtocol)> {
    let mut brokers = Vec::with_capacity(MQTT_BROKERS.len());
    for broker in QUIC_MQTT_BROKERS.iter() {
        brokers.push((broker.key().clone(),
                      broker.value().clone()));
    }
    brokers
}

///
/// 注册指定Mqtt代理的网络监听器
///
pub fn register_mqtt_listener(name: &str,
                              listener: Arc<dyn MqttBrokerListener<TcpSocket>>) -> bool {
    if let Some(item) = MQTT_BROKERS.get(&name.to_string()) {
        match item.value() {
            MqttBrokerProtocol::WsMqtt311(broker) => {
                broker
                    .get_broker()
                    .register_listener(listener);
                return true;
            },
            _ => {
                unimplemented!();
            },
        }
    }

    false
}

///
/// 注册指定Mqtts代理的网络监听器
///
pub fn register_mqtts_listener(name: &str,
                               listener: Arc<dyn MqttBrokerListener<TlsSocket>>) -> bool {
    if let Some(item) = MQTT_BROKERS.get(&name.to_string()) {
        match item.value() {
            MqttBrokerProtocol::WssMqtt311(broker) => {
                broker
                    .get_broker()
                    .register_listener(listener);
                return true;
            },
            _ => {
                unimplemented!();
            },
        }
    }

    false
}

///
/// 注册指定Quic Mqtt代理的网络监听器
///
pub fn register_quic_mqtt_listener(name: &str,
                                   listener: Arc<dyn QuicMqttBrokerListener>) -> bool {
    if let Some(item) = QUIC_MQTT_BROKERS.get(&name.to_string()) {
        match item.value() {
            MqttBrokerProtocol::QuicMqtt311(broker) => {
                broker
                    .get_broker()
                    .register_listener(listener);
                return true;
            },
            _ => {
                unimplemented!();
            },
        }
    }

    false
}

///
/// 注册指定Mqtt代理的网络服务
///
pub fn register_mqtt_service(name: &str,
                             service: Arc<dyn MqttBrokerService<TcpSocket>>) -> bool {
    if let Some(item) = MQTT_BROKERS.get(&name.to_string()) {
        match item.value() {
            MqttBrokerProtocol::WsMqtt311(broker) => {
                broker
                    .get_broker()
                    .register_service(service);
                return true;
            },
            _ => {
                unimplemented!();
            },
        }
    }

    false
}

///
/// 注册指定Mqtts代理的网络服务
///
pub fn register_mqtts_service(name: &str,
                              service: Arc<dyn MqttBrokerService<TlsSocket>>) -> bool {
    if let Some(item) = MQTT_BROKERS.get(&name.to_string()) {
        match item.value() {
            MqttBrokerProtocol::WssMqtt311(broker) => {
                broker
                    .get_broker()
                    .register_service(service);
                return true;
            },
            _ => {
                unimplemented!();
            },
        }
    }

    false
}

///
/// 注册指定Quic Mqtt代理的网络服务
///
pub fn register_quic_mqtt_service(name: &str,
                                  service: Arc<dyn QuicMqttBrokerService>) -> bool {
    if let Some(item) = QUIC_MQTT_BROKERS.get(&name.to_string()) {
        match item.value() {
            MqttBrokerProtocol::QuicMqtt311(broker) => {
                broker
                    .get_broker()
                    .register_service(service);
                return true;
            },
            _ => {
                unimplemented!();
            },
        }
    }

    false
}

///
/// Mqtt代理
///
#[derive(Clone)]
pub enum MqttBrokerProtocol {
    WsMqtt311(Arc<WsMqtt311>),      //基于Websocket的Mqtt3.1.1版本的代理
    WssMqtt311(Arc<WssMqtt311>),    //基于Tls Websocket的Mqtt3.1.1版本的代理
    QuicMqtt311(Arc<QuicMqtt311>),  //基于Quic的Mqtt3.1.1版本的代理
}

impl MqttBrokerProtocol {
    /// 获取Mqtt代理名称
    pub fn get_broker_name(&self) -> &str {
        match self {
            MqttBrokerProtocol::WsMqtt311(broker) => broker.get_broker_name(),
            MqttBrokerProtocol::WssMqtt311(broker) => broker.get_broker_name(),
            MqttBrokerProtocol::QuicMqtt311(borker) => borker.get_broker_name(),
        }
    }
}

///
/// 基于Websocket的Mqtt代理工厂
///
pub struct WsMqttBrokerFactory {
    protocol_name:  String, //协议名
    broker_name:    String, //代理名
    broker_port:    u16,    //代理端口
}

impl WsMqttBrokerFactory {
    /// 构建指定的基于Websocket的Mqtt代理工厂
    pub fn new(protocol_name: &str,
               broker_name: &str,
               broker_port: u16) -> Self {
        let broker = Arc::new(WsMqtt311::with_name(protocol_name,
                                                   broker_name,
                                                   WsMqtt311::MAX_QOS));

        //注册代理
        MQTT_BROKERS
            .insert(broker_name.to_string(),
                    MqttBrokerProtocol::WsMqtt311(broker));
        MQTT_BROKERS_MAP
            .insert(broker_port,
                    broker_name.to_string());

        WsMqttBrokerFactory {
            protocol_name: protocol_name.to_string(),
            broker_name: broker_name.to_string(),
            broker_port,
        }
    }

    /// 构建一个Websocket子协议处理器
    pub fn new_child_protocol(&self) -> Arc<dyn ChildProtocol<TcpSocket>> {
        if let Some(item) = MQTT_BROKERS.get(&self.broker_name) {
            if let MqttBrokerProtocol::WsMqtt311(broker) = item.value() {
                //已存在指定名称的代理，则返回
                return broker.clone();
            }
        }

        //不存在指定名称的代理，则创建代理
        let broker = Arc::new(
            WsMqtt311::with_name(&self.protocol_name,
                                 &self.broker_name,
                                 WsMqtt311::MAX_QOS)
        );

        //注册代理
        MQTT_BROKERS
            .insert(self.broker_name.clone(),
                    MqttBrokerProtocol::WsMqtt311(broker.clone()));
        MQTT_BROKERS_MAP
            .insert(self.broker_port,
                    self.broker_name.clone());

        //返回代理
        broker
    }
}

///
/// 基于Tls Websocket的Mqtt代理工厂
///
pub struct WssMqttBrokerFactory {
    protocol_name:  String, //协议名
    broker_name:    String, //代理名
    broker_port:    u16,    //代理端口
}

impl WssMqttBrokerFactory {
    /// 构建指定的基于Tls Websocket的Mqtt代理工厂
    pub fn new(protocol_name: &str,
               broker_name: &str,
               broker_port: u16) -> Self {
        let broker = Arc::new(
            WssMqtt311::with_name(protocol_name,
                                  broker_name,
                                  WsMqtt311::MAX_QOS)
        );

        //注册代理
        MQTT_BROKERS
            .insert(broker_name.to_string(),
                    MqttBrokerProtocol::WssMqtt311(broker));
        MQTT_BROKERS_MAP
            .insert(broker_port, broker_name.to_string());

        WssMqttBrokerFactory {
            protocol_name: protocol_name.to_string(),
            broker_name: broker_name.to_string(),
            broker_port,
        }
    }

    /// 构建一个Websocket子协议处理器
    pub fn new_child_protocol(&self) -> Arc<dyn ChildProtocol<TlsSocket>> {
        if let Some(item) = MQTT_BROKERS.get(&self.broker_name) {
            if let MqttBrokerProtocol::WssMqtt311(broker) = item.value() {
                //已存在指定名称的代理，则返回
                return broker.clone();
            }
        }

        //不存在指定名称的代理，创建代理
        let broker = Arc::new(WssMqtt311::with_name(&self.protocol_name, &self.broker_name, WsMqtt311::MAX_QOS));

        //注册代理
        MQTT_BROKERS
            .insert(self.broker_name.clone(),
                    MqttBrokerProtocol::WssMqtt311(broker.clone()));
        MQTT_BROKERS_MAP
            .insert(self.broker_port,
                    self.broker_name.clone());

        //返回代理
        broker
    }
}

///
/// 基于Quic的Mqtt代理工厂
///
pub struct QuicMqttBrokerFactory {
    broker_name:    String, //代理名
    broker_port:    u16,    //代理端口
}

impl QuicMqttBrokerFactory {
    /// 构建指定的基于Websocket的Mqtt代理工厂
    pub fn new(broker_name: &str,
               broker_port: u16) -> Self {
        let broker = Arc::new(QuicMqtt311::with_name(broker_name,
                                                     QuicMqtt311::MAX_QOS));

        //注册代理
        QUIC_MQTT_BROKERS
            .insert(broker_name.to_string(),
                    MqttBrokerProtocol::QuicMqtt311(broker));
        QUIC_MQTT_BROKERS_MAP
            .insert(broker_port,
                    broker_name.to_string());

        QuicMqttBrokerFactory {
            broker_name: broker_name.to_string(),
            broker_port,
        }
    }

    /// 构建一个Quic服务
    pub fn new_quic_service(&self) -> Arc<dyn AsyncService> {
        if let Some(item) = QUIC_MQTT_BROKERS.get(&self.broker_name) {
            if let MqttBrokerProtocol::QuicMqtt311(broker) = item.value() {
                //已存在指定名称的代理，则返回
                return broker.clone();
            }
        }

        //不存在指定名称的代理，则创建代理
        let broker = Arc::new(
            QuicMqtt311::with_name(&self.broker_name,
                                   QuicMqtt311::MAX_QOS)
        );

        //注册代理
        QUIC_MQTT_BROKERS
            .insert(self.broker_name.clone(),
                    MqttBrokerProtocol::QuicMqtt311(broker.clone()));
        QUIC_MQTT_BROKERS_MAP
            .insert(self.broker_port,
                    self.broker_name.clone());

        //返回代理
        broker
    }
}

/// 服务器订阅增加指定的主题
pub fn add_topic(broker_name: &String,
                 is_public: bool,
                 topic: String,
                 qos: u8,
                 retain: Option<Publish>) {
    let broker = if let Some(broker) = get_broker(broker_name) {
        broker
    } else {
        return;
    };

    match broker {
        MqttBrokerProtocol::WsMqtt311(broker) => {
            broker.get_broker().subscribed(is_public, &topic, qos, retain);
        },
        MqttBrokerProtocol::WssMqtt311(broker) => {
            broker.get_broker().subscribed(is_public, &topic, qos, retain);
        },
        MqttBrokerProtocol::QuicMqtt311(broker) => {
            broker.get_broker().subscribed(is_public, &topic, qos, retain);
        },
    }
}

/// 服务器发布指定主题的消息
pub fn publish_topic(broker_name: Option<String>,
                     is_public: bool,
                     topic: String,
                     qos: u8,
                     retain: Option<Publish>,
                     payload: Arc<Vec<u8>>) -> Result<()> {
    if let Some(broker_name) = broker_name {
        //指定了Broker
        if let Some(broker) = get_broker(&broker_name) {
            publish_to_connection(&broker,
                                  &broker_name,
                                  is_public,
                                  &topic,
                                  qos,
                                  &retain,
                                  &payload)
        } else {
            Err(Error::new(ErrorKind::Other,
                           format!("Mqtt broker broadcast failed, broker: {:?}, reason: broker not exist",
                                   broker_name)))
        }
    } else {
        //未指定Broker，则全遍历
        for (broker_name, broker) in all_broker() {
            publish_to_connection(&broker,
                                  &broker_name,
                                  is_public,
                                  &topic,
                                  qos,
                                  &retain,
                                  &payload)?;
        }

        Ok(())
    }
}

// 发布指定主题到连接
fn publish_to_connection(broker: &MqttBrokerProtocol,
                         broker_name: &String,
                         is_public: bool,
                         topic: &String,
                         qos: u8,
                         retain: &Option<Publish>,
                         payload: &Arc<Vec<u8>>) -> Result<()> {
    match broker {
        MqttBrokerProtocol::WsMqtt311(broker) => {
            //获取订阅了当前主题的Mqtt会话
            if let Some(sessions) = broker.get_broker().subscribed(is_public, &topic, qos, retain.clone()) {
                //获取Mqtt会话的Ws连接
                let mut connects: Vec<WsSocket<TcpSocket>> = Vec::with_capacity(sessions.len());
                for session in sessions {
                    //返回会话绑定的Ws连接
                    if let Some(connect) = session.get_connect() {
                        connects.push(connect.clone());
                    }
                };

                //构建指定负载的报文
                let packet = Packet::Publish(Publish {
                    dup: false,
                    qos: QoS::AtMostOnce,
                    retain: false,
                    topic_name: topic.clone(),
                    pkid: None,
                    payload: payload.clone(),
                });

                if let Err(e) = v311::broadcast_packet(&connects[..], &packet) {
                    //发布消息失败，则立即返回错误原因
                    return Err(Error::new(ErrorKind::BrokenPipe,
                                          format!("Mqtt broker broadcast failed, broker: {:?}, reason: {:?}",
                                                  broker_name,
                                                  e)));
                }
            }

            Ok(())
        },
        MqttBrokerProtocol::WssMqtt311(broker) => {
            //获取订阅了当前主题的Mqtt会话
            if let Some(sessions) = broker.get_broker().subscribed(is_public, &topic, qos, retain.clone()) {
                //获取Mqtt会话的Ws连接
                let mut connects: Vec<WsSocket<TlsSocket>> = Vec::with_capacity(sessions.len());
                for session in sessions {
                    //返回会话绑定的Ws连接
                    if let Some(connect) = session.get_connect() {
                        connects.push(connect.clone());
                    }
                };

                //构建指定负载的报文
                let packet = Packet::Publish(Publish {
                    dup: false,
                    qos: QoS::AtMostOnce,
                    retain: false,
                    topic_name: topic.clone(),
                    pkid: None,
                    payload: payload.clone(),
                });

                if let Err(e) = tls_v311::broadcast_packet(&connects[..], &packet) {
                    //发布消息失败，则立即返回错误原因
                    return Err(Error::new(ErrorKind::BrokenPipe,
                                          format!("Mqtt broker broadcast failed, broker: {:?}, reason: {:?}",
                                                  broker_name,
                                                  e)));
                }
            }

            Ok(())
        },
        _ => Ok(()),
    }
}

/// QUIC服务器订阅增加指定的主题
pub fn add_quic_topic(broker_name: &String,
                      is_public: bool,
                      topic: String,
                      qos: u8,
                      retain: Option<Publish>) {
    let broker = if let Some(MqttBrokerProtocol::QuicMqtt311(broker)) = get_quic_broker(broker_name) {
        broker
    } else {
        return;
    };

    broker.get_broker().subscribed(is_public, &topic, qos, retain);
}

/// QUIC服务器发布指定主题的消息
pub fn publish_quic_topic(broker_name: Option<String>,
                          is_public: bool,
                          topic: String,
                          qos: u8,
                          retain: Option<Publish>,
                          payload: Arc<Vec<u8>>) -> Result<()> {
    if let Some(broker_name) = broker_name {
        //指定了Broker
        if let Some(MqttBrokerProtocol::QuicMqtt311(broker)) = get_quic_broker(&broker_name) {
            //获取订阅了当前主题的Mqtt会话
            publish_to_quic_connection(&broker,
                                       &broker_name,
                                       is_public,
                                       &topic,
                                       qos,
                                       &retain,
                                       &payload)
        } else {
            Err(Error::new(ErrorKind::Other,
                           format!("Mqtt quic broker broadcast failed, broker: {:?}, topic: {:?}, reason: broker not exist",
                                   topic,
                                   broker_name)))
        }
    } else {
        //未指定Broker，则全遍历
        for (broker_name, broker) in all_quic_broker() {
            if let MqttBrokerProtocol::QuicMqtt311(broker) = broker {
                publish_to_quic_connection(&broker,
                                           &broker_name,
                                           is_public,
                                           &topic,
                                           qos,
                                           &retain,
                                           &payload)?;
            }
        }

        Ok(())
    }
}

// 发布指定主题到Quic连接
fn publish_to_quic_connection(broker: &Arc<QuicMqtt311>,
                              broker_name: &String,
                              is_public: bool,
                              topic: &String,
                              qos: u8,
                              retain: &Option<Publish>,
                              payload: &Arc<Vec<u8>>) -> Result<()> {
    if let Some(sessions) = broker.get_broker().subscribed(is_public, &topic, qos, retain.clone()) {
        //获取Mqtt会话的Ws连接
        let mut connects: Vec<SocketHandle> = Vec::with_capacity(sessions.len());
        for session in sessions {
            //返回会话绑定的Ws连接
            if let Some(connect) = session.get_connect() {
                connects.push(connect.clone());
            }
        };

        //构建指定负载的报文
        let packet = Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            topic_name: topic.clone(),
            pkid: None,
            payload: payload.clone(),
        });

        if let Err(e) = quic_v311::broadcast_packet(&connects[..], &packet) {
            //发布消息失败，则立即返回错误原因
            Err(Error::new(ErrorKind::BrokenPipe,
                           format!("Mqtt quic broker broadcast failed, broker: {:?}, topic: {:?}, reason: {:?}",
                                   topic,
                                   broker_name,
                                   e)))
        } else {
            Ok(())
        }
    } else {
        Err(Error::new(ErrorKind::Other,
                       format!("Mqtt quic broker broadcast failed, broker: {:?}, topic: {:?}, reason: the specified topic is not subscribed",
                               topic,
                               broker_name)))
    }
}

