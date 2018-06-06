use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;
use std::sync::atomic::{AtomicUsize, Ordering};

use magnetic::mpsc::mpsc_queue;
use magnetic::buffer::dynamic::DynamicBuffer;
use magnetic::{Producer, Consumer};
use magnetic::mpsc::{MPSCProducer, MPSCConsumer};

use data::Server;
use mqtt3::{self, Packet};
use net::{Socket, Stream};
use string_cache::DefaultAtom as Atom;
use util;
use fnv::FnvHashMap;
use handler::TopicHandle;

pub struct ServerNode(Arc<Mutex<ServerNodeImpl>>);

/// 主题元信息
pub struct TopicMeta {
    //
    topic: mqtt3::TopicPath,
    // 该主题是否可以发布
    can_publish: bool,
    // 该主题是否可以订阅
    can_subscribe: bool,
    // 如果有唯一键，需要到ClientStub去找值
    only_one_key: Option<Atom>,
    // 对应的应用层回调 
    publish_func: Box<Fn(ClientStub, Result<Arc<Vec<u8>>>)>,
}

/// 订阅的主题
struct SubTopic {
    // 主题名，可能是模式
    path: mqtt3::TopicPath,
    // 主题对应的元信息
    meta: Arc<TopicMeta>,
    // 主题关联的客户端
    clients: Vec<usize>,
}

/// 保留的主题
struct RetainTopic {
    // 主题路径
    path: mqtt3::TopicPath,
    // 该主题最近的保留消息
    retain_msg: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct ClientStub {
    socket: Socket,
    _keep_alive: u16,
    _last_will: Option<mqtt3::LastWill>,
    attributes: Arc<RwLock<FnvHashMap<Atom, Arc<Vec<u8>>>>>,
    queue: Arc<(MPSCProducer<Arc<TopicHandle>, DynamicBuffer<Arc<TopicHandle>>>, MPSCConsumer<Arc<TopicHandle>, DynamicBuffer<Arc<TopicHandle>>>)>,
    queue_size: Arc<AtomicUsize>,
}

struct ServerNodeImpl {
    clients: FnvHashMap<usize, Arc<ClientStub>>,

    sub_topics: FnvHashMap<Atom, SubTopic>,
    retain_topics: FnvHashMap<Atom, RetainTopic>,
    metas: FnvHashMap<Atom, Arc<TopicMeta>>,
}

unsafe impl Sync for ServerNodeImpl {}
unsafe impl Send for ServerNodeImpl {}

impl ClientStub {
    pub fn get_queue_size(&self) -> usize {
        self.queue_size.load(Ordering::Relaxed)
    }
    pub fn queue_producer(&self, val: usize) {
        self.queue_size.store(val, Ordering::Relaxed)
    }
}

impl ServerNode {
    pub fn new() -> ServerNode {
        ServerNode(Arc::new(Mutex::new(ServerNodeImpl {
            clients: FnvHashMap::default(),
            sub_topics: FnvHashMap::default(),
            retain_topics: FnvHashMap::default(),
            metas: FnvHashMap::default(),
        })))
    }
}

impl Server for ServerNode {
    fn add_stream(&mut self, socket: Socket, stream: Arc<RwLock<Stream>>) {
        handle_stream(self.0.clone(), socket, stream);
    }

    fn publish(
        &mut self,
        retain: bool,
        qos: mqtt3::QoS,
        topic: Atom,
        payload: Vec<u8>,
    ) -> Result<()> {
        if qos != mqtt3::QoS::AtMostOnce {
            return Err(Error::new(ErrorKind::Other, "server publish: InvalidQos"));
        }
        return publish_impl(self.0.clone(), retain, qos, topic, payload);
    }

    fn shutdown(&mut self) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        node.clients.clear();
        node.sub_topics.clear();
        node.retain_topics.clear();
        node.metas.clear();
        return Ok(());
    }

