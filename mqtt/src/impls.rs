use std::sync::Arc;
use std::marker::PhantomData;
use std::collections::HashMap;
use std::io::{Cursor, Result, ErrorKind, Error};

use mio::Token;
use fnv::FnvBuildHasher;
use parking_lot::RwLock;
use mqtt311::{MqttWrite, MqttRead, Protocol, ConnectReturnCode, Packet, Connect,
              Connack, QoS, Publish, PacketIdentifier, SubscribeTopic, SubscribeReturnCodes,
              Subscribe, Suback, Unsubscribe};

use atom::Atom;
use hash::XHashMap;

use tcp::{server::AsyncWaitsHandle,
          driver::Socket};
use ws::{connect::WsSocket,
         util::{ChildProtocol, ChildProtocolFactory, WsFrameType, WsContext}};

use crate::session::{MqttSession, QosZeroSession};

/*
* 基于Websocket的Mqtt3.1.1协议，服务质量为Qos0
*/
pub struct WsMqttProtocol<S: Socket, E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>> {
    name:       String,                                                 //协议名
    qos:        QoS,                                                    //支持的最大Qos
    sessions:   Arc<RwLock<XHashMap<String, E>>>,                       //会话表
    binds:      Arc<RwLock<HashMap<Token, String, FnvBuildHasher>>>,    //连接客户端绑定表
    marker:     PhantomData<S>,
}

unsafe impl<S: Socket, E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>> Send for WsMqttProtocol<S, E> {}
unsafe impl<S: Socket, E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>> Sync for WsMqttProtocol<S, E> {}

impl<S: Socket, E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>> ChildProtocol<S, AsyncWaitsHandle> for WsMqttProtocol<S, E> {
    fn protocol_name(&self) -> &str {
        self.name.as_str()
    }

    fn decode_protocol(&self, connect: WsSocket<S, AsyncWaitsHandle>, context: &mut WsContext) -> Result<()> {
        match context.as_buf().read_packet() {
            Err(e) => {
                //解码Mqtt报文失败，则立即关闭Ws连接
                Err(Error::new(ErrorKind::Other, format!("websocket decode child protocol failed, protocol: {:?}, reason: {:?}", self.name, e)))
            },
            Ok(packet) => {
                match packet {
                    Packet::Connect(packet) => {
                        accept(self, connect, packet)
                    },
                    Packet::Publish(packet) => {
                        publish(self, connect, packet)
                    },
                    Packet::Subscribe(packet) => {
                        subscribe(self, connect, packet)
                    },
                    Packet::Unsubscribe(packet) => {
                        unsubscribe(self, connect, packet)
                    },
                    _ => {
                        //TODO 在实现Mqtt客户端时再实现其它报文的处理
                        Ok(())
                    },
                }
            },
        }
    }
}

//发送指定的Mqtt报文，一般用于报文回应
fn send_packet<S, E>(protocol: &WsMqttProtocol<S, E>,
                     connect: &WsSocket<S, AsyncWaitsHandle>,
                     packet: &Packet) -> Result<()>
    where S: Socket,
          E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>, {
    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    if let Err(e) = buf.write_packet(packet) {
        //序列化Mqtt报文失败
        return Err(Error::new(ErrorKind::InvalidData, format!("mqtt send failed, reason: {}", e)));
    }

    //通过Ws连接发送指定报文
    let mut payload = connect.alloc();
    payload.get_iolist_mut().push_back(buf.into_inner().into());
    connect.send(WsMqttProtocol::<S, E>::WS_MSG_TYPE, payload)
}

//广播指定的Mqtt报文，用于发布消息的发送
fn broadcast_packet<S, E>(protocol: &WsMqttProtocol<S, E>,
                          connects: &[WsSocket<S, AsyncWaitsHandle>],
                          packet: &Packet) -> Result<()>
    where S: Socket,
          E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>, {
    if connects.len() == 0 {
        //Ws连接列表为空，则忽略
        return Ok(());
    }

    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    if let Err(e) = buf.write_packet(packet) {
        //序列化Mqtt报文失败
        return Err(Error::new(ErrorKind::InvalidData, format!("mqtt send failed, reason: {}", e)));
    }

    //通过Ws连接列表广播指定报文
    let mut payload = connects[0].alloc();
    payload.get_iolist_mut().push_back(buf.into_inner().into());
    WsSocket::<S, AsyncWaitsHandle>::broadcast(connects, WsMqttProtocol::<S, E>::WS_MSG_TYPE, payload)
}

