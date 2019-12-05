use std::sync::Arc;
use std::io::Result;

use parking_lot::RwLock;
use mqtt311::{TopicPath, Publish};

use hash::XHashMap;

use tcp::driver::Socket;

use crate::{session::{MqttSession, MqttConnect, QosZeroSession}, util::{PathTree, BrokerSession}};

/*
* Mqtt连接回应的系统主题
*/
lazy_static! {
    pub static ref MQTT_RESPONSE_SYS_TOPIC: String = "$r".to_string();
}

/*
* Mqtt代理监听器
*/
pub trait MqttBrokerListener {
    //处理Mqtt客户端连接事件
    fn connected(&self, connect: Arc<dyn MqttConnect>) -> Result<()>;

    //处理Mqtt客户端关闭事件
    fn closed(&self, connect: Arc<dyn MqttConnect>, context: BrokerSession, reason: Result<()>);
}

/*
* Mqtt代理服务
*/
pub trait MqttBrokerService {
    //指定Mqtt客户端请求的指定主题的服务
    fn request(&self, connect: Arc<dyn MqttConnect>, topic: String, payload: Arc<Vec<u8>>) -> Result<()>;
}

/*
* 保留的发布消息
*/
pub enum Retain {
    Single(Publish),        //单个保留的发布消息
    Mutil(Vec<Publish>),    //多个保留的发布消息
}

/*
* 订阅缓存，缓存订阅指定主题的会话，和主题的retain
*/
struct SubCache<S: Socket> {
    retain:     Option<Publish>,                //retain
    first:      Option<Arc<QosZeroSession<S>>>, //主题的首个订阅者
    sessions:   Vec<Arc<QosZeroSession<S>>>,    //会话表
}

impl<S: Socket> SubCache<S> {
    //构建指定会话和retain的订阅缓存
    pub fn with_session(first: Option<Arc<QosZeroSession<S>>>, retain: Option<Publish>) -> Self {
        SubCache {
            retain,
            first,
            sessions: Vec::new(),
        }
    }
}

/*
* Mqtt代理
*/
#[derive(Clone)]
pub struct MqttBroker<S: Socket> {
    listener:   Arc<RwLock<Option<Arc<dyn MqttBrokerListener>>>>,           //监听器，用于监听Mqtt连接和关闭事件
    services:   Arc<RwLock<XHashMap<String, Arc<dyn MqttBrokerService>>>>,  //服务表，保存指定主题的服务
    sessions:   Arc<RwLock<XHashMap<String, Arc<QosZeroSession<S>>>>>,      //会话表
    sub_tab:    Arc<RwLock<XHashMap<String, Arc<RwLock<SubCache<S>>>>>>,    //会话订阅表
    patterns:   Arc<RwLock<PathTree<Arc<QosZeroSession<S>>>>>,              //订阅模式表
    publics:    Arc<RwLock<Vec<(String, u8)>>>,                             //已发布的公共主题列表
    topics:     Arc<RwLock<XHashMap<Arc<QosZeroSession<S>>, Vec<String>>>>, //用户已订阅主题表
}

unsafe impl<S: Socket> Send for MqttBroker<S> {}
unsafe impl<S: Socket> Sync for MqttBroker<S> {}

impl<S: Socket> MqttBroker<S> {
    //构建Mqtt代理
    pub fn new() -> Self {
        MqttBroker {
            listener: Arc::new(RwLock::new(None)),
            services: Arc::new(RwLock::new(XHashMap::default())),
            sessions: Arc::new(RwLock::new(XHashMap::default())),
            sub_tab: Arc::new(RwLock::new(XHashMap::default())),
            patterns: Arc::new(RwLock::new(PathTree::empty())),
            publics: Arc::new(RwLock::new(Vec::new())),
            topics: Arc::new(RwLock::new(XHashMap::default())),
        }
    }

    //获取代理监听器
    pub fn get_listener(&self) -> Option<Arc<dyn MqttBrokerListener>> {
        self.listener.read().as_ref().cloned()
    }

    //注册代理监听器
    pub fn register_listener(&self, listener: Arc<dyn MqttBrokerListener>) {
        *self.listener.write() = Some(listener);
    }

    //获取指定主题的服务
    pub fn get_service(&self, topic: &String) -> Option<Arc<dyn MqttBrokerService>> {
        self.services.read().get(topic).cloned()
    }

    //注册指定主题的服务
    pub fn register_service(&self, topic: String, service: Arc<dyn MqttBrokerService>) {
        self.services.write().insert(topic, service);
    }

