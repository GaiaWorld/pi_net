use std::boxed::FnBox;
use std::fmt::{Debug, Formatter, Result as DebugResult};
use std::io::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use magnetic::buffer::dynamic::DynamicBuffer;
use magnetic::mpsc::mpsc_queue;
use magnetic::mpsc::{MPSCConsumer, MPSCProducer};
use magnetic::{Consumer, Producer};

use atom::Atom;
use time::now_millis;
use gray::GrayVersion;
use lib_util::uncompress;
use data::{Server, SetAttrFun};
use fnv::FnvHashMap;
use mqtt3::{self, Packet};
use net::CloseFn;
use net::api::{Socket, Stream};
use session;
use util;

#[derive(Clone)]
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
    //only_one_key: Option<Atom>,
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
    keep_alive: u16,
    last_will: Arc<RwLock<Option<mqtt3::LastWill>>>,
    queue: Arc<(
        MPSCProducer<Box<FnBox()>, DynamicBuffer<Box<FnBox()>>>,
        MPSCConsumer<Box<FnBox()>, DynamicBuffer<Box<FnBox()>>>,
    )>,
    queue_size: Arc<AtomicUsize>,
}

impl GrayVersion for ClientStub {
    fn get_gray(&self) -> &Option<usize>{
        &self.socket.get_gray()
    }

    fn set_gray(&mut self, gray: Option<usize>){
        &self.socket.set_gray(gray);
    }

    fn get_id(&self) -> usize {
        self.socket.get_id()
    }
}

impl Debug for ClientStub {
    fn fmt(&self, f: &mut Formatter) -> DebugResult {
        let socket = match &self.socket {
            &Socket::Raw(ref s) => s.socket,
            &Socket::Tls(ref s) => s.socket,
        };

        write!(
            f,
            "ClientStub[socket = {}, keep_alive = {}]",
            socket, self.keep_alive
        )
    }
}

struct ServerNodeImpl {
    clients: FnvHashMap<usize, Arc<ClientStub>>,

    sub_topics: FnvHashMap<Atom, SubTopic>,
    retain_topics: FnvHashMap<Atom, RetainTopic>,
    metas: FnvHashMap<Atom, Arc<TopicMeta>>,
    set_attr: Option<SetAttrFun>,
}

unsafe impl Sync for ServerNodeImpl {}
unsafe impl Send for ServerNodeImpl {}

impl ClientStub {
    //获取发送队列大小
    pub fn get_queue_size(&self) -> usize {
        self.queue_size.load(Ordering::Relaxed)
    }
    //增加队列消息
    pub fn queue_push(&self, handle: Box<FnBox()>) {
        self.queue.0.push(handle).is_ok();
        self.queue_size
            .store(self.get_queue_size() + 1, Ordering::Relaxed)
    }
    //弹出队列消息
    pub fn queue_pop(&self) -> Option<Box<FnBox()>> {
        if self.get_queue_size() > 0 {
            let v = self.queue.1.pop().unwrap();
            self.queue_size
                .store(self.get_queue_size() - 1, Ordering::Relaxed);
            return Some(v);
        }
        None
    }
    //获取连接
    pub fn get_socket(&self) -> Socket {
        self.socket.clone()
    }

    //修改遗言
    pub fn set_last_will(&mut self, last_will: mqtt3::LastWill) {
        let mut last_will2 = self.last_will.write().unwrap();
        *last_will2 = Some(last_will);
    }
}

