use std::sync::{Arc, RwLock};
use std::collections::BTreeMap;

use dashmap::DashMap;
use mqtt311::TopicPath;

use atom::Atom;
use tcp::driver::Socket;

use crate::{session::{MqttSession, QosZeroSession}, util::PathTree};

/*
* Mqtt代理
*/
#[derive(Clone)]
pub struct MqttBroker<S: Socket> {
    sessions:   Arc<DashMap<String, Arc<QosZeroSession<S>>>>,               //会话表
    sub_tab:    Arc<DashMap<String, BTreeMap<Arc<QosZeroSession<S>>, u8>>>, //会话订阅表
    patterns:   Arc<RwLock<PathTree<Arc<QosZeroSession<S>>>>>,              //订阅模式表
    topics:     Arc<RwLock<Vec<TopicPath>>>,                                //服务端已发布的通用主题表
}

unsafe impl<S: Socket> Send for MqttBroker<S> {}
unsafe impl<S: Socket> Sync for MqttBroker<S> {}

impl<S: Socket> MqttBroker<S> {
    //构建Mqtt代理
    pub fn new() -> Self {
        MqttBroker {
            sessions: Arc::new(DashMap::default()),
            sub_tab: Arc::new(DashMap::default()),
            patterns: Arc::new(RwLock::new(PathTree::empty())),
            topics: Arc::new(RwLock::new(Vec::new())),
        }
    }

    //会话数量
    pub fn session_size(&self) -> usize {
        self.sessions.len()
    }

    //已订阅的主题数
    pub fn sub_size(&self) -> usize {
        self.sub_tab.len()
    }

    //已订阅的主题模式的会话数量
    pub fn pattern_size(&self) -> usize {
        self.patterns.read().unwrap().len()
    }

    //服务端已发布的通用主题数
    pub fn gen_topic_size(&self) -> usize {
        self.topics.read().unwrap().len()
    }

    //判断指定客户端id的会话是否存在
    pub fn is_exist_session(&self, client_id: &String) -> bool {
        self.sessions.contains_key(client_id)
    }

    //获取指定会话
    pub fn get_session(&self, client_id: &String) -> Option<Arc<QosZeroSession<S>>> {
        if let Some(r) = self.sessions.get(client_id) {
            return Some(r.clone());
        }

        None
    }

    //插入指定会话
    pub fn insert_session(&self, client_id: String, session: QosZeroSession<S>) {
        self.sessions.insert(client_id, Arc::new(session));
    }

    //获取已订阅指定主题的会话
    pub fn subscribed(&self, topic: &String) -> Option<Vec<Arc<QosZeroSession<S>>>> {
        if let Some(r) = self.sub_tab.get(topic) {
            return Some(r.keys().cloned().collect::<Vec<Arc<QosZeroSession<S>>>>());
        }

        None
    }

    //为指定会话订阅指定主题
    pub fn subscribe(&self, session: Arc<QosZeroSession<S>>, topic: String, qos: u8) {
        let path = TopicPath::from(topic.clone());
        if path.wildcards {
            //订阅的是主题模式
        } else {
            //订阅的是主题
            match self.sub_tab.get_mut(&topic) {
                None => {
                    //当前主题，没有任何会话订阅，则初始化指定主题的会话表
                    let mut map = BTreeMap::new();
                    map.insert(session, qos);
                    self.sub_tab.insert(topic, map);
                },
                Some(mut map) => {
                    //当前主题，有会话订阅
                    map.insert(session, qos);
                },
            }
        }
    }

    //为指定会话退订指定主题
    pub fn unsubscribe(&self, session: &Arc<QosZeroSession<S>>, topic: String) {
        let path = TopicPath::from(topic.clone());
        if path.wildcards {
            //退订的是主题模式
        } else {
            //退订的是主题
            match self.sub_tab.get_mut(&topic) {
                None => {
                    //当前主题，没有任何会话订阅，则忽略
                   return;
                },
                Some(mut map) => {
                    //当前主题，有会话订阅
                    map.remove(session);
                },
            }
        }
    }
}