    fn set_topic_meta(
        &mut self,
        name: Atom,
        can_publish: bool,
        can_subscribe: bool,
        only_one_key: Option<Atom>,
        handler: Box<Fn(ClientStub, Result<Arc<Vec<u8>>>)>,
    ) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        let topic = mqtt3::TopicPath::from_str(&name);
        if topic.is_err() {
            return Err(Error::new(
                ErrorKind::Other,
                "set_Topic_meta, invalid topic",
            ));
        }
        let topic = topic.unwrap();
        node.metas.insert(
            name,
            Arc::new(TopicMeta {
                topic,
                can_publish,
                can_subscribe,
                only_one_key,
                publish_func: handler,
            }),
        );
        return Ok(());
    }
    fn unset_topic_meta(
        &mut self,
        name: Atom,
    ) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        node.metas.remove(&name);
        return Ok(());
    }
}

fn handle_stream(node: Arc<Mutex<ServerNodeImpl>>, socket: Socket, stream: Arc<RwLock<Stream>>) {

    let s = stream.clone();
    util::recv_mqtt_packet(
        stream,
        Box::new(move |packet: Result<Packet>| {
            handle_recv(node.clone(), &socket, s.clone(), packet);
        }),
    );
}

fn handle_recv(
    node: Arc<Mutex<ServerNodeImpl>>,
    socket: &Socket,
    stream: Arc<RwLock<Stream>>,
    packet: Result<Packet>,
) {
    let n = node.clone();
    let st = stream.clone();
    if let Ok(packet) = packet {
        match packet {
            Packet::Connect(connect) => recv_connect(n, socket, stream, connect),
            Packet::Subscribe(sub) => recv_sub(n, socket, sub),
            Packet::Unsubscribe(unsub) => recv_unsub(n, socket, unsub),
            Packet::Publish(publish) => recv_publish(n, publish, socket),
            Packet::Pingreq => recv_pingreq(n, socket),
            Packet::Disconnect => recv_disconnect(n, socket.socket),
            _ => panic!("server handle_recv: invalid packet!"),
        }
    }

    {
        let s = st.clone();
        let socket = socket.clone();
        let n = node.clone();
        util::recv_mqtt_packet(
            st,
            Box::new(move |packet: Result<Packet>| {
                handle_recv(n.clone(), &socket, s.clone(), packet);
            }),
        );
    }
}

fn recv_connect(
    node: Arc<Mutex<ServerNodeImpl>>,
    socket: &Socket,
    _stream: Arc<RwLock<Stream>>,
    connect: mqtt3::Connect,
) {
    let mut code = mqtt3::ConnectReturnCode::Accepted;
    if connect.protocol != mqtt3::Protocol::MQTT(4) {
        code = mqtt3::ConnectReturnCode::RefusedProtocolVersion;
    } else {
        // TODO: 验证 client_id 是否合法
        // code = mqtt3::ConnectReturnCode::RefusedIdentifierRejected;
        let mut att = FnvHashMap::default();
        
        
        if let Some(username) = connect.username {
            att.insert(Atom::from("$username"), Arc::new(Vec::from(username)));
        }
        if let Some(password) = connect.password {
            att.insert(Atom::from("$password"), Arc::new(Vec::from(password)));
        }
        att.insert(Atom::from("$client_id"), Arc::new(Vec::from(connect.client_id)));
        att.insert(Atom::from("$connect_time"), Arc::new(Vec::from(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs().to_string())));

        let node = &mut node.lock().unwrap();
        let s = socket.clone();
        node.clients.insert(
            socket.socket,
            Arc::new(ClientStub {
                socket: s,
                _keep_alive: connect.keep_alive,
                _last_will: connect.last_will,
                attributes: Arc::new(RwLock::new(att)),
                queue: Arc::new(mpsc_queue(DynamicBuffer::new(32).unwrap())),
                queue_size: Arc::new(AtomicUsize::new(0)),
            }),
        );
        //创建$r/$id
        let name = Atom::from(String::from("$r/") + &socket.socket.to_string());
        let topic = mqtt3::TopicPath::from_str(&name);
        let topic = topic.unwrap();
        node.metas.insert(
            name,
            Arc::new(TopicMeta {
                topic,
                can_publish: false,
                can_subscribe: false,
                only_one_key: None,
                publish_func: Box::new(|_attr, _result| {}),
            }),
        );
    }
    util::send_connack(socket, code);
}

