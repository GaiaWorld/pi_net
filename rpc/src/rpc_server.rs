/**
 * RPC传输协议：
 * 消息体：1字节表示压缩和版本，4字节消息ID，剩下的BonBuffer
 * 第一字节：前3位表示压缩算法，后5位表示版本（灰度）
 * 压缩算法：0：不压缩，1：rsync, 2:LZ4 BLOCK, 3:LZ4 SEREAM, 4、5、6、7预留
 */

use std::sync::{Arc, RwLock};
use std::io::{Error, Result};

use string_cache::DefaultAtom as Atom;
use fnv::FnvHashMap;

use mqtt::{ServerNode as MQTT, ClientStub, Server};


use handler::TopicHandler;

use traits::RPCServer;

pub struct RpcServer(MQTT);

#[derive(Clone)]
pub struct Session {
    client: Arc<ClientStub>,
    msg_id: Option<usize>,
    gray: Option<u8>,
}

impl RpcServer {
    pub fn new(mqtt: MQTT) -> Self {
        RpcServer(mqtt)
    }
}

//会话
impl Session {
    pub fn new(client: Arc<ClientStub>) -> Self {
        Session{
            client,
            msg_id: None,
            gray: None,
        }
    }

    //发布消息
    pub fn send(&self, topic: Atom, msg: Vec<u8>) -> Result<()> {
        Ok(())
    }
    //获取会话表数据
    pub fn get_attr(&self, key: Atom) -> Result<Arc<Vec<u8>>> {
        Ok(Arc::new(vec![]))
    }
    //设置会话表
    pub fn set_attr(&self, key: Atom, Value: Option<Arc<[u8]>>) -> Result<()> {
        Ok(())
    }
    pub fn close(&self) {

    }
    pub fn get_gray(&self) -> Option<u8> {
        self.gray
    }
    pub fn set_gray(&mut self, gray: u8) {
        self.gray = Some(gray);
    }
}

impl RPCServer for RpcServer {
    fn register_sync(&mut self, topic: Atom, func: TopicHandler) -> Result<()> {
        let handle = | client: Arc<ClientStub>, r: Result<Arc<Vec<u8>>> | {
            let mut session = Session::new(client);
            
            let vsn: u8 = 0; //灰度版本
            //TODO 分析数据(解压、uid)
            //TODO msg_id写入session
            // func.handle(topic, session, vsn, r);
        };
        self.0.set_topic_meta(topic, true, true, None, Box::new(move |c, r| {handle(c, r)}));
        Ok(())
    }

    fn unregister(&mut self, topic: Atom) -> Result<()> {
        self.0.unset_topic_meta(topic);
        Ok(())
    }
}

