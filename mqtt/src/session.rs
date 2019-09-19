use std::cmp::Ordering;

use dashmap::DashMap;
use mqtt311::{QoS, LastWill};

use atom::Atom;

use tcp::{server::AsyncWaitsHandle, driver::Socket};
use ws::connect::WsSocket;

/*
* Mqtt会话
*/
pub trait MqttSession: Default + Clone + Ord + Send + Sync + 'static {
    type Connect;

    //获取连接
    fn get_connect(&self) -> Option<&Self::Connect>;

    //绑定连接
    fn bind_connect(&mut self, connect: Self::Connect);

    //解绑定连接
    fn unbind_connect(&mut self) -> Option<Self::Connect>;

    //判断是否已接受连接
    fn is_accepted(&self) -> bool;

    //设置是否已连接
    fn set_accept(&mut self, connect: bool);

    //是否清理会话
    fn is_clean(&self) -> bool;

    //设置是否清理会话
    fn set_clean(&mut self, clean: bool);

    //获取Will
    fn get_will(&self) -> Option<(&str, &str, u8, bool)>;

    //设置Will
    fn set_will(&mut self, topic: String, msg: String, qos: u8, retain: bool);

    //取消息Will
    fn unset_will(&mut self) -> Option<(String, String, u8, bool)>;

    //获取用户
    fn get_user(&self) -> Option<&str>;

    //获取用户密码
    fn get_pwd(&self) -> Option<&str>;

    //设置用户名和密码
    fn set_user_pwd(&mut self, user: Option<String>, pwd: Option<String>);

    //获取未发送的Mqtt报文
    fn unsend_packet(&self) -> Option<&[Vec<u8>]> {
        None
    }

    //获取已发送但未确认的Mqtt报文
    fn unconfirm_sended(&self) -> Option<&[Vec<u8>]> {
        None
    }

    //获取已接收但未确认的Mqtt报文
    fn unconfirm_received(&self) -> Option<&[Vec<u8>]> {
        None
    }

    //获取连接保持间隔时长
    fn get_keep_alive(&self) -> u16;

    //设置连接保持间隔时长
    fn set_keep_alive(&mut self, keep_alive: u16);
}

/*
* Qos0的Mqtt会话
*/
pub struct QosZeroSession<S: Socket> {
    connect:        Option<WsSocket<S, AsyncWaitsHandle>>,  //Websocket连接
    is_accepted:    bool,                                   //是否已连接
    is_clean:       bool,                                   //是否清理会话
    will:           Option<LastWill>,                       //Will
    user:           Option<String>,                         //会话用户
    pwd:            Option<String>,                         //会话用户密码
    keep_alive:     u16,                                    //连接保持间隔时长，单位秒，服务器端在1.5倍间隔时长内没有收到任何控制报文，则主动关闭连接
}

unsafe impl<S: Socket> Send for QosZeroSession<S> {}
unsafe impl<S: Socket> Sync for QosZeroSession<S> {}

impl<S: Socket> PartialEq<QosZeroSession<S>> for QosZeroSession<S> {
    fn eq(&self, other: &Self) -> bool {
        if let Some(ws) = &self.connect {
            if let Some(ws_other) = &other.connect {
                return ws.get_token().eq(&ws_other.get_token())
            }
        }

        false
    }
}

impl<S: Socket> Eq for QosZeroSession<S> {}

impl<S: Socket> PartialOrd<QosZeroSession<S>> for QosZeroSession<S> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if let Some(ws) = &self.connect {
            if let Some(ws_other) = &other.connect {
                ws.get_token().partial_cmp(&ws_other.get_token())
            } else {
                Some(Ordering::Greater)
            }
        } else if other.connect.is_none() {
            Some(Ordering::Equal)
        } else {
            Some(Ordering::Less)
        }
    }
}

impl<S: Socket> Ord for QosZeroSession<S> {
    fn cmp(&self, other: &Self) -> Ordering {
        if let Some(ws) = &self.connect {
            if let Some(ws_other) = &other.connect {
                ws.get_token().cmp(&ws_other.get_token())
            } else {
                Ordering::Greater
            }
        } else if other.connect.is_none() {
            Ordering::Equal
        } else {
            Ordering::Less
        }
    }
}

impl<S: Socket> Clone for QosZeroSession<S> {
    fn clone(&self) -> Self {
        QosZeroSession {
            connect: self.connect.clone(),
            is_accepted: self.is_accepted,
            is_clean: self.is_clean,
            will: self.will.clone(),
            user: self.user.clone(),
            pwd: self.pwd.clone(),
            keep_alive: self.keep_alive,
        }
    }
}

impl<S: Socket> Default for QosZeroSession<S> {
    fn default() -> Self {
        QosZeroSession {
            connect: None,
            is_accepted: false,
            is_clean: false,
            will: None,
            user: None,
            pwd: None,
            keep_alive: 0,
        }
    }
}

impl<S: Socket> MqttSession for QosZeroSession<S> {
    type Connect = WsSocket<S, AsyncWaitsHandle>;

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
        self.is_accepted
    }

    fn set_accept(&mut self, accept: bool) {
        self.is_accepted = accept;
    }

    fn is_clean(&self) -> bool {
        self.is_clean
    }

    fn set_clean(&mut self, clean: bool) {
        self.is_clean = clean;
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