//接受Mqtt连接请求
fn accept<S, E>(protocol: &WsMqttProtocol<S, E>,
                connect: WsSocket<S, AsyncWaitsHandle>,
                packet: Connect) -> Result<()>
    where S: Socket,
          E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>, {
    let clean_session = packet.clean_session;
    let client_id = packet.client_id.clone();
    let is_exist_session = protocol.is_exist(&client_id); //指定客户端会话是否存在

    //设置当前会话标记
    let mut session_present = true;
    if clean_session {
        //如果需要在关闭连接后清理会话
        session_present = false;
    } else if !clean_session && !is_exist_session {
        //如果需要在关闭连接后保留会话，且当前会话表中没有指定ClientId的会话
        session_present = false;
    }

    //检查协议名和协议
    let level = packet.protocol.level();
    if (level != WsMqttProtocol::<S, E>::MQTT31) && (level != WsMqttProtocol::<S, E>::MQTT311) {
        //协议等级不匹配，则回应，并返回错误原因
        let ack = Connack {
            session_present,
            code: ConnectReturnCode::RefusedProtocolVersion,
        };

        if let Err(e) = send_packet(protocol, &connect, &Packet::Connack(ack)) {
            //发送回应失败，则立即返回错误原因
            return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by connect, reason: {:?}", e)));
        }

        return Err(Error::new(ErrorKind::ConnectionAborted, "mqtt connect failed, reason: invalid protocol version"));
    }

    //回应连接已接受
    let ack = Connack {
        session_present,
        code: ConnectReturnCode::Accepted,
    };
    if let Err(e) = send_packet(protocol, &connect, &Packet::Connack(ack)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by connect, reason: {:?}", e)));
    }

    //绑定当前连接与当前客户端会话
    protocol.binds.write().insert(connect.get_token().unwrap().clone(), client_id.clone());

    //缓存当前客户端会话
    if is_exist_session {
        //当前客户端会话存在
        if let Some(session) = protocol.sessions.read().get(&client_id) {
            if session.is_accepted() {
                //已连接，则立即返回错误原因
                return Err(Error::new(ErrorKind::AlreadyExists, "mqtt connect failed, reason: connect already exist"));
            }
        }

        if clean_session {
            //需要清理会话，则创建新会话，并重置当前客户端会话
            reset_session::<S, E>(protocol, connect, packet);
        }
    } else {
        //当前客户端会话不存在，则创建新会话，并设置当前客户端会话
        reset_session::<S, E>(protocol, connect, packet);
    }

    Ok(())
}

//创建新会话，并重置会话
fn reset_session<S, E>(protocol: &WsMqttProtocol<S, E>,
                       connect: WsSocket<S, AsyncWaitsHandle>,
                       packet: Connect)
    where S: Socket,
          E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>, {
    let mut session = E::default();
    session.bind_connect(connect);
    session.set_accept(true);
    session.set_clean(packet.clean_session);
    if let Some(w) = &packet.last_will {
        session.set_will(w.topic.clone(), w.message.clone(), w.qos.to_u8(), w.retain);
    }
    session.set_user_pwd(packet.username, packet.password);
    session.set_keep_alive(packet.keep_alive);

    protocol.sessions.write().insert(packet.client_id, session);
}

//发布消息
fn publish<S, E>(protocol: &WsMqttProtocol<S, E>,
                 connect: WsSocket<S, AsyncWaitsHandle>,
                 packet: Publish) -> Result<()>
    where S: Socket,
          E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>, {

    Ok(())
}

