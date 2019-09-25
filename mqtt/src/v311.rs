use std::sync::Arc;
use std::marker::PhantomData;
use std::collections::HashMap;
use std::io::{Cursor, Result, ErrorKind, Error};

use mio::Token;
use fnv::FnvBuildHasher;
use mqtt311::{MqttWrite, MqttRead, Protocol, ConnectReturnCode, Packet, Connect,
              Connack, QoS, Publish, PacketIdentifier, SubscribeTopic, SubscribeReturnCodes,
              Subscribe, Suback, Unsubscribe, TopicPath};

use atom::Atom;
use hash::XHashMap;

use tcp::{server::AsyncWaitsHandle, driver::Socket, connect::TcpSocket, util::SocketEvent};
use ws::{connect::WsSocket,
         util::{ChildProtocol, ChildProtocolFactory, WsFrameType, WsSession}};

use crate::{broker::{Retain, MqttBroker}, session::{MqttSession, QosZeroSession}};

/*
* 全局Mqtt3.1.1协议代理
*/
lazy_static! {
    static ref WS_MQTT3_BROKER: MqttBroker<TcpSocket> = MqttBroker::new();
}

/*
* 基于Websocket的Mqtt3.1.1协议，服务质量为Qos0
*/
pub struct WsMqtt311 {
    name:       String,         //协议名
    qos:        QoS,            //支持的最大Qos
}

unsafe impl Send for WsMqtt311 {}
unsafe impl Sync for WsMqtt311 {}

impl ChildProtocol<TcpSocket, AsyncWaitsHandle> for WsMqtt311 {
    fn protocol_name(&self) -> &str {
        self.name.as_str()
    }

    fn decode_protocol(&self, connect: WsSocket<TcpSocket, AsyncWaitsHandle>, context: &mut WsSession) -> Result<()> {
        match context.as_buf().read_packet() {
            Err(e) => {
                //解码Mqtt报文失败，则立即返回错误原因
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
                    Packet::Pingreq => {
                        ping_req(connect)
                    },
                    Packet::Disconnect => {
                        disconnect(connect)
                    },
                    _ => {
                        //TODO 在实现Mqtt大于Qos0的消息质量时再实现其它报文的处理
                        Ok(())
                    },
                }
            },
        }
    }

    fn close_protocol(&self, connect: WsSocket<TcpSocket, AsyncWaitsHandle>, mut context: WsSession) {
        //连接已关闭，则清理Mqtt会话
        if let Some(handle) = context.get_context().get::<(String, u16)>() {
            let (client_id, _) = handle.as_ref();
            if let Some(session) = WS_MQTT3_BROKER.get_session(client_id) {
                if session.is_clean() {
                    //如果会话需要被清理
                    WS_MQTT3_BROKER.unsubscribe_all(&session); //退订当前会话订阅的所有主题
                    WS_MQTT3_BROKER.remove_session(client_id); //从会话表中移除会话
                }
            }
        }

        //连接已关闭，则立即释放Ws连接会话
        if let Err(e) = context.get_context_mut().remove::<(String, u16)>() {
            println!("!!!> Free Context Failed of Mqtt Close, reason: {:?}", e);
        }
    }

    fn protocol_timeout(&self, connect: WsSocket<TcpSocket, AsyncWaitsHandle>, context: &mut WsSession, event: SocketEvent) -> Result<()> {
        if let Err(e) = send_packet(&connect, &Packet::Disconnect) {
            //发送关闭连接报文失败，则立即返回错误原因
            return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by disconnect, reason: {:?}", e)));
        }

        println!("!!!> Mqtt Session Timeout, uid: {:?}, local: {:?}, remote: {:?}", connect.get_uid(), connect.get_local(), connect.get_remote());
        Ok(())
    }
}

//更新当前会话的连接超时时长
fn update_timeout(connect: &WsSocket<TcpSocket, AsyncWaitsHandle>, client_id: String, keep_alive: u16) {
    //设置当前会话超时时长，一般为keep_alive的1.5倍
    let mut event = SocketEvent::empty();
    event.set::<String>(client_id);
    connect.set_timeout(keep_alive as usize * 1500, event);
}

