use std::io::Result;
/**
 * RPC传输协议：
 * 消息体：1字节表示压缩和版本,4字节消息ID，1字节超时时长（0表示不超时), 剩下的BonBuffer ,
 * 第一字节：前2位表示压缩算法,第3位表示差异比较，后5位表示版本（灰度）
 * 压缩算法：0：不压缩，1：lz4, 2:zstd
 */
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use pi_lib::atom::Atom;

use mqtt::data::{Server, SetAttrFun};
use mqtt::server::{ClientStub, ServerNode};
use mqtt::session::Session;
use mqtt::util;

use pi_base::util::uncompress;
use pi_lib::handler::{Args, Handler};
use traits::RPCServerTraits;

use net::{CloseFn, Socket, Stream};

#[derive(Clone)]
pub struct RPCServer {
    pub mqtt: ServerNode,
}

// enum Compress {
//     NO = 0,
//     RSYNC,
//     LZ4_BLOCK = 2,
//     LZ4_SEREAM,
// }

impl RPCServer {
    pub fn new(mqtt: ServerNode) -> Self {
        RPCServer { mqtt }
    }

    pub fn unset_topic_meta(&self, topic: Atom) {
        self.mqtt.unset_topic_meta(topic).is_ok();
    }
    //设置连接关闭回调
    pub fn set_close_callback(&self, stream: &mut Stream, func: CloseFn) {
        self.mqtt.set_close_callback(stream, func)
    }
    //
    pub fn add_stream(&self, socket: Socket, stream: Arc<RwLock<Stream>>) {
        self.mqtt.add_stream(socket, stream)
    }
    //为连接设置初始化attr
    pub fn set_attr(&self, handler: SetAttrFun) -> Result<()> {
        self.mqtt.set_attr(handler)
    }
}

impl RPCServerTraits for RPCServer {
    fn register(
        &self,
        topic: Atom,
        sync: bool,
        handle: Arc<
            Handler<
                A = u8,
                B = Arc<Vec<u8>>,
                C = (),
                D = (),
                E = (),
                F = (),
                G = (),
                H = (),
                HandleResult = (),
            >,
        >,
    ) -> Result<()> {
        let topic2 = topic.clone();
        let rpc_handle = move |client: ClientStub, r: Result<Arc<Vec<u8>>>| {
            let data = r.unwrap();
            let header = data[0];
            //压缩版本
            let compress = (&header >> 6) as u8;
            //消息版本
            let vsn = &header & 0b11111;
            let uid = data[1..4].as_ptr();
            let mut session = Session::new(
                client.clone(),
                sync,
                u32::from_be(unsafe { *(uid as *mut u32) }),
            );
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            session.set_timeout(now as usize, data[5]);
            let mut rdata = Vec::new();
            match compress {
                util::UNCOMPRESS => rdata.extend_from_slice(&data[6..]),
                util::LZ4_BLOCK => {
                    let mut vec_ = Vec::new();
                    uncompress(&data[6..], &mut vec_).is_ok();
                    rdata.extend_from_slice(&vec_[..]);
                }
                _ => session.close(),
            }
            let session = Arc::new(session);
            let rdata = Arc::new(rdata);
            let handle_func;
            {
                let topic2 = topic2.clone();
                let vsn = vsn;
                let rdata = rdata.clone();
                let session = session.clone();
                let handle = handle.clone();
                handle_func = Box::new(move || {
                    handle.clone().handle(
                        session.clone(),
                        topic2.clone(),
                        Args::TwoArgs(vsn, rdata),
                    )
                });
            }
            if sync {
                handle_func();
            } else if client.get_queue_size() == 0 {
                handle
                    .clone()
                    .handle(session.clone(), topic2.clone(), Args::TwoArgs(vsn, rdata));
                client.queue_push(handle_func);
            } else {
                client.queue_push(handle_func);
            }
        };
        match self.mqtt.set_topic_meta(
            topic,
            true,
            true,
            //None,
            Box::new(move |c, r| rpc_handle(c, r)),
        ) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn unregister(&self, topic: Atom) -> Result<()> {
        self.unset_topic_meta(topic);
        Ok(())
    }
}
