use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};
use std::io::{Error, Result, ErrorKind, BufWriter, BufReader};

use mqtt311::{MqttWrite, MqttRead, Protocol, ConnectReturnCode, Packet, Connect,
              Connack, QoS, Publish, PacketIdentifier, SubscribeTopic, SubscribeReturnCodes,
              Subscribe, Suback, Unsubscribe};

use worker::task::TaskType;
use worker::impls::{unlock_net_task_queue, cast_net_task};
use wsc::SharedWSClient;
use atom::Atom;

/*
* Mqtt同步访问任务类型
*/
const SYNC_MQTTC_TASK_TYPE: TaskType = TaskType::Sync(true);

/*
* Mqtt异步访问任务类型
*/
const ASYNC_MQTTC_TASK_TYPE: TaskType = TaskType::Async(false);

/*
* Mqtt异步访问任务优先级
*/
const ASYNC_MQTTC_PRIORITY: usize = 100;

/*
* 默认的连接协议名和级别， MQTT3.1.1
*/
const MQTT_CONNECT_PROTOCOL: Protocol = Protocol::MQTT(0x4);

/*
* 共享Mqtt客户端
*/
#[derive(Clone)]
pub struct SharedMqttClient(Arc<MqttClient>, isize);

unsafe impl Send for SharedMqttClient {}
unsafe impl Sync for SharedMqttClient {}

impl SharedMqttClient {
    //创建指定Broker的mqtt客户端
    pub fn create(url: &str) -> Result<Self> {
        match SharedWSClient::create(url) {
            Err(e) => Err(e),
            Ok(connect) => {
                let queue = connect.get_queue();
                Ok(SharedMqttClient(Arc::new(MqttClient {
                    pkid: PacketIdentifier::zero(),
                    connect,
                }), queue))
            },
        }
    }

    //获取当前客户端通讯id生成器
    pub fn get_id_gen(&self) -> Arc<AtomicU32> {
        self.0.connect.get_id_gen()
    }

    //连接Broker
    pub fn connect(&self,
                   level: Option<u8>,           //协议等级
                   keep_alive: u16,             //保持连接的超时时长，单位秒
                   client_id: String,           //客户端id
                   clean_session: bool,         //是否清理上次会话
                   username: Option<String>,    //用户名
                   password: Option<String>,    //密码
                   timeout: Option<u32>,        //连接超时时长，单位毫秒
                   callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
        let protocol = match level {
            None => MQTT_CONNECT_PROTOCOL,
            Some(level) => Protocol::MQTT(level),
        };

        let packet = Packet::Connect(Connect {
            protocol,
            keep_alive,
            client_id,
            clean_session,
            last_will: None,
            username,
            password,
        });

        let client = self.clone();
        let request = Box::new(move |_lock| {
            match client.0.connect.connect() {
                Err(e) => {
                    //websocket连接失败
                    callback(Err(e));
                },
                Ok(_) => {
                    match send_packet(&client, &packet) {
                        Err(e) => {
                            //握手消息发送失败
                            callback(Err(e));
                        },
                        Ok(_) => {
                            //握手消息发送成功，等待broker回应
                            recv_packet(client, None, None, timeout, callback);
                        },
                    }
                },
            }
        });
        cast_net_task(ASYNC_MQTTC_TASK_TYPE, ASYNC_MQTTC_PRIORITY, None, request, Atom::from("mqttc connect task"));
    }

    //订阅指定主题
    pub fn subscribe(&self,
                     topics: Vec<String>,
                     timeout: Option<u32>,
                     callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
        let mut vec = Vec::with_capacity(topics.len());
        for topic in topics {
            vec.push(SubscribeTopic {
                topic_path: topic,
                qos: QoS::AtMostOnce, //QoS0
            });
        }

        let packet = Packet::Subscribe(Subscribe {
            pkid: self.0.pkid.next(), //分配下一个pkid
            topics: vec,
        });

        let client = self.clone();
        let request = Box::new(move |_lock| {
            match send_packet(&client, &packet) {
                Err(e) => {
                    callback(Err(e));
                },
                Ok(_) => {
                    //发送成功，等待broker回应
                    let pkid = client.0.pkid;
                    recv_packet(client, None, Some(pkid), timeout, callback);
                }
            }
        });
        cast_net_task(ASYNC_MQTTC_TASK_TYPE, ASYNC_MQTTC_PRIORITY, None, request, Atom::from("mqttc subscribe task"));
    }

