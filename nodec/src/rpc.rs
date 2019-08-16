use std::time::Instant;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicU32, Ordering};

use worker::task::TaskType;
use mqttc::SharedMqttClient;
use compress::{CompressLevel, compress, uncompress};
use atom::Atom;

/*
* 默认压缩标记
*/
const RPC_COMPRESS_FLAG: u8 = 0;

/*
* 默认版本号
*/
const RPC_VERSION_FLAG: u8 = 0;

/*
* 默认的rpc回应主题
*/
const RPC_RESPONSE_TOPIC: &str = "$r";

/*
* RPC客户端
* 3位压缩标记，0：不压缩，1：LZ4 BLOCK, 2:LZ4 SEREAM, 3、4、5、6、7预留
* 5位版本，4字节消息唯一id，1字节超时时长，0表示不超时，Bon序列化的消息体
*/
#[derive(Clone)]
pub struct RPCClient{
    id:         Arc<AtomicU32>,                                                     //消息id
    connect:    SharedMqttClient,                                                   //mqtt连接
    resp_tab:   Arc<Mutex<HashMap<u32, Arc<Fn(Result<Option<Vec<u8>>>) -> bool>>>>, //请求回应表，返回false表示处理完成
}

unsafe impl Send for RPCClient {}
unsafe impl Sync for RPCClient {}

impl RPCClient {
    //创建一个RPC客户端
    pub fn create(url: &str) -> Result<Self> {
        match SharedMqttClient::create(url) {
            Err(e) => Err(e),
            Ok(connect) => {
                Ok(RPCClient {
                    id: connect.get_id_gen(),
                    connect,
                    resp_tab: Arc::new(Mutex::new(HashMap::new())),
                })
            },
        }
    }

    //连接
    pub fn connect(&self,
                   keep_alive: u16,
                   client_id: &str,
                   timeout: u8,
                   connect_callback: Arc<Fn(Result<Option<Vec<u8>>>)>,
                   closed_callback: Arc<Fn(Result<Option<Vec<u8>>>)>) {
        //注册连接关闭回调
        self.resp_tab.lock().unwrap().insert(0, Arc::new(move |result: Result<Option<Vec<u8>>>| {
            closed_callback(result);
            false
        }));

        let callback = Arc::new(move |result: Result<Option<Vec<u8>>>| {
            connect_callback(result);
            false
        });
        self.connect.connect(None,
                       keep_alive,
                       client_id.to_string(),
                       true,
                       None,
                       None,
                       Some(timeout as u32 * 1000),
                       callback);
    }

    //注册可接收消息的cmd
    pub fn register(&self, cmd_list: Vec<String>, timeout: u8, callback:  Arc<Fn(Result<Option<Vec<u8>>>)>) {
        let register_callback = Arc::new(move |result: Result<Option<Vec<u8>>>| {
            callback(result);
            false
        });

        if timeout == 0 {
            self.connect.subscribe(cmd_list, None, register_callback);
        } else {
            self.connect.subscribe(cmd_list, Some(timeout as u32 * 1000), register_callback);
        }
    }

