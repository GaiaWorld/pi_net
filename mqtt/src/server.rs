use std::sync::Arc;
use std::io::{Error, Result, ErrorKind};

use mqtt311::{QoS, Packet, Publish};
use parking_lot::RwLock;

use hash::XHashMap;

use tcp::{server::AsyncWaitsHandle, connect::TcpSocket, tls_connect::TlsSocket};
use ws::{connect::WsSocket, util::{ChildProtocol, ChildProtocolFactory}};

use crate::{v311::{self, WsMqtt311}, tls_v311::{self, WssMqtt311}, broker::{MqttBrokerListener, MqttBrokerService}, session::{MqttSession, QosZeroSession}};

/*
* Mqtt代理表和代理映射表
*/
lazy_static! {
    static ref MQTT_BROKERS: RwLock<XHashMap<String, MqttBrokerProtocol>> = RwLock::new(XHashMap::default());
    static ref MQTT_BROKERS_MAP: RwLock<XHashMap<u16, String>> = RwLock::new(XHashMap::default());
}

/*
* 获取指定端口的Mqtt代理名称
*/
pub fn get_broker_name(port: u16) -> Option<String> {
    MQTT_BROKERS_MAP.read().get(&port).cloned()
}

/*
* 注册指定Mqtt代理的网络监听器
*/
pub fn register_listener(name: &str, listener: Arc<dyn MqttBrokerListener>) -> bool {
    if let Some(broker) = MQTT_BROKERS.read().get(&name.to_string()) {
        match broker {
            MqttBrokerProtocol::WsMqtt311(broker) => {
                broker.get_broker().register_listener(listener);
                return true;
            },
            MqttBrokerProtocol::WssMqtt311(broker) => {
                broker.get_broker().register_listener(listener);
                return true;
            }
        }
    }

    false
}

/*
* 注册指定Mqtt代理的网络服务
*/
pub fn register_service(name: &str, service: Arc<dyn MqttBrokerService>) -> bool {
    if let Some(broker) = MQTT_BROKERS.read().get(&name.to_string()) {
        match broker {
            MqttBrokerProtocol::WsMqtt311(broker) => {
                broker.get_broker().register_service(service);
                return true;
            },
            MqttBrokerProtocol::WssMqtt311(broker) => {
                broker.get_broker().register_service(service);
                return true;
            }
        }
    }

    false
}

/*
* Mqtt代理
*/
#[derive(Clone)]
pub enum MqttBrokerProtocol {
    WsMqtt311(Arc<WsMqtt311>),      //基于Websocket的Mqtt3.1.1版本的代理
    WssMqtt311(Arc<WssMqtt311>),    //基于Tls Websocket的Mqtt3.1.1版本的代理
}

impl MqttBrokerProtocol {
    //获取Mqtt代理名称
    pub fn get_broker_name(&self) -> &str {
        match self {
            MqttBrokerProtocol::WsMqtt311(broker) => broker.get_broker_name(),
            MqttBrokerProtocol::WssMqtt311(broker) => broker.get_broker_name(),
        }
    }
}

/*
* 基于Websocket的Mqtt代理工厂
*/
pub struct WsMqttBrokerFactory {
    protocol_name:  String, //协议名
    broker_name:    String, //代理名
    broker_port:    u16,    //代理端口
}

impl ChildProtocolFactory for WsMqttBrokerFactory {
    type Connect = TcpSocket;
    type Waits = AsyncWaitsHandle;

    fn new_protocol(&self) -> Arc<dyn ChildProtocol<Self::Connect, Self::Waits>> {
        if let Some(broker) = MQTT_BROKERS.read().get(&self.broker_name) {
            if let MqttBrokerProtocol::WsMqtt311(broker) = broker {
                //已存在指定名称的代理，则返回
                return broker.clone();
            }
        }

        //不存在指定名称的代理，则创建代理
        let broker = Arc::new(WsMqtt311::with_name(&self.protocol_name, &self.broker_name, WsMqtt311::MAX_QOS));

        //注册代理
        MQTT_BROKERS.write().insert(self.broker_name.clone(), MqttBrokerProtocol::WsMqtt311(broker.clone()));
        MQTT_BROKERS_MAP.write().insert(self.broker_port, self.broker_name.clone());

        //返回代理
        broker
    }
}

impl WsMqttBrokerFactory {
    //构建指定的基于Websocket的Mqtt代理工厂
    pub fn new(protocol_name: &str,
               broker_name: &str,
               broker_port: u16) -> Self {
        let broker = Arc::new(WsMqtt311::with_name(protocol_name, broker_name, WsMqtt311::MAX_QOS));

        //注册代理
        MQTT_BROKERS.write().insert(broker_name.to_string(), MqttBrokerProtocol::WsMqtt311(broker));
        MQTT_BROKERS_MAP.write().insert(broker_port, broker_name.to_string());

        WsMqttBrokerFactory {
            protocol_name: protocol_name.to_string(),
            broker_name: broker_name.to_string(),
            broker_port,
        }
    }
}

