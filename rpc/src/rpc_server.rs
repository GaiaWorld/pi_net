use std::io::{Error, Result};
/**
 * RPC传输协议：
 * 消息体：1字节表示压缩和版本，4字节消息ID，剩下的BonBuffer
 * 第一字节：前3位表示压缩算法，后5位表示版本（灰度）
 * 压缩算法：0：不压缩，1：rsync, 2:LZ4 BLOCK, 3:LZ4 SEREAM, 4、5、6、7预留
 */
use std::sync::{Arc, RwLock};

use fnv::FnvHashMap;
use string_cache::DefaultAtom as Atom;

use mqtt::{ClientStub, Server, ServerNode as MQTT};

use handler::TopicHandler;

use traits::RPCServer;
use util::{compress, uncompress, CompressLevel};

pub struct RpcServer {
    mqtt: MQTT,
    sessions: Arc<RwLock<FnvHashMap<Atom, Arc<RwLock<Session>>>>>,
    }

// enum Compress {
//     NO = 0,
//     RSYNC,
//     LZ4_BLOCK = 2,
//     LZ4_SEREAM,
// }

#[derive(Clone)]
pub struct Session {
    client: Arc<ClientStub>,
    msg_id: Option<usize>,
    gray: Option<u8>,
}

impl RpcServer {
    pub fn new(mqtt: MQTT) -> Self {
        RpcServer{
            mqtt,
            sessions: Arc::new(RwLock::new(FnvHashMap::default())),
        }
    }
}

//会话
impl Session {
    pub fn new(client: Arc<ClientStub>) -> Self {
        Session {
            client,
            msg_id: None,
            gray: None,
        }
    }

    //发布消息
    pub fn send(&self, topic: Atom, msg: Vec<u8>) -> Result<()> {
        //大于64k需要压缩
        if msg.len() > 64 {
            
        }
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
    pub fn close(&self) {}
    pub fn get_gray(&self) -> Option<u8> {
        self.gray
    }
    pub fn set_gray(&mut self, gray: u8) {
        self.gray = Some(gray);
    }
}

impl RPCServer for RpcServer {
    fn register_sync(&mut self, topic: Atom, func: TopicHandler) -> Result<()> {
        let sessions = self.sessions.clone();
        let rpc_handle = |client: Arc<ClientStub>, r: Result<Arc<Vec<u8>>>| {
            let data = r.unwrap();
            let header = data[0];
            let compress = (&header >> 5) as u8;
            let vsn = &header & 0b11111;
            let [a, b, c] = data[1..4];
            let mut session;
            if let Some(s) = sessions.write().unwrap().get(&topic) {
                session = s;
            } else {
                let s = Arc::new(RwLock::new(Session::new(client)));
                session = &s;
                sessions.write().unwrap().insert(topic, s);
                func.set_session(s.clone());
            }
            session.write().unwrap().msg_id = Some(((a << 16) + (b << 8) + c) as usize);
            let mut rdata = Vec::new();
            match compress {
                2 => {
                    let mut vec_ = Vec::new();
                    uncompress(&data[5..], &mut vec_).is_ok();
                    rdata.extend_from_slice(&vec_[..]);
                },
                _ => rdata.extend_from_slice(&data[5..]),
            }

            func.handle(topic, vsn, Arc::new(rdata));
        };
        self.mqtt
            .set_topic_meta(topic, true, true, None, Box::new(move |c, r| rpc_handle(c, r)));
        Ok(())
    }

    fn unregister(&mut self, topic: Atom) -> Result<()> {
        self.mqtt.unset_topic_meta(topic);
        Ok(())
    }
}