    //取消订阅主题
    pub fn unsubscribe(&self,
                       topics: Vec<String>,
                       timeout: Option<u32>,
                       callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
        let packet = Packet::Unsubscribe(Unsubscribe {
            pkid: self.0.pkid.next(), //分配下一个pkid
            topics,
        });

        let client = self.clone();
        let request = Box::new(move |_lock| {
            match send_packet(&client, &packet) {
                Err(e) => {
                    callback(Err(e));
                },
                Ok(_) => {
                    //发送成功，等待broker回应
                    let pkid = client.0.pkid;
                    recv_packet(client, None, Some(pkid), timeout, callback);
                }
            }
        });
        cast_net_task(ASYNC_MQTTC_TASK_TYPE, ASYNC_MQTTC_PRIORITY, None, request, Atom::from("mqttc unsubscribe task"));
    }

    //发布指定主题的消息
    pub fn publish(&self,
                   topic: String,
                   payload: Arc<Vec<u8>>,
                   timeout: Option<u32>,
                   callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
        let packet = Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtMostOnce, //QoS0
            retain: false,
            topic_name: topic,
            pkid: None,
            payload,
        });

        let client = self.clone();
        let request = Box::new(move |lock: Option<isize>| {
            match send_packet(&client, &packet) {
                Err(e) => {
                    callback(Err(e));
                },
                Ok(_) => {
                    //发布消息发送成功，对于Qos0，则表示发布成功
                    callback(Ok(None));
                }
            }

            unlock_net_task_queue(lock.unwrap()); //解锁同步任务队列，以保证下一个同步任务被处理
        });
        cast_net_task(SYNC_MQTTC_TASK_TYPE, 0, Some(self.1), request, Atom::from("mqttc publish task"));
    }

    //发送ping消息
    pub fn ping(&self, timeout: Option<u32>, callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
        let client = self.clone();
        let request = Box::new(move |_lock| {
            match send_packet(&client, &Packet::Pingreq) {
                Err(e) => {
                    callback(Err(e));
                },
                Ok(_) => {
                    //ping发送成功，等待broker回应
                    recv_packet(client, None, None, timeout, callback);
                }
            }
        });
        cast_net_task(ASYNC_MQTTC_TASK_TYPE, ASYNC_MQTTC_PRIORITY, None, request, Atom::from("mqttc ping task"));
    }

    //异步接收指定主题的消息
    pub fn receive(&self,
                   topic: String,
                   timeout: Option<u32>,
                   callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
        //等待接收指定主题的消息
        recv_packet(self.clone(), Some(topic), None, timeout, callback);
    }

    //关闭Mqtt连接
    pub fn disconnect(&self, callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
        let client = self.clone();
        let request = Box::new(move |_lock| {
            match send_packet(&client, &Packet::Disconnect) {
                Err(e) => {
                    callback(Err(e));
                },
                Ok(_) => {
                    //关闭连接消息发送成功，则立即关闭连接
                    if let Err(e) = client.0.connect.close(1000, "mqtt disconnect") {
                        //关闭连接错误
                        callback(Err(e));
                    } else {
                        //关闭连接成功
                        callback(Ok(None));
                    }
                }
            }
        });
        cast_net_task(ASYNC_MQTTC_TASK_TYPE, ASYNC_MQTTC_PRIORITY, None, request, Atom::from("mqttc disconnect task"));
    }
}

/*
* Mqtt客户端
*/
struct MqttClient {
    pkid:       PacketIdentifier,   //pkid
    connect:    SharedWSClient,     //websocket连接
}

//将Mqtt包序列化为二进制数据
fn packet_to_binary(packet: &Packet) -> Result<Vec<u8>> {
    let mut vec = Vec::new();
    let mut buf: BufWriter<&mut [u8]> = BufWriter::new(vec.as_mut_slice());

    if let Err(e) = buf.write_packet(packet) {
        return Err(Error::new(ErrorKind::InvalidData, e.to_string()));
    }

    Ok(Vec::from(buf.buffer()))
}

//将二进制数据反序列化为Mqtt包
fn binary_to_packet(bin: Vec<u8>) -> Result<Packet> {
    let mut buf: BufReader<&[u8]> = BufReader::new(bin.as_slice());
    match buf.read_packet() {
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e.to_string())),
        Ok(packet) => Ok(packet),
    }
}