fn recv_sub(node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket, sub: mqtt3::Subscribe) {
    let mut codes = Vec::with_capacity(sub.topics.len());
    let node = &mut node.lock().unwrap();

    for mqtt3::SubscribeTopic {
        qos,
        topic_path: path,
    } in sub.topics.iter()
    {
        if *qos != mqtt3::QoS::AtMostOnce {
            codes.push(mqtt3::SubscribeReturnCodes::Failure);
            continue;
        }

        // str不合法，失败，下一个
        {
            let topic = mqtt3::TopicPath::from_str(&path);
            if topic.is_err() {
                codes.push(mqtt3::SubscribeReturnCodes::Failure);
                continue;
            }
        }

        codes.push(recv_sub_impl(
            node,
            socket.socket,
            Atom::from(path.as_str()),
        ));
    }
    util::send_suback(socket, sub.pid, codes);
}

fn recv_sub_impl(node: &mut ServerNodeImpl, cid: usize, name: Atom) -> mqtt3::SubscribeReturnCodes {
    {
        // 已经有主题的情况
        let topic = node.sub_topics.get_mut(&name);
        if topic.is_some() {
            let topic = topic.unwrap();
            if topic.clients.iter().all(|e| *e != cid) {
                topic.clients.push(cid);
            }
            return mqtt3::SubscribeReturnCodes::Success(mqtt3::QoS::AtMostOnce);
        }
    }

    let topic_atom;
    {
        let meta = node.metas.get(&name);
        if meta.is_none() {
            return mqtt3::SubscribeReturnCodes::Failure;
        }
        let meta = meta.unwrap();
        if !meta.can_subscribe {
            return mqtt3::SubscribeReturnCodes::Failure;
        }

        let mut name = meta.topic.path.clone();
        if meta.only_one_key.is_some() {
            if let Ok(t) = mqtt3::TopicPath::from_str(&name) {
                if t.wildcards {
                    return mqtt3::SubscribeReturnCodes::Failure;
                }
            }
            
            let key = meta.only_one_key.as_ref().unwrap();
            let c = node.clients.get(&cid).unwrap();
            let att = c.attributes.read().unwrap();
            let attr = att.get(key).unwrap();
            
            use std::str;
            let attr = str::from_utf8(attr).unwrap();
            name = name + attr;
        }
        topic_atom = Atom::from(name.as_str());
        node.sub_topics.insert(
            topic_atom.clone(),
            SubTopic {
                meta: meta.clone(),
                path: mqtt3::TopicPath::from_str(name).unwrap(),
                clients: vec![cid],
            },
        );
    }

    {
        let mtopic = mqtt3::TopicPath::from_str(topic_atom).unwrap();
        // 发布保留主题
        for (_, curr) in node.retain_topics.iter() {
            if mtopic.is_match(&curr.path) {
                // TODO: node???
                // publish_impl(node, retain, qos, topic, payload)
            }
        }
    }
    return mqtt3::SubscribeReturnCodes::Success(mqtt3::QoS::AtMostOnce);
}

fn recv_unsub(node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket, unsub: mqtt3::Unsubscribe) {
    let node = &mut node.lock().unwrap();

    for path in unsub.topics.iter() {
        // str不合法，失败，下一个
        {
            let topic = mqtt3::TopicPath::from_str(&path);
            if topic.is_err() {
                continue;
            }
        }

        recv_unsub_impl(node, socket.socket, Atom::from(path.as_str()));
    }
    util::send_unsuback(socket, unsub.pid);
}

