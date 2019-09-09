use std::collections::HashMap;

use fnv::FnvBuildHasher;

use atom::Atom;

/*
* Mqtt会话
*/
pub trait MqttSession: Send + Sync + 'static {
    //判断会话是否已订阅了指定主题
    fn is_subscribed(&self, topic: &Atom) -> bool;

    //获取会话订阅的所有主题
    fn topics(&self) -> Option<Vec<Atom>>;

    //订阅指定主题
    fn subscribe(&mut self, topic: Atom);

    //取消订阅指定的主题
    fn unsubscribe(&mut self, topic: &Atom);

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
}

/*
* Qos0的Mqtt会话
*/
pub struct QosZeroSession {
    sub_tab:    HashMap<Atom, (), FnvBuildHasher>,  //订阅表
}

unsafe impl Send for QosZeroSession {}
unsafe impl Sync for QosZeroSession {}

impl MqttSession for QosZeroSession {
    fn is_subscribed(&self, topic: &Atom) -> bool {
        self.sub_tab.contains_key(topic)
    }

    fn topics(&self) -> Option<Vec<Atom>> {
        if self.sub_tab.len() == 0 {
            return None;
        }

        Some(self.sub_tab.keys().map(|topic| {
            topic.clone()
        }).collect())
    }

    fn subscribe(&mut self, topic: Atom) {
        self.sub_tab.insert(topic, ());
    }

    fn unsubscribe(&mut self, topic: &Atom) {
        self.sub_tab.remove(topic);
    }
}

impl QosZeroSession {
    //构建Qos0的Mqtt会话
    pub fn new() -> Self {
        QosZeroSession {
            sub_tab: HashMap::with_hasher(FnvBuildHasher::default()),
        }
    }
}