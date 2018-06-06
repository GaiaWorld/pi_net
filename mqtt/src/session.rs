use std::io::{Result};
use std::sync::{Arc};

use server::{ClientStub};
use string_cache::DefaultAtom as Atom;

#[derive(Clone)]
pub struct Session {
    pub client: ClientStub,
    pub msg_id: Option<u32>,
    pub seq: bool,
}

//会话
impl Session {
    pub fn new(client: ClientStub, seq: bool) -> Self {
        Session {
            client,
            msg_id: None,
            seq,
        }
    }

    //发布消息
    pub fn send(&self, topic: Atom, msg: Vec<u8>) -> Result<()> {
        // let mut body;
        // if msg.len() > 64 {

        // }
        Ok(())
    }
    //获取会话表数据
    pub fn get_attr(&self, key: Atom) -> Option<Arc<Vec<u8>>> {
        Some(Arc::new(vec![]))
    }
    //设置会话表
    pub fn set_attr(&self, key: Atom, Value: Option<Arc<[u8]>>) -> Result<()> {
        Ok(())
    }
    pub fn close(&self) {}
}