impl ServerNode {
    pub fn new() -> ServerNode {
        ServerNode(Arc::new(Mutex::new(ServerNodeImpl {
            clients: FnvHashMap::default(),
            sub_topics: FnvHashMap::default(),
            retain_topics: FnvHashMap::default(),
            metas: FnvHashMap::default(),
            set_attr: None,
        })))
    }
    //设置连接关闭回调(遗言发布)
    pub fn set_close_callback(&self, stream: Stream, func: CloseFn) {
        let node = self.0.clone();
        let handle = move |socket_id: usize, r: Result<()>| {
            //获取遗言消息
            let node = node.lock().unwrap();
            let client = node.clients.get(&socket_id).unwrap();
            if let Some(last_will) = client.last_will.read().unwrap().clone() {
                // let retain = last_will.retain;
                // let qos = last_will.qos;
                // let topic = Atom::from(last_will.topic.as_str());
                let payload = Vec::from(last_will.message);

                //固定遗言topic 通过set_topic_meta设置回调
                let will_topic = Atom::from("$last_will");
                if let Some(meta) = node.metas.get(&will_topic) {
                    let client_stub = node.clients.get(&socket_id).unwrap();
                    let client_stub = &*client_stub.clone();
                    (meta.publish_func)(client_stub.clone(), Ok(Arc::new(payload)));
                }
            }
            func.call_box((socket_id, r));
        };
        stream.set_close_callback(Box::new(handle));
    }
}

impl Server for ServerNode {
    fn add_stream(&self, socket: Socket, stream: Stream) {
        handle_stream(self.0.clone(), socket, stream);
    }

    fn publish(
        &self,
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

    fn shutdown(&self) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        node.clients.clear();
        node.sub_topics.clear();
        node.retain_topics.clear();
        node.metas.clear();
        return Ok(());
    }

    fn set_topic_meta(
        &self,
        name: Atom,
        can_publish: bool,
        can_subscribe: bool,
        //only_one_key: Option<Atom>,
        handler: Box<Fn(ClientStub, Result<Arc<Vec<u8>>>)>,
    ) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        let topic = mqtt3::TopicPath::from_str((*name).clone().as_str());
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
                //only_one_key,
                publish_func: handler,
            }),
        );
        return Ok(());
    }
    fn unset_topic_meta(&self, name: Atom) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        node.metas.remove(&name);
        return Ok(());
    }
    fn set_attr(&self, handler: SetAttrFun) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        node.set_attr = Some(handler);
        Ok(())
    }
}

//处理socket接收流
fn handle_stream(node: Arc<Mutex<ServerNodeImpl>>, socket: Socket, stream: Stream) {
    let s = stream.clone();
    util::recv_mqtt_packet(
        stream,
        Box::new(move |packet: Result<Packet>| {
            handle_recv(node.clone(), &socket, s.clone(), packet);
        }),
    );
}