//发送指定的Mqtt报文，一般用于报文回应
fn send_packet(connect: &WsSocket<TcpSocket, AsyncWaitsHandle>,
               packet: &Packet) -> Result<()> {
    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    if let Err(e) = buf.write_packet(packet) {
        //序列化Mqtt报文失败
        return Err(Error::new(ErrorKind::InvalidData, format!("mqtt send failed, reason: {:?}", e)));
    }

    //通过Ws连接发送指定报文
    let mut payload = connect.alloc();
    payload.get_iolist_mut().push_back(buf.into_inner().into());
    connect.send(WsMqtt311::WS_MSG_TYPE, payload)
}

//广播指定的Mqtt报文，用于发布消息的发送
fn broadcast_packet(connects: &[WsSocket<TcpSocket, AsyncWaitsHandle>],
                    packet: &Packet) -> Result<()> {
    if connects.len() == 0 {
        //Ws连接列表为空，则忽略
        return Ok(());
    }

    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    if let Err(e) = buf.write_packet(packet) {
        //序列化Mqtt报文失败
        return Err(Error::new(ErrorKind::InvalidData, format!("mqtt send failed, reason: {:?}", e)));
    }

    //通过Ws连接列表广播指定报文
    let mut payload = connects[0].alloc();
    payload.get_iolist_mut().push_back(buf.into_inner().into());
    WsSocket::<TcpSocket, AsyncWaitsHandle>::broadcast(connects, WsMqtt311::WS_MSG_TYPE, payload)
}

