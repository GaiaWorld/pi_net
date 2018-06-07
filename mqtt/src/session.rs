use std::io::Result;
use std::sync::Arc;
use mqtt3::{self, Packet};

use server::ClientStub;
use pi_lib::atom::Atom;
use pi_base::util::{compress, uncompress, CompressLevel};
use util;

//LZ4_BLOCK 压缩
pub const LZ4_BLOCK: u8 = 2;
//不压缩
pub const UNCOMPRESS: u8 = 0;

#[derive(Clone)]
pub struct Session {
    pub client: ClientStub,
    pub msg_id: Option<u32>,
    pub seq: bool,
    pub timeout: (usize, usize),
}

//会话
impl Session {
    pub fn new(client: ClientStub, seq: bool) -> Self {
        Session {
            client,
            msg_id: None,
            seq,
            timeout: (0, 0),
        }
    }

    //发布消息
    pub fn send(&self, topic: Atom, msg: Vec<u8>) -> Result<()> {
        //从队列中取出当前handle
        let topic_handle = self.client.queue_pop();
        let mut buff: Vec<u8> = vec![];
        let msg_size = msg.len();
        let msg_id = self.msg_id.unwrap();
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

        let t = mqtt3::TopicPath::from_str((*topic).clone().as_str());
        //发送数据
        util::send_publish(
            &self.client.socket.clone(),
            false,
            mqtt3::QoS::AtMostOnce,
            t.unwrap().path.as_str(),
            buff.clone(),
        );
        //检查队列中是否还有未处理的handle
        if self.client.get_queue_size() > 0 {
            let func = self.client.queue_pop().unwrap();
            func();
        }
        Ok(())
    }
    //获取会话表数据
    pub fn get_attr(&self, key: Atom) -> Option<Arc<Vec<u8>>> {
        let attr = self.client.attributes.read().unwrap();
        if let Some(v) = attr.get(&key) {
            return Some(v.clone())
        }
        None
    }
    //设置会话表
    pub fn set_attr(&self, key: Atom, value: Option<Arc<Vec<u8>>>) -> Option<Arc<Vec<u8>>> {
        let mut attr = self.client.attributes.write().unwrap();
        if let Some(v) = value {
            return attr.insert(key, v)
        } else {
            return attr.remove(&key)
        }
    }
    pub fn close(&self) {
        self.client.socket.close(true)
    }
}