fn recv_unsub_impl(node: &mut ServerNodeImpl, cid: usize, name: Atom) {
    {
        // 已经有主题的情况
        let topic = node.sub_topics.get_mut(&name);
        if topic.is_some() {
            let topic = topic.unwrap();
            topic.clients.retain(|e| *e != cid);
            return;
        }
    }

    {
        let meta = node.metas.get(&name);
        if meta.is_none() {
            return;
        }
        let meta = meta.unwrap();
        if !meta.can_subscribe {
            return;
        }

        let mut name = meta.topic.path.clone();
        if meta.only_one_key.is_some() {
            if let Ok(t) = mqtt3::TopicPath::from_str(&name) {
                if t.wildcards {
                    return;
                }
            }

            let key = meta.only_one_key.as_ref().unwrap();
            let c = node.clients.get(&cid).unwrap();
            let att = c.attributes.read().unwrap();
            let attr = att.get(key).unwrap();
            
            use std::str;
            let attr = str::from_utf8(attr).unwrap();
            name = name + attr;
        }
        let atom = Atom::from(name.as_str());
        node.sub_topics.remove(&atom);
    }
}

fn recv_publish(node: Arc<Mutex<ServerNodeImpl>>, publish: mqtt3::Publish, socket: &Socket) {
    if publish.qos != mqtt3::QoS::AtMostOnce {
        return;
    }

    let topic = mqtt3::TopicPath::from_str(&publish.topic_name);
    if topic.is_err() {
        return;
    }
    let topic = topic.unwrap();
    let node = &mut node.lock().unwrap();
    for (_, meta) in node.metas.iter() {
        if meta.topic.is_match(&topic) {
            let client_stub = node.clients.get(&socket.socket).unwrap();
            let client_stub = &*client_stub.clone();
            (meta.publish_func)(client_stub.clone(), Ok(publish.payload.clone()));
        }
    }
}

fn recv_pingreq(_node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket) {
    // TODO
    util::send_pingresp(socket);
}

fn recv_disconnect(node: Arc<Mutex<ServerNodeImpl>>, cid: usize) {
    let node = &mut node.lock().unwrap();
    node.clients.remove(&cid);
}

fn publish_impl(
    node: Arc<Mutex<ServerNodeImpl>>,
    retain: bool,
    qos: mqtt3::QoS,
    topic: Atom,
    payload: Vec<u8>,
) -> Result<()> {
    if qos != mqtt3::QoS::AtMostOnce {
        return Err(Error::new(ErrorKind::Other, "publish impl, invalid qos"));
    }

    let t = mqtt3::TopicPath::from_str(&topic);
    if t.is_err() {
        return Err(Error::new(ErrorKind::Other, "publish impl, invalid topic"));
    }
    let t = t.unwrap();
    let node = &mut node.lock().unwrap();

    if retain {
        let atom = Atom::from(t.path.as_str());
        let has_topic = node.retain_topics.contains_key(&atom);
        if has_topic {
            let m = node.retain_topics.get_mut(&atom).unwrap();
            m.retain_msg = Some(payload.clone());
        } else {
            node.retain_topics.insert(
                topic,
                RetainTopic {
                    path: t.clone(),
                    retain_msg: Some(payload.clone()),
                },
            );
        }
    }
    
    for (_, top) in node.sub_topics.iter() {
        if top.meta.can_publish && top.path.is_match(&t) {
            for cid in top.clients.iter() {
                let client = node.clients.get(cid).unwrap();
                let socket = client.socket.clone();
                util::send_publish(
                    &socket,
                    retain,
                    mqtt3::QoS::AtMostOnce,
                    t.path.as_str(),
                    payload.clone(),
                );
            }
        }
    }
    return Ok(());
}
