use std::io::{Cursor, Error, ErrorKind, Result};
use std::sync::{Arc, RwLock};

use rand::{self, Rng};

use mqtt3::{self, MqttRead, MqttWrite, Packet, PacketIdentifier, QoS};

use net::{Socket, Stream};
use net::net::recv;

//LZ4_BLOCK 压缩
pub const LZ4_BLOCK: u8 = 1;
//不压缩
pub const UNCOMPRESS: u8 = 0;

type MqttRecvCallback = Box<FnMut(Result<Packet>)>;

pub fn send_connect(socket: &Socket, keep_alive: u16, last_will: Option<mqtt3::LastWill>) {
    send_packet(
        socket,
        Packet::Connect(mqtt3::Connect {
            protocol: mqtt3::Protocol::MQTT(4),
            keep_alive: keep_alive,
            client_id: gen_client_id(),
            clean_session: true,
            last_will: last_will,
            username: None,
            password: None,
        }),
    );
}

pub fn send_subscribe(socket: &Socket, id: u16, topics: Vec<(String, mqtt3::QoS)>) {
    let mut ts = vec![];
    for &(ref name, ref qos) in topics.iter() {
        ts.push(mqtt3::SubscribeTopic {
            qos: qos.clone(),
            topic_path: name.clone(),
        });
    }

    send_packet(
        socket,
        Packet::Subscribe(mqtt3::Subscribe {
            pid: PacketIdentifier(id),
            topics: ts,
        }),
    );
}

pub fn send_unsubscribe(socket: &Socket, id: u16, topics: Vec<String>) {
    send_packet(
        socket,
        Packet::Unsubscribe(mqtt3::Unsubscribe {
            pid: PacketIdentifier(id),
            topics,
        }),
    );
}

pub fn send_disconnect(socket: &Socket) {
    send_packet(socket, Packet::Disconnect);
    //关闭连接
    socket.close(true);
}

pub fn send_connack(socket: &Socket, code: mqtt3::ConnectReturnCode) {
    send_packet(
        socket,
        Packet::Connack(mqtt3::Connack {
            code,
            session_present: false,
        }),
    );
}

pub fn send_suback(socket: &Socket, id: PacketIdentifier, codes: Vec<mqtt3::SubscribeReturnCodes>) {
    send_packet(
        socket,
        Packet::Suback(mqtt3::Suback {
            pid: id,
            return_codes: codes,
        }),
    );
}

pub fn send_unsuback(socket: &Socket, id: PacketIdentifier) {
    send_packet(socket, Packet::Unsuback(id));
}

//发送pong
pub fn send_pingresp(socket: &Socket) {
    send_packet(socket, Packet::Pingresp);
}

//发送ping
pub fn send_pingreq(socket: &Socket) {
    send_packet(socket, Packet::Pingreq);
}

pub fn send_publish(socket: &Socket, retain: bool, _qos: QoS, topic: &str, payload: Vec<u8>) {
    println!("send_publish------------------------");
    send_packet(
        socket,
        Packet::Publish(mqtt3::Publish {
            dup: false,
            qos: QoS::AtMostOnce,
            retain: retain,
            topic_name: topic.to_string(),
            pid: None,
            payload: Arc::new(payload),
        }),
    );
}

pub fn recv_mqtt_packet(stream: Arc<RwLock<Stream>>, func: MqttRecvCallback) {
    recv_header(stream, func);
}

fn send_packet(socket: &Socket, packet: Packet) {
    let mut stream = Cursor::new(Vec::new());
    stream.write_packet(&packet).unwrap();
    socket.send(Arc::new(stream.into_inner()));
    //socket.send_bin(Arc::new(stream.into_inner()));
}

fn gen_client_id() -> String {
    let mut rng = rand::thread_rng();
    let id = rng.gen::<u32>();
    return format!("mqtt_{}", id);
}

fn recv_header(stream: Arc<RwLock<Stream>>, func: MqttRecvCallback) {
    const FIXTED_LEN: usize = 1;
    let stream2 = stream.clone();
    let handle_header;
    {
        let mut pack = vec![];
        let stream = stream.clone();
        handle_header = Box::new(move |data: Result<Arc<Vec<u8>>>| {
            pack.extend_from_slice(data.unwrap().as_slice());
            // let size = get_recv_size(pack.as_slice()).unwrap();
            // recv_pack(stream, pack, size, func);
            recv_header2(stream, pack, func);
        });
    }

    let r = recv(stream2, FIXTED_LEN, handle_header);
    if let Some((func, data)) = r {
        func(data);
    }
}

