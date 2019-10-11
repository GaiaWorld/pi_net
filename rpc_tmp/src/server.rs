use std::io::Result;
/**
 * RPC传输协议：
 * 消息体：1字节表示压缩和版本,4字节消息ID，1字节超时时长（0表示不超时), 剩下的BonBuffer ,
 * 第一字节：前2位表示压缩算法,第3位表示差异比较，后5位表示版本（灰度）
 * 压缩算法：0：不压缩，1：lz4, 2:zstd
 */
use std::sync::Arc;
use std::time::SystemTime;
use std::net::SocketAddr;

use atom::Atom;

use mqtt_tmp::data::{Server, SetAttrFun};
use mqtt_tmp::server::{ClientStub, ServerNode};
use mqtt_tmp::session::Session;

use handler::{Args, Handler};
use traits::RPCServerTraits;

use net::CloseFn;

use net::api::{Socket, Stream};

/**
* RPC服务器
*/
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
    /**
    * 构建一个RPC服务器
    * @param mqtt Mqtt服务器
    * @returns 返回RPC服务器
    */
    pub fn new(mqtt: ServerNode) -> Self {
        RPCServer { mqtt }
    }

    pub fn unset_topic_meta(&self, topic: Atom) {
        self.mqtt.unset_topic_meta(topic).is_ok();
    }
    //设置连接关闭回调
    pub fn handle_close(&self, socket_id: usize) {
        self.mqtt.handle_close(socket_id)
    }
    //
    pub fn add_stream(&self, socket: Socket, stream: Stream) {
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
                B = Option<SocketAddr>,
                C = Arc<Vec<u8>>,
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
            //println!("rpc_handle -----------------------------------------{:?}", &topic2);
            let rdata = r.unwrap();
            let uid = rdata[0..3].as_ptr();
            let mut session = Session::new(
                client.clone(),
                sync,
                u32::from_be(unsafe { *(uid as *mut u32) }),
            );
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            session.set_timeout(now as usize, rdata[4]);
            let session = Arc::new(session);
            let rdata = Arc::new(rdata);
            let handle_func;
            {
                let topic2 = topic2.clone();
                let rdata = rdata.clone();
                let session = session.clone();
                let handle = handle.clone();
                handle_func = Box::new(move || {
                    handle.clone().handle(
                        session.clone(),
                        topic2.clone(),
                        Args::ThreeArgs(0, client.get_socket().peer_addr(), Arc::new(Vec::from(&rdata[5..]))),
                    )
                });
            }
            //if sync {
            //    println!("rpc_handle -----------------------------------------{:?}", &topic2);
                handle_func();
            // } else if client.get_queue_size() == 0 {
            //     println!("rpc_handle queue_push 0 -----------------------------------------{:?}", &topic2);
            //     handle
            //         .clone()
            //         .handle(session.clone(), topic2.clone(), Args::TwoArgs(0, Arc::new(Vec::from(&rdata[5..]))));
            //     client.queue_push(handle_func);
            // } else {
            //     println!("rpc_handle queue_push 1-----------------------------------------{:?}", &topic2);
            //     client.queue_push(handle_func);
            // }
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