//发送Mqtt包
fn send_packet(client: &SharedMqttClient, packet: &Packet) -> Result<()> {
    match packet_to_binary(&packet) {
        Err(e) => {
            Err(e) //消息序列化失败
        },
        Ok(bin) => {
            client.0.connect.send_bin(bin)
        },
    }
}

//异步接收Mqtt包，可以指定是否接收指定主题的发布消息，和接收的超时时长
fn recv_packet(client: SharedMqttClient, topic: Option<String>, pkid: Option<PacketIdentifier>, timeout: Option<u32>, callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
    let now = Instant::now(); //接收开始时间
    let client_copy = client.clone();
    let response = Box::new(move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
        match result {
            Ok(None) => {
                //接收空回应包
                callback(Err(Error::new(ErrorKind::InvalidInput, "mqttc empty response")));
            },
            Ok(Some(bin)) => {
                //接收回应包成功
                match timeout {
                    None => {
                        //不超时
                        handle_recv(client_copy, topic, pkid, bin, None, callback);
                    },
                    Some(t) => {
                        //扣除本次接收的时长
                        match t.checked_sub(now.elapsed().as_millis() as u32) {
                            None => {
                                //超时时间已用完
                                callback(Err(Error::new(ErrorKind::TimedOut, "mqttc receive timeout")));
                            },
                            Some(t_) => {
                                //超时时间未用完
                                handle_recv(client_copy, topic, pkid, bin, Some(t_), callback);
                            },
                        }
                    },
                };

            },
            Err(e) => {
                //接收回应包失败
                callback(Err(e));
            }
        }
    });
    client.0.connect.receive(timeout, response);
}

//处理接收的Mqtt包
fn handle_recv(client: SharedMqttClient, topic: Option<String>, pkid: Option<PacketIdentifier>, bin: Vec<u8>, timeout: Option<u32>, callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>) {
    match binary_to_packet(bin) {
        Ok(Packet::Connack(ack)) => {
            //处理连接回应
            match ack.code {
                ConnectReturnCode::Accepted => {
                    //连接已接受
                    callback(Ok(None));
                }
                reason => {
                    callback(Err(Error::new(ErrorKind::ConnectionRefused, format!("mqttc connect failed, reason: {:?}", reason))));
                }
            }
        },
        Ok(Packet::Suback(ack)) => {
            //处理订阅回应
            if let Some(id) = pkid {
                if ack.pkid != id {
                    callback(Err(Error::new(ErrorKind::Other, format!("mqttc subscribe failed, reason: invalid pkid {:?} {:?}", ack.pkid, id))));
                    return;
                }
            }

            let mut index = 0;
            for code in ack.return_codes {
                if let SubscribeReturnCodes::Success(_) = code {
                    index += 1;
                    continue;
                }
                callback(Err(Error::new(ErrorKind::Other, format!("mqttc subscribe failed, index: {:?}", index))));
                return;
            }

            callback(Ok(None)); //订阅成功
        },
        Ok(Packet::Unsuback(ack)) => {
            //处理退订回应
            if let Some(id) = pkid {
                if ack != id {
                    callback(Err(Error::new(ErrorKind::Other, format!("mqttc unsubscribe failed, reason: invalid pkid {:?} {:?}", ack, id))));
                    return;
                }
            }

            callback(Ok(None)); //取消订阅成功
        },
        Ok(Packet::Pingresp) => {
            //处理ping回应
            callback(Ok(None));
        },
        Ok(Packet::Publish(msg)) => {
            //处理发布消息
            if let Some(name) = topic {
                if msg.topic_name == name {
                    //处理指定主题的发布消息
                    if callback(Ok(Some(msg.payload.to_vec()))) {
                        //需要继续接收发布的消息
                        if timeout.is_some() {
                            //超时时间未用完，则继续
                            recv_packet(client, Some(name), pkid, timeout, callback);
                        } else {
                            //不超时，则继续
                            recv_packet(client, Some(name), pkid, timeout, callback);
                        }
                    }
                } else {
                    callback(Err(Error::new(ErrorKind::ConnectionRefused, format!("mqttc receive {:?} failed, reason: {:?}", name, msg.topic_name))));
                }
            }
        },
        resp => {
            //无效的回应
            callback(Err(Error::new(ErrorKind::Other, format!("mqttc receive invalid response, resp: {:?}", resp))));
        },
    }
}
