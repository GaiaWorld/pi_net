use std::sync::Arc;
use std::io::{Cursor, Result, ErrorKind, Error};

use futures::future::{FutureExt, LocalBoxFuture};
use mqtt311::{MqttWrite, MqttRead, ConnectReturnCode, Packet, Connect,
              Connack, QoS, Publish, SubscribeReturnCodes,
              Subscribe, Suback, Unsubscribe, TopicPath, Error as MqttError};
use quinn_proto::{StreamId, Dir};
use bytes::Buf;
use log::error;

use quic::{AsyncService, SocketHandle, SocketEvent,
           connect::QuicSocket,
           utils::QuicSocketReady};

use crate::{server::MqttBrokerProtocol,
            quic_broker::{Retain, MqttBroker},
            quic_session::{MqttConnect, MqttSession, QosZeroSession},
            utils::QuicBrokerSession};

///
/// 基于Quic的Mqtt3.1.1协议，服务质量为Qos0
///
#[derive(Clone)]
pub struct QuicMqtt311 {
    broker_name:    String,     //代理名
    qos:            QoS,        //支持的最大Qos
    broker:         MqttBroker, //Mqtt代理
}

unsafe impl Send for QuicMqtt311 {}
unsafe impl Sync for QuicMqtt311 {}

impl AsyncService for QuicMqtt311 {
    fn handle_connected(&self,
                        handle: SocketHandle,
                        result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {
            if let Err(e) = result {
                error!("Connect failed for mqtt by quic, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                    handle.get_uid(),
                    handle.get_remote(),
                    handle.get_local(),
                    e);
                return;
            }

            handle.set_ready(handle.get_main_stream_id().unwrap().clone(),
                             QuicSocketReady::Readable); //开始首次读
        }.boxed_local()
    }

