//use std::sync::Arc;
//use std::mem::transmute;

use mqtt3;

//use pi_base::util::{compress, CompressLevel};
use atom::Atom;
use gray::GrayVersion;
use server::ClientStub;
use util;
use net::api::Socket;

#[derive(Clone, Debug)]
pub struct Session {
    pub client: ClientStub,
    pub msg_id: u32,
    pub seq: bool,
    pub timeout: (usize, u8), //(系统当前时间, 超时时长)
}

unsafe impl Sync for Session {}
unsafe impl Send for Session {}

pub fn encode_reps(msg_id: u32, timeout: u8, msg: Vec<u8>) -> Vec<u8> {
    let mut buff: Vec<u8> = vec![];
    let b1: u8 = ((msg_id >> 24) & 0xff) as u8;
    let b2: u8 = ((msg_id >> 16) & 0xff) as u8;
    let b3: u8 = ((msg_id >> 8) & 0xff) as u8;
    let b4: u8 = (msg_id & 0xff) as u8;
    //4字节消息ID
    buff.extend_from_slice(&[b1, b2, b3, b4]);
    //一字节超时时长（秒）
    buff.push(timeout as u8);
    buff.extend_from_slice(msg.as_slice());
    buff
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
    pub fn send(&self, topic: Atom, msg: Vec<u8>) {
        let msg_id = self.msg_id;
        let timeout = self.timeout.1;
        let buff =  util::encode(encode_reps(msg_id, timeout, msg));
        let t = mqtt3::TopicPath::from_str((*topic).as_str());
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
    pub fn respond(&self, _topic: Atom, msg: Vec<u8>) {
        if self.seq {
            //println!("respond 111111111 msg = {:?}", msg);
            self.send(Atom::from("$r"), msg);
        } else {
            //println!("respond 2222222 msg = {:?}", msg);
            self.client.queue_pop();
            self.send(Atom::from("$r"), msg);
            //检查队列中是否还有未处理的handle
            if self.client.get_queue_size() > 0 {
                (self.client.queue_pop().unwrap())();
            }
        }
    }
    pub fn close(&self) {
        match &self.client.get_socket() {
            &Socket::Raw(ref s) => s.close(true),
            &Socket::Tls(ref s) => s.close(true),
        }
    }

    pub fn set_timeout(&mut self, systime: usize, timeout: u8) {
        self.timeout = (systime, timeout);
    }
}

impl GrayVersion for Session {
    fn get_gray(&self) -> &Option<usize>{
        &self.client.get_gray()
    }

    fn set_gray(&mut self, gray: Option<usize>){
        self.client.set_gray(gray);
    }

    fn get_id(&self) -> usize {
        self.client.get_id()
    }
}
