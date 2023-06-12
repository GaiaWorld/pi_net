use std::sync::Arc;
use std::cmp::Ordering;
use std::net::SocketAddr;
use std::fmt::{self, Debug};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::io::{Cursor, Result, Error, ErrorKind};

use mqtt311::{MqttWrite, QoS, LastWill, Packet, Publish};

use quic::{SocketHandle,
           utils::{ContextHandle, Hibernate, QuicSocketReady}};

use crate::{utils::{ValueEq, QuicBrokerSession}};

///
/// Mqtt会话
///
pub trait MqttSession: Debug + Send + Sync + 'static {
    type Connect;

    /// 获取连接
    fn get_connect(&self) -> Option<&Self::Connect>;

    /// 绑定连接
    fn bind_connect(&mut self, connect: Self::Connect);

    /// 解绑定连接
    fn unbind_connect(&mut self) -> Option<Self::Connect>;

    /// 判断是否已接受连接
    fn is_accepted(&self) -> bool;

    /// 设置是否已连接
    fn set_accept(&self, connect: bool);

    /// 是否清理会话
    fn is_clean(&self) -> bool;

    /// 设置是否清理会话
    fn set_clean(&self, clean: bool);

    /// 获取Will
    fn get_will(&self) -> Option<(&str, &str, u8, bool)>;

    /// 设置Will
    fn set_will(&mut self, topic: String, msg: String, qos: u8, retain: bool);

    /// 取消息Will
    fn unset_will(&mut self) -> Option<(String, String, u8, bool)>;

    /// 获取用户
    fn get_user(&self) -> Option<&str>;

    /// 获取用户密码
    fn get_pwd(&self) -> Option<&str>;

    /// 设置用户名和密码
    fn set_user_pwd(&mut self, user: Option<String>, pwd: Option<String>);

    /// 获取未发送的Mqtt报文
    fn unsend_packet(&self) -> Option<&[Vec<u8>]> {
        None
    }

    /// 获取已发送但未确认的Mqtt报文
    fn unconfirm_sended(&self) -> Option<&[Vec<u8>]> {
        None
    }

    /// 获取已接收但未确认的Mqtt报文
    fn unconfirm_received(&self) -> Option<&[Vec<u8>]> {
        None
    }

    /// 获取连接保持间隔时长
    fn get_keep_alive(&self) -> u16;

    /// 设置连接保持间隔时长
    fn set_keep_alive(&mut self, keep_alive: u16);
}

///
/// Mqtt连接
///
pub trait MqttConnect: Debug + Send + Sync + 'static {
    /// 获取Tcp连接唯一id
    fn get_uid(&self) -> Option<usize>;

    /// 获取本地地址
    fn get_local_addr(&self) -> Option<SocketAddr>;

    /// 获取远端地址
    fn get_remote_addr(&self) -> Option<SocketAddr>;

    /// 判断是否是安全的连接
    fn is_security(&self) -> bool {
        true
    }

    /// 是否被接收消息
    fn is_passive_receive(&self) -> bool;

    /// 设置是否被动接收消息
    fn passive_receive(&self, b: bool);

    /// 获取连接的代理会话
    fn get_session(&self) -> Option<ContextHandle<QuicBrokerSession>>;

    /// 发送消息
    fn send(&self, topic: &String, payload: Arc<Vec<u8>>) -> Result<()>;

    /// 线程安全的异步休眠当前连接，直到被唤醒，返回空表示连接已关闭
    fn hibernate(&self, ready: QuicSocketReady) -> Option<Hibernate>;

    /// 线程安全的唤醒被休眠的当前连接，如果当前连接未被休眠，则忽略
    fn wakeup(&self, result: Result<()>) -> bool;

    /// 关闭连接
    fn close(&self, code: u32, reason: Result<()>) -> Result<()>;
}

///
/// Qos0的Mqtt会话
///
pub struct QosZeroSession {
    connect:        Option<SocketHandle>,    //Quic连接
    is_passive:     Arc<AtomicBool>,            //是否被动接收消息
    client_id:      String,                     //客户端id
    is_accepted:    Arc<AtomicBool>,            //是否已连接
    is_clean:       Arc<AtomicBool>,            //是否清理会话
    will:           Option<LastWill>,           //Will
    user:           Option<String>,             //会话用户
    pwd:            Option<String>,             //会话用户密码
    keep_alive:     u16,                        //连接保持间隔时长，单位秒，服务器端在1.5倍间隔时长内没有收到任何控制报文，则主动关闭连接
}

unsafe impl Send for QosZeroSession {}
unsafe impl Sync for QosZeroSession {}

impl PartialEq<QosZeroSession> for QosZeroSession {
    fn eq(&self, other: &Self) -> bool {
        self.client_id.eq(&other.client_id)
    }
}

impl Eq for QosZeroSession {}

impl PartialOrd<QosZeroSession> for QosZeroSession {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.client_id.partial_cmp(&other.client_id)
    }
}

impl Ord for QosZeroSession {
    fn cmp(&self, other: &Self) -> Ordering {
        self.client_id.cmp(&other.client_id)
    }
}

impl Hash for QosZeroSession {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if let Some(ws) = &self.connect {
            ws.get_uid().hash(state);
        }
    }
}

impl Debug for QosZeroSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QosZeroSession {{ client_id: {:?}, is_accepted: {}, is_clean: {}, keep_alive: {} }}",
               self.client_id,
               self.is_accepted.load(AtomicOrdering::Relaxed),
               self.is_clean.load(AtomicOrdering::Relaxed),
               self.keep_alive)
    }
}