    fn handle_opened_expanding_stream(&self,
                                      _handle: SocketHandle,
                                      _stream_id: StreamId,
                                      _stream_type: Dir,
                                      _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle,
                     stream_id: StreamId,
                     result: Result<usize>) -> LocalBoxFuture<'static, ()> {
        let quic_mqtt = self.clone();
        async move {
            if let Err(e) = result {
                error!("Read failed for mqtt by quic, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
                    handle.get_uid(),
                    handle.get_remote(),
                    handle.get_local(),
                    stream_id,
                    e);
                return;
            }

            loop {
                let mut ready_len = 0;
                let remaining = if let Some(len) = handle.read_buffer_remaining(handle.get_main_stream_id().unwrap()) {
                    len
                } else {
                    return;
                };

                if remaining == 0 {
                    //当前读缓冲中没有数据，则异步准备读取数据
                    ready_len = match handle.read_ready(handle.get_main_stream_id().unwrap(), 0) {
                        Err(len) => len,
                        Ok(value) => {
                            value.await
                        },
                    };

                    if ready_len == 0 {
                        //当前连接已关闭，则立即退出
                        return;
                    }
                }

                let packet = if let Some(buf) = handle.get_read_buffer(handle.get_main_stream_id().unwrap()).as_ref().unwrap().lock().as_mut() {
                    let mut bin: &[u8] = buf.chunk();
                    match bin.read_packet_with_len() {
                        Err(MqttError::PayloadSizeIncorrect) | Err(MqttError::PayloadRequired) | Err(MqttError::MalformedRemainingLength) => {
                            //Mqtt请求包负载大小错误或负载为空或错误的剩余数据大小，则立即退出本次解析，并等待接收到更多数据后再解析
                            return;
                        },
                        Err(e) => {
                            //解析Mqtt请求包失败，且无法继续解析，则立即关闭当前Quic连接
                            error!("Parse packet failed for mqtt by quic, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
                                handle.get_uid(),
                                handle.get_remote(),
                                handle.get_local(),
                                stream_id,
                                e);

                            handle.close(1000, Err(Error::new(ErrorKind::InvalidInput, format!("{:?}", e))));
                            return;
                        },
                        Ok((readed_packet, readed_len)) => {
                            //解析Mqtt请求包成功，则消耗Quic连接的读缓冲区中对应长度的数据
                            drop(bin);
                            let _ = buf.copy_to_bytes(readed_len);
                            readed_packet
                        },
                    }
                } else {
                    return;
                };

                let result = match packet {
                    Packet::Connect(packet) => {
                        accept(quic_mqtt.clone(), handle.clone(), packet).await
                    },
                    Packet::Publish(packet) => {
                        publish(quic_mqtt.clone(), handle.clone(), packet).await
                    },
                    Packet::Subscribe(packet) => {
                        subscribe(quic_mqtt.clone(), handle.clone(), packet).await
                    },
                    Packet::Unsubscribe(packet) => {
                        unsubscribe(quic_mqtt.clone(), handle.clone(), packet).await
                    },
                    Packet::Pingreq => {
                        ping_req(handle.clone())
                    },
                    Packet::Disconnect => {
                        disconnect(handle.clone())
                    },
                    _ => {
                        //TODO 在实现Mqtt大于Qos0的消息质量时再实现其它报文的处理
                        return;
                    },
                };

                if let Err(e) = result {
                    //处理Mqtt数据包错误
                    error!("Handle packet failed for mqtt by quic, uid: {:?}, remote: {:?}, local: {:?}, stream_id: {:?}, reason: {:?}",
                                handle.get_uid(),
                                handle.get_remote(),
                                handle.get_local(),
                                stream_id,
                                e);

                    handle.close(1000, Err(Error::new(ErrorKind::InvalidData, format!("{:?}", e))));
                    return;
                }
            }
        }.boxed_local()
    }

    fn handle_writed(&self,
                     _handle: SocketHandle,
                     _stream_id: StreamId,
                     _result: Result<()>) -> LocalBoxFuture<'static, ()> {
        async move {

        }.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle,
                     stream_id: Option<StreamId>,
                     code: u32,
                     result: Result<()>) -> LocalBoxFuture<'static, ()> {
        //连接已关闭，则立即释放Quic会话的上下文
        match handle.remove_session::<QuicBrokerSession>() {
            Err(e) => {
                error!("Free context failed for mqtt by quic, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                    handle.get_uid(),
                    handle.get_remote(),
                    handle.get_local(),
                    e);
            },
            Ok(opt) => {
                if let Some(context) = opt {
                    let client_id = context.get_client_id();
                    if let Some(session) = self.broker.get_session(client_id) {
                        if session.is_clean() {
                            //需要清理会话
                            self.broker.unsubscribe_all(&session); //退订当前会话订阅的所有主题
                            self.broker.remove_session(client_id); //从会话表中移除会话
                        }

                        //设置会话为已关闭
                        session.set_accept(false);

                        //处理Mqtt连接关闭
                        if let Some(listener) = self.broker.get_listener() {
                            return listener
                                .0
                                .closed(MqttBrokerProtocol::QuicMqtt311(Arc::new(self.clone())),
                                        session,
                                        context,
                                        result);
                        }
                    }
                }
            },
        }

        async move {

        }.boxed_local()
    }

    /// 异步处理已超时
    fn handle_timeouted(&self,
                        handle: SocketHandle,
                        result: Result<SocketEvent>) -> LocalBoxFuture<'static, ()> {
        if let Err(e) = send_packet(&handle, &Packet::Disconnect) {
            //发送关闭连接报文失败，则立即返回错误原因
            error!("Mqtt send closed failed for mqtt by quic, uid: {:?}, local: {:?}, remote: {:?}, reason: {:?}",
                handle.get_uid(),
                handle.get_local(),
                handle.get_remote(),
                e);
        }

        error!("Mqtt session timeout for mqtt by quic, uid: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
            handle.get_uid(),
            handle.get_remote(),
            handle.get_local(),
            result);

        async move {

        }.boxed_local()
    }
}

// 更新当前会话的连接超时时长
fn update_timeout(connect: &SocketHandle,
                  client_id: String,
                  keep_alive: u16) {
    //设置当前会话超时时长，一般为keep_alive的1.5倍
    let mut event = SocketEvent::empty();
    event.set::<String>(client_id);
    connect.set_timeout(keep_alive as usize * 1500, event);
}