//接受Mqtt连接请求
fn accept(protocol: &WsMqtt311,
          connect: WsSocket<TcpSocket, AsyncWaitsHandle>,
          packet: Connect) -> Result<()> {
    let clean_session = packet.clean_session;
    let client_id = packet.client_id.clone();
    let is_exist_session = protocol.is_exist(&client_id); //指定客户端会话是否存在

    if is_exist_session {
        //当前客户端会话存在
        if let Some(session) = WS_MQTT3_BROKER.get_session(&client_id) {
            if session.is_accepted() {
                //当前客户端已连接，则立即返回错误原因
                return Err(Error::new(ErrorKind::AlreadyExists, "mqtt connect failed, reason: connect already exist"));
            }
        }
    }

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
    if (level != WsMqtt311::MQTT31) && (level != WsMqtt311::MQTT311) {
        //协议等级不匹配，则回应，并返回错误原因
        let ack = Connack {
            session_present,
            code: ConnectReturnCode::RefusedProtocolVersion,
        };

        if let Err(e) = send_packet(&connect, &Packet::Connack(ack)) {
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
    if let Err(e) = send_packet(&connect, &Packet::Connack(ack)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by connect, reason: {:?}", e)));
    }

    //在Ws连接会话中绑定当前连接与当前客户端会话
    if let Some(mut handle) = connect.get_session() {
        if let Some(ws_session) = handle.as_mut() {
            ws_session.get_context_mut().set::<(String, u16)>((client_id.clone(), packet.keep_alive));
        }
    }

    //清理当前客户端会话
    if is_exist_session {
        if clean_session {
            //需要清理会话，则创建新会话，并重置当前客户端会话
            reset_session(protocol, connect, client_id, packet);
        }
    } else {
        //当前客户端会话不存在，则创建新会话，并设置当前客户端会话
        reset_session(protocol, connect, client_id, packet);
    }

    Ok(())
}

//创建新会话，并重置会话
fn reset_session(protocol: &WsMqtt311,
                 connect: WsSocket<TcpSocket, AsyncWaitsHandle>,
                 client_id: String,
                 packet: Connect) {
    let mut session = QosZeroSession::with_client_id(client_id.clone());
    session.bind_connect(connect.clone());
    session.set_accept(true);
    session.set_clean(packet.clean_session);
    if let Some(w) = &packet.last_will {
        session.set_will(w.topic.clone(), w.message.clone(), w.qos.to_u8(), w.retain);
    }
    session.set_user_pwd(packet.username, packet.password);
    session.set_keep_alive(packet.keep_alive);

    WS_MQTT3_BROKER.insert_session(client_id, session);

    update_timeout(&connect, packet.client_id, packet.keep_alive);
}

//获取Ws连接绑定的客户端id和客户端保持时长
#[inline(always)]
fn get_client_context(connect: &WsSocket<TcpSocket, AsyncWaitsHandle>) -> Result<(String, u16)> {
    if let Some(handle) = connect.get_session().unwrap().as_ref().get_context().get::<(String, u16)>() {
        //Ws连接绑定的客户端存在
        let h = handle.as_ref();
        Ok((h.0.clone(), h.1))
    } else {
        //Ws连接绑定的客户端不存在，则立即返回错误原因
        Err(Error::new(ErrorKind::ConnectionRefused, "mqtt subscribe failed, reason: invalid connect"))
    }
}

//发布消息
fn publish(protocol: &WsMqtt311,
           connect: WsSocket<TcpSocket, AsyncWaitsHandle>,
           mut packet: Publish) -> Result<()> {
    let (client_id, keep_alive) = match get_client_context(&connect) {
        Err(e) => {
            return Err(e);
        }
        Ok(r) => {
            r
        }
    };
    update_timeout(&connect, client_id.clone(), keep_alive);

    let qos = packet.qos.to_u8();
    if qos > protocol.get_qos().to_u8() {
        //如果客户端要求的发布消息的Qos大于服务端支持的最大Qos，则立即返回错误原因
        return Err(Error::new(ErrorKind::InvalidData, "mqtt publish failed, reason: invalid qos"));
    }

    let topic_path = TopicPath::from(packet.topic_name.as_str());
    if topic_path.wildcards || topic_path.path.is_empty() {
        //发布消息的主题为空，或者有通匹符，则立即返回错误原因
        return Err(Error::new(ErrorKind::InvalidData, "mqtt publish failed, reason: invalid topic"));
    }

    //修改需要待发送的发布消息报文，并向订阅了指定主题的客户端发送当前报文
    packet.dup = false;
    let mut retain = None;
    if packet.retain {
        //需要缓存当前主题的最近一条发布消息
        retain = Some(packet.clone());
    }

    let mut is_public = true; //是否为公共主题

    if let Some(sessions) = WS_MQTT3_BROKER.subscribed(is_public, &topic_path.path, qos, retain) {
        //指定主题有订阅
        let mut connects: Vec<WsSocket<TcpSocket, AsyncWaitsHandle>> = Vec::with_capacity(sessions.len());
        for session in sessions {
            //返回会话绑定的Ws连接
            if let Some(connect) = session.get_connect() {
                connects.push(connect.clone());
            }
        };
        if let Err(e) = broadcast_packet(&connects[..], &Packet::Publish(packet)) {
            //发布消息失败，则立即返回错误原因
            return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt broadcast failed by publish, reason: {:?}", e)));
        }
    }

    Ok(())
}

//订阅主题
fn subscribe(protocol: &WsMqtt311,
             connect: WsSocket<TcpSocket, AsyncWaitsHandle>,
             packet: Subscribe) -> Result<()> {
    let (client_id, keep_alive) = match get_client_context(&connect) {
        Err(e) => {
            return Err(e);
        }
        Ok(r) => {
            r
        }
    };
    update_timeout(&connect, client_id.clone(), keep_alive);

    let pkid = packet.pkid;
    let topics = packet.topics;
    let mut retains: Option<Retain> = None;
    let mut return_codes = Vec::with_capacity(topics.len()); //订阅回应码向量
    if let Some(session) = WS_MQTT3_BROKER.get_session(&client_id) {
        //客户端的会话存在，则开始订阅主题
        let qos = protocol.get_qos();
        let qos_val = qos.to_u8();
        for topic in topics {
            if qos_val < topic.qos.to_u8() {
                //如果订阅的消息质量大于服务支持的最大消息质量，则返回当前主题订阅失败
                return_codes.push(SubscribeReturnCodes::Failure);
            } else {
                //如果订阅的消息质量小于等于服务支持的最大消息质量，则返回当前主题订阅成功
                return_codes.push(SubscribeReturnCodes::Success(qos));
            }

            retains = WS_MQTT3_BROKER.subscribe(session.clone(), topic.topic_path);
        }
    }

    //回应订阅主题
    let ack = Suback {
        pkid,
        return_codes,
    };
    if let Err(e) = send_packet(&connect, &Packet::Suback(ack)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by subscribe, reason: {:?}", e)));
    }

    //发布订阅主题的最新的发布消息
    if let Some(r) = retains {
        match r {
            Retain::Single(p) => {
                send_packet(&connect, &Packet::Publish(p));
            },
            Retain::Mutil(ps) => {
                for p in ps {
                    send_packet(&connect, &Packet::Publish(p));
                }
            },
        }
    }

    Ok(())
}

//退订主题
fn unsubscribe(protocol: &WsMqtt311,
               connect: WsSocket<TcpSocket, AsyncWaitsHandle>,
               packet: Unsubscribe) -> Result<()> {
    let (client_id, keep_alive) = match get_client_context(&connect) {
        Err(e) => {
            return Err(e);
        }
        Ok(r) => {
            r
        }
    };
    update_timeout(&connect, client_id.clone(), keep_alive);

    let pkid = packet.pkid;
    let topics = packet.topics;
    if let Some(session) = WS_MQTT3_BROKER.get_session(&client_id) {
        //客户端的会话存在，则开始退订主题
        for topic in topics {
            WS_MQTT3_BROKER.unsubscribe(&session, topic);
        }
    }

    //回应退订主题
    if let Err(e) = send_packet(&connect, &Packet::Unsuback(pkid)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by unsubscribe, reason: {:?}", e)));
    }

    Ok(())
}

//处理ping请求
fn ping_req(connect: WsSocket<TcpSocket, AsyncWaitsHandle>) -> Result<()> {
    match get_client_context(&connect) {
        Err(e) => {
            return Err(e);
        }
        Ok((client_id, keep_alive)) => {
            update_timeout(&connect, client_id.clone(), keep_alive);
        }
    }

    if let Err(e) = send_packet(&connect, &Packet::Pingresp) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by ping, reason: {:?}", e)));
    }

    Ok(())
}

//处理关闭连接请求
fn disconnect(connect: WsSocket<TcpSocket, AsyncWaitsHandle>) -> Result<()> {
    connect.close(Ok(()))
}

impl WsMqtt311 {
    pub const WS_MSG_TYPE: WsFrameType = WsFrameType::Binary;   //Mqtt3.1.1的Websocket消息类型
    pub const MQTT31: u8 = 0x3;                                 //Mqtt3.1的协议等级
    pub const MQTT311: u8 = 0x4;                                //Mqtt3.1.1的协议等级
    pub const MAX_QOS: u8 = 0;                                  //支持的最大Qos

    //构建指定协议名和支持的最大Qos，且基于Websocket的Mqtt3.1.1协议
    pub fn with_name(name: &str, qos: u8) -> Self {
        WsMqtt311 {
            name: name.to_lowercase(),
            qos: QoS::from_u8(qos).unwrap(),
        }
    }

    //判断指定ClientId的会话是否存在
    #[inline(always)]
    pub fn is_exist(&self, client_id: &str) -> bool {
        WS_MQTT3_BROKER.is_exist_session(&client_id.to_string())
    }

    //获取支持的最大Qos
    #[inline(always)]
    pub fn get_qos(&self) -> QoS {
        self.qos
    }
}

/*
* 基于Websocket的Mqtt3.1.1协议工厂
*/
pub struct WsMqtt311Factory {
    name:   String,         //协议名
}

impl ChildProtocolFactory for WsMqtt311Factory {
    type Connect = TcpSocket;
    type Waits = AsyncWaitsHandle;

    fn new_protocol(&self) -> Arc<dyn ChildProtocol<Self::Connect, Self::Waits>> {
        Arc::new(WsMqtt311::with_name(&self.name, WsMqtt311::MAX_QOS))
    }
}

impl WsMqtt311Factory {
    //构建指定协议名的基于Websocket的Mqtt3.1.1协议工厂
    pub fn with_name(name: &str) -> Self {
        WsMqtt311Factory {
            name: name.to_string(),
        }
    }
}