    //注销指定主题的服务
    pub fn unregister_service(&self, topic: &String) {
        self.services.write().remove(topic);
    }

    //会话数量
    pub fn session_size(&self) -> usize {
        self.sessions.read().len()
    }

    //已订阅的主题数
    pub fn sub_size(&self) -> usize {
        self.sub_tab.read().len()
    }

    //已订阅的主题模式的会话数量
    pub fn pattern_size(&self) -> usize {
        self.patterns.read().len()
    }

    //判断指定客户端id的会话是否存在
    pub fn is_exist_session(&self, client_id: &String) -> bool {
        self.sessions.read().contains_key(client_id)
    }

    //获取指定会话
    pub fn get_session(&self, client_id: &String) -> Option<Arc<QosZeroSession<S>>> {
        if let Some(r) = self.sessions.read().get(client_id) {
            return Some(r.clone());
        }

        None
    }

    //插入指定会话
    pub fn insert_session(&self, client_id: String, session: QosZeroSession<S>) -> Arc<QosZeroSession<S>> {
        let connect = Arc::new(session);
        self.sessions.write().insert(client_id, connect.clone());
        connect
    }

    //移除指定会话，返回被移除的会话
    pub fn remove_session(&self, client_id: &String) -> Option<Arc<QosZeroSession<S>>> {
        self.sessions.write().remove(client_id)
    }

    //获取已订阅指定主题的会话
    pub fn subscribed(&self, is_public: bool, topic: &String, qos: u8, retain: Option<Publish>) -> Option<Vec<Arc<QosZeroSession<S>>>> {
        let mut is_new_public = false; //是否是新的公共主题
        if is_public {
            //如果是公共主题
            let mut publics = self.publics.write();
            if let Err(index) = publics.binary_search_by(|(s, _)| {
                s.cmp(topic)
            }) {
                //不在公共主题列表中，则插入指定位置，保证列表有序
                publics.insert(index, (topic.clone(), qos));
                is_new_public = true;
            }
        }

        if self.sub_tab.read().get(topic).is_some() {
            //如果在订阅表中，则返回会话
            let cache = self.sub_tab.read().get(topic).unwrap().clone();
            if let Some(publish) = &retain {
                //如果当前主题需要缓存最新的发布消息，则缓存
                cache.write().retain = Some(publish.clone());
            }

            if is_new_public {
                //如果是新公共主题，强制从主题模式表中匹配，以防止通过订阅主题模式的会话没有注册到订阅表中
                if let Some(mut sessions) = self.patterns.read().lookup(TopicPath::from(topic.as_str())) {
                    //有匹配的主题模式表中的会话
                    let first = cache.read().first.clone();
                    match first {
                        None => {
                            //订阅表的当前主题有多个会话，则合并会话
                            cache.write().sessions.extend_from_slice(&sessions[..]);
                            sessions = cache.read().sessions.clone();
                        },
                        Some(session) => {
                            //订阅表的当前主题只有一个会话
                            sessions.push(session);

                            if sessions.len() > 0 {
                                //替换订阅缓存的会话列表
                                cache.write().first = None;
                                cache.write().sessions = sessions.clone();
                            }
                        },
                    }

                    return Some(sessions);
                }
            }

            //如果不是新公共主题，或没有匹配的主题模式，则立即返回订阅会话列表
            if let Some(session) = &cache.read().first {
                //订阅当前主题的会话只有一个，则返回
                return Some(vec![session.clone()]);
            }

            //订阅当前主题的会话也许有多个，则全部返回
            let sessions = cache.read().sessions.clone();
            return Some(sessions);
        } else {
            //如果不在订阅表中，则检查主题模式表，如果在主题模式表中订阅，则将会话加入订阅表，并返回会话
            if let Some(sessions) = self.patterns.read().lookup(TopicPath::from(topic.as_str())) {
                //在订阅表中创建新的主题，并将会话加入主题
                let len = sessions.len();
                match len {
                    0 => {
                        //没有任何订阅当前主题的会话，则忽略
                        let retain_copy = retain.clone();
                        self.sub_tab.write().entry(topic.clone()).or_insert_with(move || {
                            //锁住订阅表，进行初始化，保证线程安全
                            Arc::new(RwLock::new(SubCache::with_session(None, retain_copy)))
                        });

                        //线程安全的确认当前主题的订阅缓存为空，则初始化订阅表成功，并返回
                        return Some(sessions);
                    },
                    1 => {
                        //只有一个会话订阅了当前主题
                        let retain_copy = retain.clone();
                        let session = sessions[0].clone();
                        self.sub_tab.write().entry(topic.clone()).or_insert_with(move || {
                            //锁住订阅表，进行初始化，保证线程安全
                            Arc::new(RwLock::new(SubCache::with_session(Some(session), retain_copy)))
                        });

                        //线程安全的确认当前主题的订阅缓存为空，则初始化订阅表成功，并返回
                        return Some(sessions);
                    },
                    _ => {
                        //多个会话订阅了当前主题
                        let retain_copy = retain.clone();
                        let sessions_copy = sessions.clone();
                        self.sub_tab.write().entry(topic.clone()).or_insert_with(move || {
                            //锁住订阅表，进行初始化，保证线程安全
                            let mut cache = SubCache::with_session(None, retain_copy);
                            for session in &sessions_copy {
                                cache.sessions.push(session.clone());
                            }

                            Arc::new(RwLock::new(cache))
                        });

                        //线程安全的确认当前主题的订阅缓存为空，则初始化订阅表成功，并返回
                        return Some(sessions);
                    },
                }

                //线程安全的确认当前主题的订阅缓存不为空，则初始化订阅表失败，并重试
                self.subscribed(is_public, topic, qos, retain)
            } else {
                None
            }
        }
    }