// 发送指定的Mqtt报文，一般用于报文回应
fn send_packet(connect: &SocketHandle,
               packet: &Packet) -> Result<()> {
    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    if let Err(e) = buf.write_packet(packet) {
        //序列化Mqtt报文失败
        return Err(Error::new(ErrorKind::InvalidData,
                              format!("Mqtt send failed, reason: {:?}",
                                      e)));
    }

    //通过Ws连接发送指定报文
    connect.write_ready(connect.get_main_stream_id().unwrap().clone(),
                        buf.into_inner())
}

/// 广播指定的Mqtt报文，用于发布消息的发送
pub fn broadcast_packet(connects: &[SocketHandle],
                        packet: &Packet) -> Result<()> {
    if connects.len() == 0 {
        //Ws连接列表为空，则忽略
        return Ok(());
    }

    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    if let Err(e) = buf.write_packet(packet) {
        //序列化Mqtt报文失败
        return Err(Error::new(ErrorKind::InvalidData,
                              format!("Mqtt send failed, reason: {:?}",
                                      e)));
    }

    //通过Ws连接列表广播指定报文
    for connect in connects {
        if connect.is_closed() {
            continue;
        }

        return SocketHandle::broadcast(connects,
                                       buf.into_inner());
    }

    Ok(())
}