impl Clone for QosZeroSession {
    fn clone(&self) -> Self {
        QosZeroSession {
            connect: self.connect.clone(),
            is_passive: self.is_passive.clone(),
            client_id: self.client_id.clone(),
            is_accepted: self.is_accepted.clone(),
            is_clean: self.is_clean.clone(),
            will: self.will.clone(),
            user: self.user.clone(),
            pwd: self.pwd.clone(),
            keep_alive: self.keep_alive,
        }
    }
}

impl ValueEq for Arc<QosZeroSession> {
    fn value_eq(this: &Self, other: &Self) -> bool {
        Arc::ptr_eq(this, other)
    }
}

impl MqttSession for QosZeroSession {
    type Connect = SocketHandle;

    fn get_connect(&self) -> Option<&Self::Connect> {
        self.connect.as_ref()
    }

    fn bind_connect(&mut self, connect: Self::Connect) {
        self.connect = Some(connect);
    }

    fn unbind_connect(&mut self) -> Option<Self::Connect> {
        self.connect.take()
    }

    fn is_accepted(&self) -> bool {
        self.is_accepted.load(AtomicOrdering::Relaxed)
    }

    fn set_accept(&self, accept: bool) {
        self.is_accepted.store(accept, AtomicOrdering::SeqCst);
    }

    fn is_clean(&self) -> bool {
        self.is_clean.load(AtomicOrdering::Relaxed)
    }

    fn set_clean(&self, clean: bool) {
        self.is_clean.store(clean, AtomicOrdering::SeqCst);
    }

    fn get_will(&self) -> Option<(&str, &str, u8, bool)> {
        if let Some(w) = &self.will {
            return Some((&w.topic, &w.message, w.qos.to_u8(), w.retain));
        }

        None
    }

    fn set_will(&mut self, topic: String, message: String, qos: u8, retain: bool) {
        self.will = Some(LastWill {
            topic,
            message,
            qos: QoS::from_u8(qos).unwrap(),
            retain,
        });
    }

    fn unset_will(&mut self) -> Option<(String, String, u8, bool)> {
        if let Some(w) = self.will.take() {
            return Some((w.topic, w.message, w.qos.to_u8(), w.retain));
        }

        None
    }

    fn get_user(&self) -> Option<&str> {
        if let Some(u) = &self.user {
            return Some(u.as_str());
        }

        None
    }

    fn get_pwd(&self) -> Option<&str> {
        if let Some(p) = &self.pwd {
            return Some(p.as_str());
        }

        None
    }

    fn set_user_pwd(&mut self, user: Option<String>, pwd: Option<String>) {
        self.user = user;
        self.pwd = pwd;
    }

    fn get_keep_alive(&self) -> u16 {
        self.keep_alive
    }

    fn set_keep_alive(&mut self, keep_alive: u16) {
        self.keep_alive = keep_alive;
    }
}

impl MqttConnect for QosZeroSession {
    fn get_uid(&self) -> Option<usize> {
        if let Some(connect) = &self.connect {
            return Some(connect.get_uid());
        }

        None
    }

    fn get_local_addr(&self) -> Option<SocketAddr> {
        if let Some(connect) = &self.connect {
            return Some(connect.get_local().clone());
        }

        None
    }

    fn get_remote_addr(&self) -> Option<SocketAddr> {
        if let Some(connect) = &self.connect {
            return Some(connect.get_remote().clone());
        }

        None
    }

    fn is_passive_receive(&self) -> bool {
        self.is_passive.load(AtomicOrdering::Relaxed)
    }

    fn passive_receive(&self, b: bool) {
        self.is_passive.store(b, AtomicOrdering::SeqCst);
    }

    fn get_session(&self) -> Option<ContextHandle<QuicBrokerSession>> {
        if let Some(connect) = &self.connect {
            if connect.is_closed() {
                return None;
            }

            return connect.get_session::<QuicBrokerSession>();
        }

        None
    }

    fn send(&self, topic: &String, payload: Arc<Vec<u8>>) -> Result<()> {
        if let Some(connect) = &self.connect {
            //构建指定负载的报文
            let packet = Packet::Publish(Publish {
                dup: false,
                qos: QoS::AtMostOnce,
                retain: false,
                topic_name: topic.clone(),
                pkid: None,
                payload,
            });

            //序列化报文
            let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
            if let Err(e) = buf.write_packet(&packet) {
                //序列化Mqtt报文失败
                return Err(Error::new(ErrorKind::InvalidData,
                                      format!("Mqtt session send failed, reason: {:?}",
                                              e)));
            }

            //通过Ws连接发送指定报文
            return connect.write_ready(buf.into_inner());
        }

        Ok(())
    }

    fn hibernate(&self, ready: QuicSocketReady) -> Option<Hibernate> {
        if let Some(connect) = &self.connect {
            connect.hibernate(ready)
        } else {
            None
        }
    }

    fn wakeup(&self, result: Result<()>) -> bool {
        if let Some(connect) = &self.connect {
            connect.wakeup(result)
        } else {
            true
        }
    }

    fn close(&self, code: u32, reason: Result<()>) -> Result<()> {
        if let Some(connect) = &self.connect {
            return connect.close(code, reason);
        }

        Ok(())
    }
}

impl QosZeroSession {
    //构建指定客户端id的会话
    pub fn with_client_id(client_id: String) -> Self {
        QosZeroSession {
            connect: None,
            is_passive: Arc::new(AtomicBool::new(false)),
            client_id,
            is_accepted: Arc::new(AtomicBool::new(false)),
            is_clean: Arc::new(AtomicBool::new(false)),
            will: None,
            user: None,
            pwd: None,
            keep_alive: 0,
        }
    }
}