//处理mqtt消息
fn handle_recv(
    node: Arc<Mutex<ServerNodeImpl>>,
    socket: &Socket,
    stream: Stream,
    packet: Result<Packet>,
) {
    let id = match socket {
        &Socket::Raw(ref s) => s.socket,
        &Socket::Tls(ref s) => s.socket,
    };
    let n = node.clone();
    let st = stream.clone();
    if let Ok(packet) = packet {
        match packet {
            Packet::Connect(connect) => recv_connect(n, socket, stream, connect),
            Packet::Subscribe(sub) => recv_sub(n, socket, sub),
            Packet::Unsubscribe(unsub) => recv_unsub(n, socket, unsub),
            Packet::Publish(publish) => recv_publish(n, publish, socket),
            Packet::Pingreq => recv_pingreq(n, socket),
            Packet::Disconnect => {
                let socket = match &socket {
                    &Socket::Raw(s) => s.socket,
                    &Socket::Tls(s) => s.socket,
                };
                recv_disconnect(n, socket)
            },
            _ => panic!("server handle_recv: invalid packet!"),
        }
    }

    //设置keep_alive定时器
    {
        let node = &mut node.lock().unwrap();
        let id = match &socket {
            &Socket::Raw(s) => s.socket,
            &Socket::Tls(s) => s.socket,
        };

        let socket = socket.clone();
        let clients = node.clients.get(&id).unwrap();
        //mqtt协议要求keep_alive的1.5倍超时关闭连接
        let keep_alive = ((clients.keep_alive as f32) * 1.5) as u64;
        if keep_alive > 0 {
            let stream = st.clone();
            match &stream {
                &Stream::Raw(ref s) => {
                    let n = now_millis();
                    let ss = s.read().unwrap();
                    ss.net_timers.write().unwrap().set_timeout(
                        Atom::from(String::from("handle_recv") + &id.to_string()),
                        Duration::from_millis(keep_alive as u64 * 1000),
                        Box::new(move |_src: Atom| {
                            println!("keep_alive timeout con close!!!!!!!!!!!!{}, {}",  now_millis() - n, keep_alive as u64 * 1000);
                            //关闭连接
                            match &socket {
                                &Socket::Raw(ref s) => s.close(true),
                                &Socket::Tls(ref s) => s.close(true),
                            }
                        }),
                    );
                },
                &Stream::Tls(ref s) => {
                    let n = now_millis();
                    let ss = s.read().unwrap();
                    let timers = ss.get_timers();
                    timers.write().unwrap().set_timeout(
                        Atom::from(String::from("handle_recv") + &id.to_string()),
                        Duration::from_millis(keep_alive as u64 * 1000),
                        Box::new(move |_src: Atom| {
                            println!("keep_alive timeout con close!!!!!!!!!!!!{}, {}",  now_millis() - n, keep_alive as u64 * 1000);
                            //关闭连接
                            match &socket {
                                &Socket::Raw(ref s) => s.close(true),
                                &Socket::Tls(ref s) => s.close(true),
                            }
                        }),
                    );
                },
            }
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
    _stream: Stream,
    connect: mqtt3::Connect,
) {
    let mut code = mqtt3::ConnectReturnCode::Accepted;
    println!("connect.protocol = {:?}", connect.protocol);
    if connect.protocol != mqtt3::Protocol::MQTT(4) && connect.protocol != mqtt3::Protocol::MQIsdp(3) {
        code = mqtt3::ConnectReturnCode::RefusedProtocolVersion;
    } else {
        // TODO: 验证 client_id 是否合法
        // code = mqtt3::ConnectReturnCode::RefusedIdentifierRejected;
        

        let node = &mut node.lock().unwrap();
        //调用设置attr方法
        let mut att = FnvHashMap::default();
        if let Some(attr_func) = &node.set_attr {
            attr_func(&mut att, socket.clone(), connect.clone())
        }
        let s = socket.clone();
        let client_stub = Arc::new(ClientStub {
            socket: s,
            keep_alive: connect.keep_alive,
            last_will: Arc::new(RwLock::new(connect.last_will)),
            queue: Arc::new(mpsc_queue(DynamicBuffer::new(32).unwrap())),
            queue_size: Arc::new(AtomicUsize::new(0)),
        });
        let id = match &socket {
            &Socket::Raw(s) => s.socket,
            &Socket::Tls(s) => s.socket,
        };
        node.clients.insert(id, client_stub.clone());
        //模拟客户端发送主题消息
        let name = Atom::from(String::from("$open"));
        if let Some(meta) = node.metas.get(&name) {
            let client_stub = &*client_stub.clone();
            let new_ms = util::encode(session::encode_reps(0, 10, vec![]));
            (meta.publish_func)(
                client_stub.clone(),
                Ok(Arc::new(new_ms)),
            );
        }
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
        // 目前仅支持qos = 0
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

        let id = match &socket {
            &Socket::Raw(s) => s.socket,
            &Socket::Tls(s) => s.socket,
        };
        codes.push(recv_sub_impl(
            node,
            id,
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

        let name = meta.topic.path.clone();
        // if meta.only_one_key.is_some() {
        //     if let Ok(t) = mqtt3::TopicPath::from_str(&name) {
        //         if t.wildcards {
        //             return mqtt3::SubscribeReturnCodes::Failure;
        //         }
        //     }

        //     let key = meta.only_one_key.as_ref().unwrap();
        //     let c = node.clients.get(&cid).unwrap();
        //     let att = c.attributes.read().unwrap();
        //     let attr = att.get(key).unwrap();
        //     let attr: &Vec<u8> = attr.downcast_ref().unwrap();
        //     let attr = attr.to_hex();
        //     name = name + attr.as_str();
        // }
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
        let mtopic = mqtt3::TopicPath::from_str((*topic_atom).clone().as_str()).unwrap();
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

        let id = match &socket {
            &Socket::Raw(s) => s.socket,
            &Socket::Tls(s) => s.socket,
        };
        recv_unsub_impl(node, id, Atom::from(path.as_str()));
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

        let name = meta.topic.path.clone();
        // if meta.only_one_key.is_some() {
        //     if let Ok(t) = mqtt3::TopicPath::from_str(&name) {
        //         if t.wildcards {
        //             return;
        //         }
        //     }

        //     let key = meta.only_one_key.as_ref().unwrap();
        //     let c = node.clients.get(&cid).unwrap();
        //     let att = c.attributes.read().unwrap();
        //     let attr = att.get(key).unwrap();
        //     let attr: &Vec<u8> = attr.downcast_ref().unwrap();

        //     use std::str;
        //     let attr = str::from_utf8(attr).unwrap();
        //     name = name + attr;
        // }
        let atom = Atom::from(name.as_str());
        node.sub_topics.remove(&atom);
    }
}

fn recv_publish(node: Arc<Mutex<ServerNodeImpl>>, publish: mqtt3::Publish, socket: &Socket) {
    //println!("mqtt server!!!!!!!!!!!!!!!!!");
    if publish.qos != mqtt3::QoS::AtMostOnce {
        return;
    }
    //println!("!!!recv_publish.topic_name = {:?}", &publish.topic_name);
    let topic = mqtt3::TopicPath::from_str(&publish.topic_name);
    if topic.is_err() {
        return;
    }
    let topic = topic.unwrap();
    //println!("topic = {:?}", topic);
    let mut r = None;
    {
        let node = &mut node.lock().unwrap();
        for (_, meta) in node.metas.iter() {
            if meta.topic.is_match(&topic) {
                let id = match &socket {
                    &Socket::Raw(s) => s.socket,
                    &Socket::Tls(s) => s.socket,
                };
                r = Some((node.clients.get(&id).unwrap().clone(), meta.clone()));
                break;
            }
        }
    };

    match r {
        Some(v) => {
            let data = &publish.payload;
            let header = data[0];
            //压缩版本
            let compress = (&header >> 6) as u8;
            //消息版本
            //let vsn = &header & 0b11111;
            let r = match compress {
                util::UNCOMPRESS => Vec::from(&data[1..]),
                util::LZ4_BLOCK => {
                    let mut vec_ = Vec::new();
                    uncompress(&data[1..], &mut vec_).is_ok();
                    vec_
                }
                _ => {println!("Compression mode does not support, topic:{}", &publish.topic_name); return;},
            };
            (v.1.publish_func)((&*v.0).clone(), Ok(Arc::new(r)));
        },
        None => {
            println!("Topic is not registered {:?}", &publish.topic_name);
        },
    }
}

fn recv_pingreq(_node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket) {
    util::send_pingresp(socket);
}

fn recv_disconnect(node: Arc<Mutex<ServerNodeImpl>>, cid: usize) {
    let node = &mut node.lock().unwrap();
    node.clients.remove(&cid);
    //模拟客户端发送主题消息
    let name = Atom::from(String::from("$close"));
    if let Some(meta) = node.metas.get(&name) {
        let client_stub = node.clients.get(&cid).unwrap();
        let client_stub = &*client_stub.clone();
        let new_ms = util::encode(session::encode_reps(0, 10, vec![]));

        (meta.publish_func)(
            client_stub.clone(),
            Ok(Arc::new(new_ms)),
        );
    }
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
    
    let t = mqtt3::TopicPath::from_str((*topic).clone().as_str());
    if t.is_err() {
        return Err(Error::new(ErrorKind::Other, "publish impl, invalid topic"));
    }
    let t = t.unwrap();
    let node = &mut node.lock().unwrap();

    let payload = util::encode(payload);
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