// 接受Mqtt连接请求
async fn accept(protocol: QuicMqtt311,
                connect: SocketHandle,
                packet: Connect) -> Result<()> {
    let clean_session = packet.clean_session;
    let client_id = packet.client_id.clone();
    let is_exist_session = protocol.is_exist(&client_id); //指定客户端会话是否存在

    if is_exist_session {
        //当前客户端会话存在
        if let Some(session) = protocol.broker.get_session(&client_id) {
            if session.is_accepted() {
                //当前客户端已连接，则立即返回错误原因
                return Err(Error::new(ErrorKind::AlreadyExists,
                                      "Mqtt connect failed, reason: connect already exist"));
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
    if (level != QuicMqtt311::MQTT31) && (level != QuicMqtt311::MQTT311) {
        //协议等级不匹配，则回应，并返回错误原因
        let ack = Connack {
            session_present,
            code: ConnectReturnCode::RefusedProtocolVersion,
        };

        if let Err(e) = send_packet(&connect, &Packet::Connack(ack)) {
            //发送回应失败，则立即返回错误原因
            return Err(Error::new(ErrorKind::BrokenPipe,
                                  format!("Mqtt send failed by connect, reason: {:?}",
                                          e)));
        }

        return Err(Error::new(ErrorKind::ConnectionAborted,
                              "Mqtt connect failed, reason: invalid protocol version"));
    }

    //在Ws连接会话中绑定当前连接与当前客户端会话
    connect.set_session(QuicBrokerSession::new(
        client_id.clone(),
        packet.keep_alive,
        packet.clean_session,
        packet.username.clone(),
        packet.password.clone())
    );

    //重置当前客户端会话
    let mut code = ConnectReturnCode::Accepted; //默认连接成功
    let mqtt_connect = reset_session(&protocol,
                                     connect.clone(),
                                     client_id,
                                     packet);
    if let Some(listener) = protocol.broker.get_listener() {
        //指定的监听器存在，则执行已连接处理
        if let Err(_e) = listener
            .0
                .connected(MqttBrokerProtocol::QuicMqtt311(Arc::new(protocol)),
                       mqtt_connect).await {
            //连接检查失败，则立即回应连接未授权
            code = ConnectReturnCode::NotAuthorized
        }
    }

    //回应连接已接受
    let ack = Connack {
        session_present,
        code,
    };
    if let Err(e) = send_packet(&connect, &Packet::Connack(ack)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe,
                              format!("Mqtt send failed by connect, reason: {:?}",
                                      e)));
    }

    Ok(())
}

// 创建新会话，并重置会话
fn reset_session(protocol: &QuicMqtt311,
                 connect: SocketHandle,
                 client_id: String,
                 packet: Connect) -> Arc<QosZeroSession> {
    let mut session = QosZeroSession::with_client_id(client_id.clone());
    session.bind_connect(connect.clone());
    session.set_accept(true);
    session.set_clean(packet.clean_session);
    if let Some(w) = &packet.last_will {
        session.set_will(w.topic.clone(), w.message.clone(), w.qos.to_u8(), w.retain);
    }
    session.set_user_pwd(packet.username, packet.password);
    session.set_keep_alive(packet.keep_alive);

    update_timeout(&connect, packet.client_id, packet.keep_alive);

    protocol.broker.insert_session(client_id, session)
}

// 获取Ws连接绑定的客户端id和客户端保持时长
#[inline(always)]
fn get_client_context(connect: &SocketHandle) -> Result<(String, u16)> {
    if let Some(handle) = connect.get_session::<QuicBrokerSession>() {
        //Ws连接绑定的客户端存在
        let h = handle.as_ref();
        Ok((h.get_client_id().clone(), h.get_keep_alive()))
    } else {
        //Ws连接绑定的客户端不存在，则立即返回错误原因
        Err(Error::new(ErrorKind::ConnectionRefused,
                       "Mqtt subscribe failed, reason: invalid connect"))
    }
}

// 发布消息
async fn publish(protocol: QuicMqtt311,
                 connect: SocketHandle,
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
        return Err(Error::new(ErrorKind::InvalidData,
                              "Mqtt publish failed, reason: invalid qos"));
    }

    let topic_path = if let Ok(path) = TopicPath::from_str(packet.topic_name.as_str()) {
        //解析发布的主题成功
        path
    } else {
        //解析发布的主题失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::InvalidData,
                              format!("Mqtt publish failed, topic: {:?}, reason: parse topic error",
                                      packet.topic_name)));
    };
    if topic_path.wildcards || topic_path.path.is_empty() {
        //发布消息的主题为空，或者有通匹符，则立即返回错误原因
        return Err(Error::new(ErrorKind::InvalidData,
                              "Mqtt publish failed, reason: invalid topic"));
    }

    //修改需要待发送的发布消息报文，并向订阅了指定主题的客户端发送当前报文
    packet.dup = false;
    let mut retain = None;
    if packet.retain {
        //需要缓存当前主题的最近一条发布消息
        retain = Some(packet.clone());
    }

    if let Some(service) = protocol.broker.get_service() {
        //如果有服务，则只执行服务
        if let Some(session) = protocol.broker.get_session(&client_id) {
            let is_passive_receive = session.is_passive_receive();
            let result = service
                .0
                .publish(MqttBrokerProtocol::QuicMqtt311(Arc::new(protocol)),
                         session.clone(),
                         packet.topic_name.clone(),
                         packet.payload).await;
            if is_passive_receive {
                //需要被动接收，则立即挂起连接，并等待服务回应
                if let Some(hibernate) = connect
                    .hibernate(connect.get_main_stream_id().unwrap().clone(),
                               QuicSocketReady::Writable) {
                    return hibernate.await;
                } else {
                    //连接已关闭，则立即退出
                    return Ok(());
                }
            }

            return result;
        }
    }

    //如果指定主题没有服务，则执行标准发布
    let is_public = true; //是否为公共主题

    if let Some(sessions) = protocol.broker.subscribed(is_public,
                                                       &topic_path.path,
                                                       qos,
                                                       retain) {
        //指定主题有订阅
        let mut connects: Vec<SocketHandle> = Vec::with_capacity(sessions.len());
        for session in sessions {
            //返回会话绑定的Ws连接
            if let Some(connect) = session.get_connect() {
                connects.push(connect.clone());
            }
        };
        if let Err(e) = broadcast_packet(&connects[..],
                                         &Packet::Publish(packet)) {
            //发布消息失败，则立即返回错误原因
            return Err(Error::new(ErrorKind::BrokenPipe,
                                  format!("Mqtt broadcast failed by publish, reason: {:?}",
                                          e)));
        }
    }

    Ok(())
}

