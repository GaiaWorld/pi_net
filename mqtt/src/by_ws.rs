use std::sync::Arc;
use std::marker::PhantomData;
use std::io::{Cursor, Result, ErrorKind, Error};

use mqtt311::{MqttWrite, MqttRead, Protocol, ConnectReturnCode, Packet, Connect,
              Connack, QoS, Publish, PacketIdentifier, SubscribeTopic, SubscribeReturnCodes,
              Subscribe, Suback, Unsubscribe};

use hash::XHashMap;

use tcp::{server::AsyncWaitsHandle,
          driver::Socket};
use ws::{connect::WsSocket,
         util::{ChildProtocol, ChildProtocolFactory, WsFrameType, WsContext}};

use crate::session::{MqttSession, QosZeroSession};

/*
* 基于Websocket的Mqtt3.1.1协议
*/
pub struct WsMqttProtocol<E: MqttSession> {
    name:       String,                     //协议名
    sessions:   XHashMap<String, Arc<E>>,   //会话表
}

impl<S: Socket, E: MqttSession> ChildProtocol<S, AsyncWaitsHandle> for WsMqttProtocol<E> {
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
                    Packet::Connect(con) => {
                        handle_connect(self, &connect, con)
                    },
                    _ => Ok(()),
                }
            },
        }
    }
}

//发送指定的Mqtt报文
fn send_packet<S: Socket, E: MqttSession>(protocol: &WsMqttProtocol<E>, connect: &WsSocket<S, AsyncWaitsHandle>, packet: &Packet) -> Result<()> {
    let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    if let Err(e) = buf.write_packet(packet) {
        //序列化Mqtt报文失败
        return Err(Error::new(ErrorKind::InvalidData, format!("mqtt send failed, reason: {}", e)));
    }

    //通过Ws连接发送指定报文
    let mut payload = connect.alloc();
    payload.get_iolist_mut().push_back(buf.into_inner().into());
    connect.send(WsMqttProtocol::<E>::WS_MSG_TYPE, payload)
}

//处理Mqtt连接请求
fn handle_connect<S: Socket, E: MqttSession>(protocol: &WsMqttProtocol<E>, connect: &WsSocket<S, AsyncWaitsHandle>, packet: Connect) -> Result<()> {
    if let Protocol::MQTT(level) = packet.protocol {
        if level != WsMqttProtocol::<E>::LEVEL {
            //协议等级不匹配，则回应，并返回错误原因
            let mut session_present = true;
            if packet.clean_session {
                //如果需要在关闭连接后清理会话，则重置保留标记
                session_present = false;
            }
            if session_present && !protocol.is_exist(&packet.client_id) {
                //如果当前需要保留会话，且当前会话表中没有指定ClientId的会话，则重置保留标记
                session_present = false;
            }

            let ack = Connack {
                session_present,
                code: ConnectReturnCode::RefusedProtocolVersion,
            };

            if let Err(e) = send_packet(protocol, connect, &Packet::Connack(ack)) {
                //发送回应失败，则立即返回错误原因
                return Err(Error::new(ErrorKind::BrokenPipe, format!("mqtt send failed by connect, reason: {:?}", e)));
            }

            return Err(Error::new(ErrorKind::ConnectionAborted, "mqtt connect failed, reason: invalid protocol version"));
        }
    }

    Ok(())
}

impl<E: MqttSession> WsMqttProtocol<E> {
    pub const WS_MSG_TYPE: WsFrameType = WsFrameType::Binary; //Mqtt3.1.1的Websocket消息类型
    pub const LEVEL: u8 = 0x4; //Mqtt3.1.1的协议等级

    //构建指定协议名的基于Websocket的Mqtt3.1.1协议
    pub fn with_name(name: &str) -> Self {
        WsMqttProtocol {
            name: name.to_lowercase(),
            sessions: XHashMap::default(),
        }
    }

    //判断指定ClientId的会话是否存在
    #[inline(always)]
    pub fn is_exist(&self, client_id: &str) -> bool {
        self.sessions.contains_key(client_id)
    }
}

/*
* 基于Websocket的Mqtt协议工厂
*/
pub struct WsMqttProtocolFactory<S: Socket> {
    name:   String,                 //协议名
    marker: PhantomData<S>,
}

impl<S: Socket> ChildProtocolFactory for WsMqttProtocolFactory<S> {
    type Connect = S;
    type Waits = AsyncWaitsHandle;

    fn new_protocol(&self) -> Arc<dyn ChildProtocol<Self::Connect, Self::Waits>> {
        Arc::new(WsMqttProtocol::<QosZeroSession>::with_name(&self.name))
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