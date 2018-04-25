use std::boxed::FnBox;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, Mutex, RwLock};

use data::Server;
use mqtt3::{self, Packet};
use net::{Socket, Stream};
use string_cache::DefaultAtom as Atom;
use util;

pub struct ServerNode(Arc<Mutex<ServerNodeImpl>>);

/// 主题元信息
pub struct TopicMeta {
    /// 主题名
    name: Atom,
    /// 该主题是否可以发布
    can_publish: bool,
    /// 该主题是否可以订阅
    can_subscribe: bool,
    /// 如果有唯一键，需要到ClientStub去找值
    only_one_key: Option<Atom>,
    /// 对应的应用层回调
    recv_handle: Box<Fn(Result<&[u8]>)>,
}

/// 主题
struct Topic {
    /// 主题名
    name: Atom,
    /// 主题对应的元信息
    meta: Arc<TopicMeta>,
    /// 该主题最近的保留消息
    retain_msg: Option<Vec<u8>>,
}

struct ClientStub {
    socket: Socket,
    stream: Arc<RwLock<Stream>>,

    keep_alive: u16,
    last_will: Option<mqtt3::LastWill>,

    subs: Vec<mqtt3::TopicPath>,
    attributes: HashMap<Atom, Arc<Vec<u8>>>,
}

struct ServerNodeImpl {
    clients: HashMap<usize, ClientStub>,

    topics: HashMap<Atom, Topic>,
    topic_metas: HashMap<Atom, Arc<TopicMeta>>,
}

unsafe impl Sync for ServerNodeImpl {}
unsafe impl Send for ServerNodeImpl {}

impl ServerNode {
    pub fn new() -> ServerNode {
        ServerNode(Arc::new(Mutex::new(ServerNodeImpl {
            clients: HashMap::new(),
            topics: HashMap::new(),
            topic_metas: HashMap::new(),
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
        return Ok(());
    }

    fn shutdown(&mut self) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        return Ok(());
    }

    fn set_topic_meta(
        &mut self,
        name: Atom,
        can_publish: bool,
        can_subscribe: bool,
        only_one_key: Option<Atom>,
        handler: Box<Fn(Result<&[u8]>)>,
    ) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        let n = name.clone();
        node.topic_metas.insert(
            name,
            Arc::new(TopicMeta {
                name: n.clone(),
                can_publish,
                can_subscribe,
                only_one_key,
                recv_handle: handler,
            }),
        );
        return Ok(());
    }
}

fn handle_stream(node: Arc<Mutex<ServerNodeImpl>>, socket: Socket, stream: Arc<RwLock<Stream>>) {
    println!("server handle_stream");

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
    println!("server handle_recv, packet");
    let n = node.clone();
    let st = stream.clone();
    if let Ok(packet) = packet {
        match packet {
            Packet::Connect(connect) => recv_connect(n, socket, stream, connect),
            Packet::Subscribe(sub) => recv_sub(n, socket, sub),
            Packet::Unsubscribe(unsub) => recv_unsub(n, socket, unsub),
            Packet::Publish(publish) => recv_publish(n, socket, publish),
            Packet::Pingreq => recv_pingreq(n, socket),
            Packet::Disconnect => recv_disconnect(n, socket),
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
    stream: Arc<RwLock<Stream>>,
    connect: mqtt3::Connect,
) {
    let mut code = mqtt3::ConnectReturnCode::Accepted;
    if connect.protocol != mqtt3::Protocol::MQTT(4) {
        code = mqtt3::ConnectReturnCode::RefusedProtocolVersion;
    } else {
        // TODO: 验证 client_id 是否合法
        // code = mqtt3::ConnectReturnCode::RefusedIdentifierRejected;
        let node = &mut node.lock().unwrap();
        let s = socket.clone();
        let stream = stream.clone();
        node.clients.insert(
            socket.socket,
            ClientStub {
                socket: s,
                stream: stream,
                keep_alive: connect.keep_alive,
                last_will: connect.last_will,

                subs: Vec::new(),
                attributes: HashMap::new(),
            },
        );
    }
    util::send_connack(socket, code);
}

fn recv_sub(node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket, sub: mqtt3::Subscribe) {
    let mut codes = Vec::with_capacity(sub.topics.len());
    let node = &mut node.lock().unwrap();

    for mqtt3::SubscribeTopic {
        topic_path: path,
        qos: _,
    } in sub.topics.iter()
    {
        {
            // 如果该客户端已有topic信息，返回
            let client = node.clients.get_mut(&socket.socket);
            if client.is_none() {
                println!("server recv_unsub, client is_none!");
                return;
            }
            let client = client.unwrap();
            if client
                .subs
                .iter()
                .any(|elem| elem.path.as_str() == path.as_str())
            {
                codes.push(mqtt3::SubscribeReturnCodes::Success(mqtt3::QoS::AtMostOnce));
                continue;
            }
        }

        // 验证topic合法性
        let topic = mqtt3::TopicPath::from_str(path);
        if topic.is_err() {
            codes.push(mqtt3::SubscribeReturnCodes::Failure);
            continue;
        }

        let topic = topic.unwrap();

        {
            // 到topics查topic信息
            let g_topic_meta;
            let atom = Atom::from(path.as_str());
            let g_topic = node.topics.get(&atom);
            if !g_topic.is_none() {
                g_topic_meta = g_topic.unwrap().meta.clone();
            } else {
                let meta = node.topic_metas.get(&atom);
                if !meta.is_none() {
                    g_topic_meta = meta.unwrap().clone();
                } else {
                    codes.push(mqtt3::SubscribeReturnCodes::Failure);
                    continue;
                }
            }
            if !g_topic_meta.can_subscribe {
                codes.push(mqtt3::SubscribeReturnCodes::Failure);
                continue;
            }
            if g_topic.is_none() {
                // 创建
                if g_topic_meta.only_one_key.is_none() {
                    // 这时，没必要创建主题
                } else {
                    // TODO: 这里应该会调整

                }
            }

            if g_topic.is_none() {
                
            }
        }

        // 将topic加到客户端
        {
            let client = node.clients.get_mut(&socket.socket).unwrap();
            client.subs.push(topic);
        }

        codes.push(mqtt3::SubscribeReturnCodes::Success(mqtt3::QoS::AtMostOnce));
    }
    util::send_suback(socket, sub.pid, codes);
}

fn recv_unsub(node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket, unsub: mqtt3::Unsubscribe) {
    let node = &mut node.lock().unwrap();
    let client = node.clients.get_mut(&socket.socket);
    if client.is_none() {
        println!("server recv_unsub, client is_none!");
        return;
    }
    let client = client.unwrap();
    for topic in unsub.topics {
        client.subs.retain(|elem| elem.path != topic);
    }
    util::send_unsuback(socket, unsub.pid);
}

fn recv_publish(node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket, publish: mqtt3::Publish) {
    let node = &mut node.lock().unwrap();
}

fn recv_pingreq(node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket) {
    // TODO
    util::send_pingresp(socket);
}

fn recv_disconnect(node: Arc<Mutex<ServerNodeImpl>>, socket: &Socket) {}
