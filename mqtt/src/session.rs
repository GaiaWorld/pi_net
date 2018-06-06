use std::io::{Result};
use std::sync::{Arc};

use server::{ClientStub};
use string_cache::DefaultAtom as Atom;

//LZ4_BLOCK 压缩
pub const LZ4_BLOCK: u8 = 2;
//不压缩
pub const UNCOMPRESS: u8 = 0;

#[derive(Clone)]
pub struct Session {
    pub client: ClientStub,
    pub msg_id: Option<u32>,
    pub seq: bool,
    pub timeout: usize,
}

//会话
impl Session {
    pub fn new(client: ClientStub, seq: bool) -> Self {
        Session {
            client,
            msg_id: None,
            seq,
            timeout: 0,
        }
    }

    //发布消息
    pub fn send(&self, topic: Atom, msg: Vec<u8>) -> Result<()> {
        // let mut body;
        // let mut compress = UNCOMPRESS;
        // if msg.len() > 64 {
        //     compress = LZ4_BLOCK
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