// 订阅主题
async fn subscribe(protocol: QuicMqtt311,
                   connect: SocketHandle,
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
    if let Some(service) = protocol.broker.get_service() {
        //如果有服务，则只执行服务
        if let Some(session) = protocol.broker.get_session(&client_id) {
            let mut sub_topics = Vec::with_capacity(topics.len());
            for topic in &topics {
                sub_topics.push((topic.topic_path.clone(), topic.qos.to_u8()));
            }

            if let Err(_) = service
                .0
                .subscribe(MqttBrokerProtocol::QuicMqtt311(Arc::new(protocol.clone())),
                           session,
                           sub_topics).await {
                //返回订阅主题失败
                for _ in topics {
                    return_codes.push(SubscribeReturnCodes::Failure);
                }
            } else {
                //返回订阅主题成功
                let qos = protocol.get_qos();
                for _ in topics {
                    return_codes.push(SubscribeReturnCodes::Success(qos));
                }
            }
        }
    } else {
        //没有服务，则订阅指定主题
        if let Some(session) = protocol.broker.get_session(&client_id) {
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

                retains = protocol.broker.subscribe(session.clone(), topic.topic_path);
            }
        }
    }

    //回应订阅主题
    let ack = Suback {
        pkid,
        return_codes,
    };
    if let Err(e) = send_packet(&connect, &Packet::Suback(ack)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe,
                              format!("Mqtt send failed by subscribe, reason: {:?}",
                                      e)));
    }

    if let Some(r) = retains {
        //已订阅主题有最新发布的消息，则发布订阅主题的最新消息
        match r {
            Retain::Single(p) => {
                let _ = send_packet(&connect, &Packet::Publish(p));
            },
            Retain::Mutil(ps) => {
                for p in ps {
                    let _ = send_packet(&connect, &Packet::Publish(p));
                }
            },
        }
    }

    Ok(())
}

// 退订主题
async fn unsubscribe(protocol: QuicMqtt311,
                     connect: SocketHandle,
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
    if let Some(service) = protocol.broker.get_service() {
        //如果有服务，则只执行服务
        if let Some(session) = protocol.broker.get_session(&client_id) {
            if let Err(e) = service.0.unsubscribe(MqttBrokerProtocol::QuicMqtt311(Arc::new(protocol)),
                                                  session,
                                                  topics.clone()).await {
                return Err(Error::new(ErrorKind::BrokenPipe,
                                      format!("Mqtt unsubscribe failed, reason: {:?}",
                                              e)));
            }
        }
    } else {
        //没有服务，则取消订阅
        if let Some(session) = protocol.broker.get_session(&client_id) {
            //客户端的会话存在，则开始退订主题
            for topic in topics {
                protocol.broker.unsubscribe(&session, topic);
            }
        }
    }

    //回应退订主题
    if let Err(e) = send_packet(&connect, &Packet::Unsuback(pkid)) {
        //发送回应失败，则立即返回错误原因
        return Err(Error::new(ErrorKind::BrokenPipe,
                              format!("Mqtt send failed by unsubscribe, reason: {:?}",
                                      e)));
    }

    Ok(())
}

// 处理ping请求
fn ping_req(connect: SocketHandle) -> Result<()> {
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
        return Err(Error::new(ErrorKind::BrokenPipe, format!("Mqtt send failed by ping, reason: {:?}", e)));
    }

    Ok(())
}

// 处理关闭连接请求
fn disconnect(connect: SocketHandle) -> Result<()> {
    connect.close(0, Ok(()))
}

impl QuicMqtt311 {
    pub const MQTT31: u8 = 0x3;                                 //Mqtt3.1的协议等级
    pub const MQTT311: u8 = 0x4;                                //Mqtt3.1.1的协议等级
    pub const MAX_QOS: u8 = 0;                                  //支持的最大Qos

    //构建指定协议名和支持的最大Qos，且基于Websocket的Mqtt3.1.1协议
    pub fn with_name(broker_name: &str,
                     qos: u8) -> Self {
        QuicMqtt311 {
            broker_name: broker_name.to_string(),
            qos: QoS::from_u8(qos).unwrap(),
            broker: MqttBroker::new(),
        }
    }

    //判断指定ClientId的会话是否存在
    #[inline(always)]
    pub fn is_exist(&self, client_id: &str) -> bool {
        self.broker.is_exist_session(&client_id.to_string())
    }

    //获取Mqtt代理名
    pub fn get_broker_name(&self) -> &str {
        self.broker_name.as_str()
    }

    //获取支持的最大Qos
    #[inline(always)]
    pub fn get_qos(&self) -> QoS {
        self.qos
    }

    //获取内部代理
    pub fn get_broker(&self) -> &MqttBroker {
        &self.broker
    }
}