    //为指定会话订阅指定主题，返回指定主题缓存的最新的发布消息
    pub fn subscribe(&self, session: Arc<QosZeroSession<S>>, topic: String) -> Option<Retain> {
        let path = TopicPath::from(topic.clone());
        if path.wildcards {
            //订阅的是主题模式，则匹配公共主题列表
            let mut keys = Vec::new();
            for (key, _) in self.publics.read().iter() {
                if is_match(&path, &TopicPath::from(key.as_str())) {
                    //当前公共主题与主题模式匹配
                    keys.push(key.clone()); //将当前会话加入订阅表中的指定公共主题
                    save_topic(self, &session, key); //记录当前会话订阅的公共主题
                }
            }
            save_topic(self, &session, &topic); //记录当前会话订阅的主题模式

            //将当前会话加入订阅表中的已匹配的公共主题
            let mut vec = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(Retain::Single(publish)) = self.subscribe(session.clone(), key) {
                    //当前订阅的主题有最新的发布消息，则记录
                    vec.push(publish);
                }
            }

            //将会话加入主题模式表
            self.patterns.write().insert(path, session);

            if vec.len() == 0 {
                None
            } else {
                Some(Retain::Mutil(vec))
            }
        } else {
            //订阅的是主题，则将会话加入主题订阅表
            if self.sub_tab.read().get(&topic).is_none() {
                //当前主题，没有任何会话订阅，则初始化指定主题的订阅缓存
                let session_copy = session.clone();
                self.sub_tab.write().entry(topic.clone()).or_insert_with(move || {
                    //锁住订阅表，进行初始化，保证线程安全
                    Arc::new(RwLock::new(SubCache::with_session(Some(session_copy), None)))
                });

                save_topic(self, &session, &topic); //记录当前会话订阅的主题

                //线程安全的确认当前主题的订阅缓存为空，则初始化订阅表成功，并返回
                return None;
            }

            //线程安全的确认当前主题，有会话订阅
            let cache = self.sub_tab.read().get(&topic).unwrap().clone();
            let mut w = cache.write();
            if let Some(session) = w.first.take() {
                //当前主题的缓存中只有一个订阅会话，则将会话移动到会话表中，首次插入无需排序
                w.sessions.push(session);
            }

            //将新会话加入当前主题的订阅缓存的会话表
            if let Err(index) = w.sessions.binary_search_by(|s| {
                s.cmp(&session)
            }) {
                //不在会话列表中
                save_topic(self, &session, &topic); //记录当前会话订阅的主题
                w.sessions.insert(index, session); //插入指定位置，保证列表有序
            }

            if let Some(retain) = w.retain.as_ref() {
                return Some(Retain::Single(retain.clone()));
            }

            None
        }
    }

    //为指定会话退订指定主题
    pub fn unsubscribe(&self, session: &Arc<QosZeroSession<S>>, topic: String) {
        let path = TopicPath::from(topic.clone());
        if path.wildcards {
            //退订的是主题模式，则移除匹配主题模式的指定会话
            let mut keys = Vec::new();
            for (key, cache) in self.sub_tab.read().iter() {
                if is_match(&path, &TopicPath::from(key.as_str())) {
                    //当前主题与退订的主题模式匹配，则退订当前主题的指定会话
                    let mut w = cache.write();
                    if w.first.is_some() {
                        //当前主题的只有一个订阅会话
                        if Arc::ptr_eq(session, &w.first.as_ref().unwrap()) {
                            //会话相同，则从订阅表中移除当前主题
                            w.first = None;
                            keys.push(key.clone());
                        }
                    } else {
                        //当前主题也许有多个订阅会话
                        if let Ok(index) = w.sessions.binary_search_by(|s| {
                            s.cmp(&session)
                        }) {
                            //从会话表中找到指定会话，则移除
                            w.sessions.remove(index);
                        }

                        if w.sessions.len() == 0 {
                            //当前主题已退订所有会话，则从订阅表中移除当前主题
                            keys.push(key.clone());
                        }
                    }
                }
            }


            //线程安全的移除匹配指定主题模式的主题
            for key in keys {
                self.sub_tab.write().remove(&key);
            }

            //移除注册了指定主题模式的会话
            self.patterns.write().remove(path, session.clone());
        } else {
            //退订的是主题
            if self.sub_tab.read().get(&topic).is_some() {
                //当前主题，有会话订阅
                let mut is_remove = false;
                let cache = self.sub_tab.read().get(&topic).unwrap().clone();
                let mut w = cache.write();
                if w.first.is_some() {
                    //当前主题的只有一个订阅会话
                    if Arc::ptr_eq(session, &w.first.as_ref().unwrap()) {
                        //会话相同，则从订阅表中移除当前主题
                        w.first = None;
                        is_remove = true;
                    }
                } else {
                    //当前主题也许有多个订阅会话，则从会话表中移除指定的会话
                    if let Ok(index) = w.sessions.binary_search_by(|s| {
                        s.cmp(&session)
                    }) {
                        //从会话表中找到指定会话，则移除
                        w.sessions.remove(index);
                    }

                    if w.sessions.len() == 0 {
                        //当前主题已退订所有会话，则从订阅表中移除当前主题
                        is_remove = true;
                    }
                }

                //线程安全的移除当前主题
                if is_remove {
                    self.sub_tab.write().remove(&topic);
                }
            }
        }
    }

    //退订指定会话已订阅的所有主题
    pub fn unsubscribe_all(&self, session: &Arc<QosZeroSession<S>>) {
        //从用户已订阅主题中移除当前会话
        let opt = self.topics.write().remove(session);

        match opt {
            None => {
                //当前会话没有订阅任何主题，则忽略
                ()
            },
            Some(topics) => {
                //当前会话有订阅主题，则退订
                for topic in topics {
                    self.unsubscribe(session, topic);
                }
            },
        }
    }
}

