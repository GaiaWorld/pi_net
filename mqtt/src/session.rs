use std::sync::Arc;
use mqtt3::{self};

use server::ClientStub;
use pi_lib::atom::Atom;
use pi_base::util::{compress, CompressLevel};
use util;

//LZ4_BLOCK 压缩
pub const LZ4_BLOCK: u8 = 2;
//不压缩
pub const UNCOMPRESS: u8 = 0;

#[derive(Clone)]
pub struct Session {
    client: ClientStub,
    msg_id: u32,
    seq: bool,
    timeout: (usize, u8), //(系统当前时间, 超时时长)
}

//会话
impl Session {
    pub fn new(client: ClientStub, seq: bool, msg_id: u32) -> Self {
        Session {
            client,
            msg_id,
            seq,
            timeout: (0, 0),
        }
    }

    //发送消息
    pub fn send(&self, _topic: Atom, msg: Vec<u8>) {
        let mut buff: Vec<u8> = vec![];
        let msg_size = msg.len();
        let msg_id = self.msg_id;
        let timeout = self.timeout.1;
        let mut compress_vsn = UNCOMPRESS;
        let mut body = vec![];
        if msg_size > 64 {
            compress_vsn = LZ4_BLOCK;
            compress(msg.as_slice(), &mut body, CompressLevel::High).is_ok();
        } else {
            body = msg;
        }
        //第一字节：3位压缩版本、5位消息版本 TODO 消息版本以后定义
        buff.push(((compress_vsn << 5) | 0) as u8);
        let b1: u8 = ((msg_id >> 24) & 0xff) as u8;
        let b2: u8 = ((msg_id >> 16) & 0xff) as u8;
        let b3: u8 = ((msg_id >> 8) & 0xff) as u8;
        let b4: u8 = (msg_id & 0xff) as u8;
        //4字节消息ID
        buff.extend_from_slice(&[b1, b2, b3, b4]);
        //一字节超时时长（秒）
        buff.push(timeout as u8);
        //剩下的消息体
        buff.extend_from_slice(body.as_slice());

        let t = mqtt3::TopicPath::from_str(String::from("$r").as_str());
        //发送数据
        util::send_publish(
            &self.client.get_socket(),
            false,
            mqtt3::QoS::AtMostOnce,
            t.unwrap().path.as_str(),
            buff.clone(),
        );
    }
    //回应消息
    pub fn respond(&self, topic: Atom, msg: Vec<u8>) {
        if self.seq {
            self.send(topic, msg);    
        } else {
            self.client.queue_pop();
            self.send(topic, msg);
            //检查队列中是否还有未处理的handle
            if self.client.get_queue_size() > 0 {
                let func = self.client.queue_pop().unwrap();
                func();
            }
        }
    }
    //获取会话表数据
    pub fn get_attr(&self, key: Atom) -> Option<Arc<Vec<u8>>> {
        let attr = self.client.get_attributes();
        let attr = attr.read().unwrap();
        if let Some(v) = attr.get(&key) {
            return Some(v.clone())
        }
        None
    }
    //设置会话表
    pub fn set_attr(&self, key: Atom, value: Option<Arc<Vec<u8>>>) -> Option<Arc<Vec<u8>>> {
        let attr = self.client.get_attributes();
        let mut attr = attr.write().unwrap();
        if let Some(v) = value {
            return attr.insert(key, v)
        } else {
            return attr.remove(&key)
        }
    }
    pub fn close(&self) {
        self.client.get_socket().close(true)
    }
    pub fn set_timeout(&mut self, systime: usize, timeout: u8) {
        self.timeout = (systime, timeout);
    }
}
