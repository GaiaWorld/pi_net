use std::io::Result;
/**
 * RPC传输协议：
 * 消息体：1字节表示压缩和版本,4字节消息ID，1字节超时时长（0表示不超时), 剩下的BonBuffer ,
 * 第一字节：前3位表示压缩算法，后5位表示版本（灰度）
 * 压缩算法：0：不压缩，1：rsync, 2:LZ4 BLOCK, 3:LZ4 SEREAM, 4、5、6、7预留
 */
use std::sync::{Arc, Mutex, RwLock};

use fnv::FnvHashMap;
use pi_lib::atom::Atom;

use mqtt3;
use mqtt3::LastWill;

use mqtt::client::ClientNode;
use mqtt::data::{Client, ClientCallback};
use mqtt::session::{LZ4_BLOCK, UNCOMPRESS};
use mqtt::util;

use net::{Socket, Stream};
use net::timer::{NetTimers, TimerCallback};

use pi_base::util::{compress, uncompress, CompressLevel};
use traits::RPCClientTraits;

#[derive(Clone)]
pub struct RPCClient {
    mqtt: ClientNode,
    msg_id: Arc<Mutex<u32>>,
    handlers: Arc<Mutex<FnvHashMap<u32, Box<Fn(Result<Arc<Vec<u8>>>)>>>>,
}

unsafe impl Sync for RPCClient {}
unsafe impl Send for RPCClient {}

impl RPCClient {
    pub fn new(mqtt: ClientNode) -> Self {
        RPCClient {
            mqtt,
            msg_id: Arc::new(Mutex::new(0)),
            handlers: Arc::new(Mutex::new(FnvHashMap::default())),
        }
    }
    pub fn connect(
        &self,
        keep_alive: u16,        //ping-pong
        will: Option<LastWill>, //遗言
        close_func: Option<ClientCallback>,
        connect_func: Option<ClientCallback>,
    ) {
        println!("rpc client connect!!!!!!!!");
        //连接MQTTser
        self.mqtt
            .connect(keep_alive, will, close_func, connect_func);
        let handlers = self.handlers.clone();
        //topic回调方法
        let topic_handle = move |r: Result<(Socket, &[u8])>| {
            let (socket, data) = r.unwrap();
            let header = data[0];
            //压缩版本
            let compress = (&header >> 6) as u8;
            //消息版本
            let _vsn = &header & 0b11111;
            let msg_id = u32::from_be(unsafe { *((data[1..4].as_ptr()) as *mut u32) });
            let mut rdata = Vec::new();
            match compress {
                UNCOMPRESS => rdata.extend_from_slice(&data[6..]),
                LZ4_BLOCK => {
                    let mut vec_ = Vec::new();
                    uncompress(&data[6..], &mut vec_).is_ok();
                    rdata.extend_from_slice(&vec_[..]);
                }
                _ => socket.close(true),
            }
            let rdata = Arc::new(rdata);
            let mut handlers = handlers.lock().unwrap();
            match handlers.get(&msg_id) {
                Some(func) => {
                    func(Ok(rdata));
                }
                None => socket.close(true),
            };
            handlers.remove(&msg_id);
        };
        self.mqtt
            .set_topic_handler(
                Atom::from(String::from("$r").as_str()),
                Box::new(move |r| topic_handle(r)),
            )
            .is_ok();
    }

    pub fn set_stream(&self, socket: Socket, stream: Arc<RwLock<Stream>>) {
        self.mqtt.set_stream(socket, stream)
    }

    pub fn get_timers(&self) -> Arc<RwLock<NetTimers<TimerCallback>>> {
        self.mqtt.get_timers()
    }

    pub fn set_topic_handler(
        &self,
        name: Atom,
        handler: Box<Fn(Result<(Socket, &[u8])>)>,
    ) -> Result<()> {
        self.mqtt.set_topic_handler(name, handler)
    }
}

impl RPCClientTraits for RPCClient {
    fn request(
        &self,
        topic: Atom,
        msg: Vec<u8>,
        resp: Box<Fn(Result<Arc<Vec<u8>>>)>,
        timeout: u8,
    ) {
        println!("pi_net rpc client request !!!!!!!!!!!!");
        *self.msg_id.lock().unwrap() += 1;
        println!("pi_net rpc client request 00000000000000");
        let socket = self.mqtt.get_socket();
        println!("pi_net rpc client request 00000000000000");
        let mut buff: Vec<u8> = vec![];
        println!("pi_net rpc client request 00000000000000");
        let msg_size = msg.len();
        println!("pi_net rpc client request 00000000000000");
        let msg_id = *self.msg_id.lock().unwrap();
        let mut compress_vsn = UNCOMPRESS;
        let mut body = vec![];
        if msg_size > 64 {
            compress_vsn = LZ4_BLOCK;
            compress(msg.as_slice(), &mut body, CompressLevel::High).is_ok();
        } else {
            body = msg;
        }
        println!("pi_net rpc client request 00000000000000");
        //第一字节：3位压缩版本、5位消息版本 TODO 消息版本以后定义
        buff.push(((compress_vsn << 6) | 0) as u8);
        let b1: u8 = ((msg_id >> 24) & 0xff) as u8;
        let b2: u8 = ((msg_id >> 16) & 0xff) as u8;
        let b3: u8 = ((msg_id >> 8) & 0xff) as u8;
        let b4: u8 = (msg_id & 0xff) as u8;
        //4字节消息ID
        buff.extend_from_slice(&[b1, b2, b3, b4]);
        //一字节超时时长（秒）
        buff.push(timeout);
        //剩下的消息体
        buff.extend_from_slice(body.as_slice());
        //发布消息
        util::send_publish(&socket, false, mqtt3::QoS::AtMostOnce, &topic, buff);
        println!("pi_net rpc client request 11111111111");
        let mut handlers = self.handlers.lock().unwrap();
        println!("pi_net rpc client request 2222222222");
        handlers.insert(msg_id, resp);
        println!("pi_net rpc client request 333333333");
    }
}