//判断指定的主题与指定的主题模式是否匹配
fn is_match(pattern: &TopicPath, path: &TopicPath) -> bool {
    let path_len = path.len();
    let pattern_len = pattern.len();
    if path_len < pattern_len {
        //当指定主题的级数小于模式的级数，则根据级差进行判断
        let level_diff = pattern_len - path_len; //获取级数差
        if level_diff > 1 {
            //级差过大，则一定不匹配
            return false;
        } else if level_diff == 1 {
            //级差为1
            if pattern.is_multi(pattern_len - 1) {
                //如果主题模式以全通配符结尾，则匹配
                let index = pattern_len - 2;
                return pattern.get(index).unwrap().fit(path.get(index).unwrap());
            } else {
                //如果主题模式以具体主题或单通配符结尾，则不匹配
                return false;
            }
        } else {
            //级差过小，则一定不匹配
            return false;
        }
    } else {
        //当指定主题的级数大于等于模式的级数，则逐级匹配
        for index in 0..path_len {
            if pattern.is_final(index) {
                //当前模式已结束
                if pattern.is_multi(index) {
                    //如果主题模式以全通配符结尾，则匹配
                    return true;
                }
            }

            if let Some(p) = pattern.get(index) {
                if p.fit(path.get(index).unwrap()) {
                    //当前主题与模式匹配，则继续逐级匹配
                    continue;
                }
            }

            return false;
        }
    }

    true
}

//为指定会话记录订阅主题，包括主题、主题模式和匹配主题模式的主题
fn save_topic<S: Socket>(broker: &MqttBroker<S>, session: &Arc<QosZeroSession<S>>, topic: &String) {
    if broker.topics.read().contains_key(session) {
        //指定会话的已订阅主题存在
        if let Some(topics) = broker.topics.write().get_mut(session) {
            topics.push(topic.clone());
        }
    } else {
        //指定会话的已订阅主题不存在
        broker.topics.write().insert(session.clone(), vec![topic.clone()]);
    }
}