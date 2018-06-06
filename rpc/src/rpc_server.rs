use std::io::{Error, Result};
/**
 * RPC传输协议：
 * 消息体：1字节表示压缩和版本, 1字节超时时长（0表示不超时），4字节消息ID，剩下的BonBuffer
 * 第一字节：前3位表示压缩算法，后5位表示版本（灰度）
 * 压缩算法：0：不压缩，1：rsync, 2:LZ4 BLOCK, 3:LZ4 SEREAM, 4、5、6、7预留
 */
use std::sync::{Arc, RwLock};

use fnv::FnvHashMap;
use string_cache::DefaultAtom as Atom;

use mqtt::{ClientStub, Server, ServerNode as MQTT};
use mqtt::session::Session;
use mqtt::handler::TopicHandle;

use traits::RPCServer;
use util::{compress, uncompress, CompressLevel};

pub struct RpcServer {
    mqtt: MQTT,
}

// enum Compress {
//     NO = 0,
//     RSYNC,
//     LZ4_BLOCK = 2,
//     LZ4_SEREAM,
// }



impl RpcServer {
    pub fn new(mqtt: MQTT) -> Self {
        RpcServer { mqtt }
    }
}



impl RPCServer for RpcServer {
    fn register(&mut self, topic: Atom, sync: bool, topic_handle: Arc<TopicHandle>) -> Result<()> {
        let topic2 = topic.clone();
        let rpc_handle = move |client: ClientStub, r: Result<Arc<Vec<u8>>>| {
            let data = r.unwrap();
            let header = data[0];
            let compress = (&header >> 5) as u8;
            let vsn = &header & 0b11111;

            let mut session = Session::new(client.clone(), sync);
            let uid = data[1..4].as_ptr();
            session.msg_id = Some(u32::from_be(unsafe { *(uid as *mut u32) }));
            let mut rdata = Vec::new();
            match compress {
                2 => {
                    let mut vec_ = Vec::new();
                    uncompress(&data[5..], &mut vec_).is_ok();
                    rdata.extend_from_slice(&vec_[..]);
                }
                _ => rdata.extend_from_slice(&data[5..]),
            }
            if sync {
                topic_handle.handle(topic2.clone(), vsn, Arc::new(session), Arc::new(rdata));
            } else if client.get_queue_size() > 0 {
                
            }

            
        };
        match self.mqtt.set_topic_meta(
            topic,
            true,
            true,
            None,
            Box::new(move |c, r| rpc_handle(c, r)),
        ) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn unregister(&mut self, topic: Atom) -> Result<()> {
        self.mqtt.unset_topic_meta(topic).is_ok();
        Ok(())
    }
}