//订阅主题
fn subscribe<S, E>(protocol: &WsMqttProtocol<S, E>,
                   connect: WsSocket<S, AsyncWaitsHandle>,
                   packet: Subscribe) -> Result<()>
    where S: Socket,
          E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>, {
    let client_id;
    if let Some(id) = protocol.binds.read().get(connect.get_token().unwrap()) {
        //连接绑定的客户端存在
        client_id = id.clone();
    } else {
        return Err(Error::new(ErrorKind::ConnectionRefused, "mqtt subscribe failed, reason: invalid connect"));
    }

    let pkid = packet.pkid;
    let topics = packet.topics;
    let mut return_codes = Vec::with_capacity(topics.len()); //订阅回应码向量
    if let Some(session) = protocol.sessions.write().get_mut(&client_id) {
        //客户端的会话存在，则开始订阅主题
        let qos = protocol.get_qos();
        for topic in topics {
            return_codes.push(SubscribeReturnCodes::Success(qos));
            session.subscribe(Atom::from(topic.topic_path), topic.qos.to_u8());
        }
    }

    //回应订阅主题
    let ack = Suback {
        pkid,
        return_codes,
    };
    if let Err(e) = send_packet(protocol, &connect, &Packet::Suback(ack)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by subscribe, reason: {:?}", e)));
    }

    Ok(())
}

//退订主题
fn unsubscribe<S, E>(protocol: &WsMqttProtocol<S, E>,
                     connect: WsSocket<S, AsyncWaitsHandle>,
                     packet: Unsubscribe) -> Result<()>
    where S: Socket,
          E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>, {
    let client_id;
    if let Some(id) = protocol.binds.read().get(connect.get_token().unwrap()) {
        //连接绑定的客户端存在
        client_id = id.clone();
    } else {
        return Err(Error::new(ErrorKind::ConnectionRefused, "mqtt subscribe failed, reason: invalid connect"));
    }

    let pkid = packet.pkid;
    let topics = packet.topics;
    if let Some(session) = protocol.sessions.write().get_mut(&client_id) {
        //客户端的会话存在，则开始退订主题
        for topic in topics {
            session.unsubscribe(&Atom::from(topic));
        }
    }

    //回应退订主题
    if let Err(e) = send_packet(protocol, &connect, &Packet::Unsuback(pkid)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by unsubscribe, reason: {:?}", e)));
    }

    Ok(())
}

impl<S: Socket, E: MqttSession<Connect = WsSocket<S, AsyncWaitsHandle>>> WsMqttProtocol<S, E> {
    pub const WS_MSG_TYPE: WsFrameType = WsFrameType::Binary;   //Mqtt3.1.1的Websocket消息类型
    pub const MQTT31: u8 = 0x3;                                 //Mqtt3.1的协议等级
    pub const MQTT311: u8 = 0x4;                                //Mqtt3.1.1的协议等级
    pub const MAX_QOS: u8 = 0;                                  //支持的最大Qos

    //构建指定协议名和支持的最大Qos，且基于Websocket的Mqtt3.1.1协议
    pub fn with_name(name: &str, qos: u8) -> Self {
        WsMqttProtocol {
            name: name.to_lowercase(),
            qos: QoS::from_u8(qos).unwrap(),
            sessions: Arc::new(RwLock::new(XHashMap::default())),
            binds: Arc::new(RwLock::new(HashMap::with_hasher(FnvBuildHasher::default()))),
            marker: PhantomData,
        }
    }

    //判断指定ClientId的会话是否存在
    #[inline(always)]
    pub fn is_exist(&self, client_id: &str) -> bool {
        self.sessions.read().contains_key(client_id)
    }

    //获取支持的最大Qos
    #[inline(always)]
    pub fn get_qos(&self) -> QoS {
        self.qos
    }
}

/*
* 基于Websocket的Mqtt协议工厂
*/
pub struct WsMqttProtocolFactory<S: Socket> {
    name:   String,         //协议名
    marker: PhantomData<S>,
}

impl<S: Socket> ChildProtocolFactory for WsMqttProtocolFactory<S> {
    type Connect = S;
    type Waits = AsyncWaitsHandle;

    fn new_protocol(&self) -> Arc<dyn ChildProtocol<Self::Connect, Self::Waits>> {
        Arc::new(WsMqttProtocol::<S, QosZeroSession<S>>::with_name(&self.name, WsMqttProtocol::<S, QosZeroSession<S>>::MAX_QOS))
    }
}

impl<S: Socket> WsMqttProtocolFactory<S> {
    //构建指定协议名的基于Websocket的Mqtt3.1.1协议工厂
    pub fn with_name(name: &str) -> Self {
        WsMqttProtocolFactory {
            name: name.to_string(),
            marker: PhantomData,
        }
    }
}