fn recv_header2(stream: Arc<RwLock<Stream>>, packs: Vec<u8>, func: MqttRecvCallback) {
    const FIXTED_LEN: usize = 1;
    let stream2 = stream.clone();
    let handle_header2;
    {
        let mut pack = vec![];
        let stream = stream.clone();
        handle_header2 = Box::new(move |data: Result<Arc<Vec<u8>>>| {
            pack.extend_from_slice(packs.as_slice());
            pack.extend_from_slice(data.unwrap().as_slice());
            match if_ack_size(pack.as_slice()) {
                Ok(-1) => {
                    recv_header2(stream.clone(), pack, func);
                },
                Ok(size) => {
                    println!("!!!!!!!!!recv_pack_size:{}", size);
                    recv_pack(stream.clone(), pack, size as usize, func);
                },
                Err(e) => println!("recv_header error: {}", e)
            }
        })
    }
    let r = recv(stream2, FIXTED_LEN, handle_header2);
    if let Some((func, data)) = r {
        func(data);
    }
}

fn recv_pack(
    stream: Arc<RwLock<Stream>>,
    mut pack: Vec<u8>,
    recv_size: usize,
    mut func: MqttRecvCallback,
) {
    let stream2 = stream.clone();
    let handler_pack;
    {
        handler_pack = Box::new(move |data: Result<Arc<Vec<u8>>>| {
            pack.extend_from_slice(data.unwrap().as_slice());
            let mut cursor = Cursor::new(pack);
            let packet = cursor.read_packet().unwrap();
            println!("!!!!!!!!!!!!recv_pack!!!!!!!packet:{:?}", packet);
            func(Ok(packet));
        });
    }

    let r = recv(stream2, recv_size, handler_pack);
    if let Some((func, data)) = r {
        func(data);
    }
}

pub fn encode(msg: Vec<u8>) -> Vec<u8> {
    let  mut msg = msg;
    //let msg_size = msg.len();
    let compress_vsn = UNCOMPRESS;
    
    //暂不处理解压问题
    // if msg_size > 64 {
    //     println!("LZ4_BLOCK=-------------------------------------------");
    //     compress_vsn = LZ4_BLOCK;
        // let body = vec![];
        // body.extend_from_slice(&unsafe{transmute::<u32, [u8; 4]>(len as u32)});
    //     compress(buff.as_slice(), &mut body, CompressLevel::High).is_ok();
        // buff = body;
    // } else {
        
    // }

    //let len = buff.len();

    //第一字节：3位压缩版本、5位消息版本 TODO 消息版本以后定义
    msg.insert(0, ((compress_vsn << 6) | 0) as u8);
    //剩下的消息体
    
    //println!("encode--------------------{:?}", &buff);
    return msg;
}

// fn get_recv_size(pack: &[u8]) -> Result<usize> {
//     let mut mult: usize = 1;
//     let mut len: usize = 0;
//     let mut done = false;

//     const MULTIPLIER: usize = 0x80 * 0x80 * 0x80 * 0x80;

//     let mut i = 1;
//     while !done && i < pack.len() {
//         len += mult * (pack[i] & 0x7F) as usize;
//         mult *= 0x80;
//         if mult > MULTIPLIER {
//             return Err(Error::new(ErrorKind::Other, "mult > MULTIPLIER"));
//         }

//         done = (pack[i] & 0x80) == 0;
//         i += 1;
//     }
//     if i <= pack.len() {
//         let mut r = 0;
//         if len > pack.len() - i {
//             r = len - (pack.len() - i);
//         }
//         return Ok(r);
//     } else {
//         return Err(Error::new(ErrorKind::Other, "i < pack.len()"));
//     }
// }

fn if_ack_size(pack: &[u8]) -> Result<isize> {
    if pack[pack.len() - 1] != 0 {
        return Ok(-1);
    }
    println!("!!!!!!!!!!ack_size pack:{:?}", pack);
    let mut mult: usize = 1;
    let mut len: usize = 0;
    let mut done = false;

    const MULTIPLIER: usize = 0x80 * 0x80 * 0x80 * 0x80;

    let mut i = 1;
    while !done && i < pack.len() {
        len += mult * (pack[i] & 0x7F) as usize;
        mult *= 0x80;
        if mult > MULTIPLIER {
            return Err(Error::new(ErrorKind::Other, "mult > MULTIPLIER"));
        }

        done = (pack[i] & 0x80) == 0;
        i += 1;
    }
    if i <= pack.len() {
        let mut r = 0;
        if len > pack.len() - i {
            r = len - (pack.len() - i);
        }
        return Ok(r as isize);
    } else {
        return Ok(-1);
    }
}