    //请求
    pub fn request(&self,
                   cmd: String,
                   body: Vec<u8>,
                   timeout: u8,
                   callback: Arc<Fn(Result<Option<Vec<u8>>>)>) {
        //分配本次请求的消息唯一id
        let uid = self.id.fetch_add(1, Ordering::Relaxed);

        //注册本次请求回调
        let client = self.clone();
        let receive_callback = Arc::new(move |result: Result<Option<Vec<u8>>>| {
            match result {
                Err(e) => {
                    //本次接收失败，则执行用户的请求回调
                    callback(Err(e));
                    false
                },
                Ok(None) => {
                    //收到空数据，则继续接收请求回应
                    true
                },
                Ok(Some(bin)) => {
                    if let Some((id, data)) = binary_to_rpc(&bin) {
                        if uid != id {
                            let receive_callback: Arc<Fn(Result<Option<Vec<u8>>>) -> bool>;
                            if let Some(cb) = client.resp_tab.lock().unwrap().get(&id) {
                                //是其它请求的回应
                                receive_callback = cb.clone();
                            } else {
                                //非法回应，则忽略，并继续等待本次请求的回应
                                return true;
                            }

                            //执行用户其它请求的接收回调
                            receive_callback(Ok(Some(bin)))
                        } else {
                            //是本次请求的回应，则执行用户的请求回调，并移除当前请求的接收回调
                            callback(Ok(data));
                            client.resp_tab.lock().unwrap().remove(&uid);
                            false
                        }
                    } else {
                        //收到无效数据，则继续接收请求回应
                        true
                    }
                },
            }
        });
        self.resp_tab.lock().unwrap().insert(uid, receive_callback.clone());

        //异步发送请求，并异步等待回应
        let now = Instant::now(); //请求开始时间
        let topic = RPC_RESPONSE_TOPIC.to_string();
        let bin = rpc_to_binary(RPC_COMPRESS_FLAG, RPC_VERSION_FLAG, uid, timeout, body);
        let t = if timeout == 0 {
            //不超时
            None
        } else {
            Some(timeout as u32 * 1000)
        };
        let client = self.clone();
        let request_callback = Arc::new(move |result: Result<Option<Vec<u8>>>| {
            let receive_callback = match client.resp_tab.lock().unwrap().get(&uid) {
                None => {
                    //未注册本次请求回调
                    Arc::new(move |result: Result<Option<Vec<u8>>>| false)
                },
                Some(cb) => {
                    //注册了本次请求回调
                    cb.clone()
                },
            };
            match result {
                Err(e) => {
                    //发送请求失败
                    receive_callback(Err(e));
                    false
                },
                Ok(_) => {
                    //发送请求成功，则异步接收回应
                    match t {
                        None => {
                            //不超时，则接收回应
                            client.connect.receive(topic.clone(), None, receive_callback);
                        },
                        Some(t) => {
                            //扣除发送请求的时长
                            match t.checked_sub(now.elapsed().as_millis() as u32) {
                                None => {
                                    //超时时间已用完
                                    receive_callback(Err(Error::new(ErrorKind::TimedOut, "rpc client receive timeout")));
                                },
                                Some(t_) => {
                                    //超时时间未用完，则接收回应
                                    client.connect.receive(topic.clone(), Some(t_), receive_callback);
                                },
                            }
                        },
                    }
                    false
                }
            }
        });
        self.connect.publish(cmd, Arc::new(bin), t.clone(), request_callback);
    }

    //关闭连接
    pub fn close(&self) {
        if let Some(closed_callback) = self.resp_tab.lock().unwrap().remove(&0){
            //注册了关闭回调
            self.connect.disconnect(Arc::new(move |result: Result<Option<Vec<u8>>>| {
                closed_callback(result); //调用用户关闭回调
                false
            }))
        } else {
            //未注册关闭回调
            self.connect.disconnect(Arc::new(move |result: Result<Option<Vec<u8>>>| false));
        }
    }
}

//将rpc请求序列化为binary
fn rpc_to_binary(compress_level: u8, version: u8, uid: u32, timeout: u8, body: Vec<u8>) -> Vec<u8> {
    let head = vec![(compress_level << 5) | (version & 0x1f),
                    ((uid >> 24) & 0xff) as u8,
                    ((uid >> 16) & 0xff) as u8,
                    ((uid >> 8) & 0xff) as u8,
                    (uid & 0xff) as u8,
                    timeout];

    match compress_level {
        0 => head.into_iter().chain(body).collect(),
        _ => {
            let mut buf = Vec::new();
            compress(&body[..], &mut buf, CompressLevel::Low);
            head.into_iter().chain(buf).collect()
        },
    }
}

//将binary反序列化为rpc回应
fn binary_to_rpc(bin: &[u8]) -> Option<(u32, Option<Vec<u8>>)> {
    if bin.len() < 7 {
        return None;
    }

    let compress_level = bin[0] >> 5;
    let version = bin[0] & 0x1f;
    let uid = (bin[1] as u32) << 24 & 0xffffffff | (bin[2] as u32) << 16 & 0xffffff | (bin[3] as u32) << 8 & 0xffff | (bin[4] & 0xff) as u32;

    match compress_level {
        0 => Some((uid, Some(Vec::from(&bin[6..])))),
        _ => {
            let mut buf = Vec::new();
            if let Ok(_) = uncompress(&bin[6..], &mut buf) {
                Some((uid, Some(buf)))
            } else {
                //解压缩失败
                None
            }
        }
    }
}