/*
* 基于Tls Websocket的Mqtt代理工厂
*/
pub struct WssMqttBrokerFactory {
    protocol_name:  String, //协议名
    broker_name:    String, //代理名
    broker_port:    u16,    //代理端口
}

impl ChildProtocolFactory for WssMqttBrokerFactory {
    type Connect = TlsSocket;
    type Waits = AsyncWaitsHandle;

    fn new_protocol(&self) -> Arc<dyn ChildProtocol<Self::Connect, Self::Waits>> {
        if let Some(broker) = MQTT_BROKERS.read().get(&self.broker_name) {
            if let MqttBrokerProtocol::WssMqtt311(broker) = broker {
                //已存在指定名称的代理，则返回
                return broker.clone();
            }
        }

        //不存在指定名称的代理，创建代理
        let broker = Arc::new(WssMqtt311::with_name(&self.protocol_name, &self.broker_name, WsMqtt311::MAX_QOS));

        //注册代理
        MQTT_BROKERS.write().insert(self.broker_name.clone(), MqttBrokerProtocol::WssMqtt311(broker.clone()));
        MQTT_BROKERS_MAP.write().insert(self.broker_port, self.broker_name.clone());

        //返回代理
        broker
    }
}

impl WssMqttBrokerFactory {
    //构建指定的基于Tls Websocket的Mqtt代理工厂
    pub fn new(protocol_name: &str,
               broker_name: &str,
               broker_port: u16) -> Self {
        let broker = Arc::new(WssMqtt311::with_name(protocol_name, broker_name, WsMqtt311::MAX_QOS));

        //注册代理
        MQTT_BROKERS.write().insert(broker_name.to_string(), MqttBrokerProtocol::WssMqtt311(broker));
        MQTT_BROKERS_MAP.write().insert(broker_port, broker_name.to_string());

        WssMqttBrokerFactory {
            protocol_name: protocol_name.to_string(),
            broker_name: broker_name.to_string(),
            broker_port,
        }
    }
}

//服务器订阅增加指定的主题
pub fn add_topic(broker_name: &String,
                 is_public: bool,
                 topic: String,
                 qos: u8,
                 retain: Option<Publish>) {
    if let Some(broker) = MQTT_BROKERS.read().get(broker_name) {
        match broker {
            MqttBrokerProtocol::WsMqtt311(broker) => {
                broker.get_broker().subscribed(is_public, &topic, qos, retain);
            },
            MqttBrokerProtocol::WssMqtt311(broker) => {
                broker.get_broker().subscribed(is_public, &topic, qos, retain);
            },
        }
    }
}

//服务器发布指定主题的消息
pub fn publish_topic(broker_name: String,
                     is_public: bool,
                     topic: String,
                     qos: u8,
                     retain: Option<Publish>,
                     payload: Arc<Vec<u8>>) -> Result<()> {
    if let Some(broker) = MQTT_BROKERS.read().get(&broker_name) {
        match broker {
            MqttBrokerProtocol::WsMqtt311(broker) => {
                //获取订阅了当前主题的Mqtt会话
                if let Some(sessions) = broker.get_broker().subscribed(is_public, &topic, qos, retain) {
                    //获取Mqtt会话的Ws连接
                    let mut connects: Vec<WsSocket<TcpSocket, AsyncWaitsHandle>> = Vec::with_capacity(sessions.len());
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
                        payload,
                    });

                    if let Err(e) = v311::broadcast_packet(&connects[..], &packet) {
                        //发布消息失败，则立即返回错误原因
                        return Err(Error::new(ErrorKind::BrokenPipe, format!("Mqtt broker broadcast failed, broker: {:?}, reason: {:?}", broker_name, e)));
                    }
                }

                Ok(())
            },
            MqttBrokerProtocol::WssMqtt311(broker) => {
                //获取订阅了当前主题的Mqtt会话
                if let Some(sessions) = broker.get_broker().subscribed(is_public, &topic, qos, retain) {
                    //获取Mqtt会话的Ws连接
                    let mut connects: Vec<WsSocket<TlsSocket, AsyncWaitsHandle>> = Vec::with_capacity(sessions.len());
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
                        payload,
                    });

                    if let Err(e) = tls_v311::broadcast_packet(&connects[..], &packet) {
                        //发布消息失败，则立即返回错误原因
                        return Err(Error::new(ErrorKind::BrokenPipe, format!("Mqtt broker broadcast failed, broker: {:?}, reason: {:?}", broker_name, e)));
                    }
                }

                Ok(())
            },
        }
    } else {
        Err(Error::new(ErrorKind::Other, format!("Mqtt broker broadcast failed, broker: {:?}, reason: broker not exist", broker_name)))
    }
}