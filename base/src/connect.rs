use std::sync::Arc;
use std::net::SocketAddr;
use std::io::{Result, Error, ErrorKind};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicIsize, Ordering};

use bytes::BufMut;

use compress::{CompressLevel, compress, uncompress};

use tcp::util::SocketContext;
use mqtt::broker::MQTT_RESPONSE_SYS_TOPIC;
use mqtt::session::MqttConnect;

use crate::util::BaseSession;

/*
* 基础协议的上层监听器
*/
pub trait BaseInnerListener {
    //处理基础协议连接事件
    fn connected(&self, connect: &mut BaseConnect) -> Result<()>;

    //处理基础协议连接关闭事件
    fn closed(&self, connect: &mut BaseConnect, reason: Result<()>);
}

/*
* 基础协议的上层服务
*/
pub trait BaseInnerService {
    //指定基础协议的指定主题的服务
    fn request(&self, connect: &BaseConnect, topic: String, payload: Arc<Vec<u8>>) -> Result<()>;
}

/*
* 解码基础协议请求
*/
#[inline(always)]
pub fn decode(connect: &BaseConnect, bin: &[u8]) -> Result<Vec<u8>> {
    //解码头，并更新当前连接的头信息
    let compress_level = bin[0] >> 6;
    connect.compress_level.store(compress_level, Ordering::Relaxed);
    connect.compare.store(bin[0] & 0x20, Ordering::Relaxed);
    connect.version.store(bin[0] & 0x1f, Ordering::Relaxed);

    match compress_level {
        0 => Ok(Vec::from(&bin[1..])),
        _ => {
            let mut buf = Vec::new();
            if let Err(e) = uncompress(&bin[1..], &mut buf) {
                //解压缩失败
                return Err(Error::new(ErrorKind::Other, format!("uncompress failed by rpc decode, reason: {:?}", e)));
            }

            Ok(buf)
        }
    }
}

/*
* 编码基础协议请求
*/
#[inline(always)]
pub fn encode(compress_level: u8, compare: bool, version: u8, bin: &[u8]) -> Result<Vec<u8>> {
    //编码头
    let mut head = match compare {
        false => {
            vec![
                (compress_level << 6) | (version & 0x1f),
            ]
        },
        true => {
            vec![
                (compress_level << 6) | 0x20 | (version & 0x1f),
            ]
        },
    };

    //编码体
    match compress_level {
        0 => head.put(bin),
        _ => {
            let mut buf = Vec::new();
            if let Err(e) = compress(&bin[..], &mut buf, CompressLevel::Low) {
                return Err(Error::new(ErrorKind::Other, format!("compress failed by rpc decode, reason: {:?}", e)));
            }
            head.put(bin)
        },
    }

    Ok(head)
}

/*
* 基础应用协议连接
*/
pub struct BaseConnect {
    connect:            Arc<dyn MqttConnect>,   //请求的Mqtt连接
    compress_level:     Arc<AtomicU8>,          //压缩级别
    compare:            Arc<AtomicU8>,          //是否比较差异
    version:            Arc<AtomicU8>,          //版本号
    context:            SocketContext,          //上下文
}

unsafe impl Send for BaseConnect {}
unsafe impl Sync for BaseConnect {}

impl BaseConnect {
    //构建基础协议连接
    pub fn new(connect: Arc<dyn MqttConnect>) -> Self {
        BaseConnect {
            connect,
            compress_level: Arc::new(AtomicU8::new(0)),
            compare: Arc::new(AtomicU8::new(0)),
            version: Arc::new(AtomicU8::new(0)),
            context: SocketContext::empty(),
        }
    }

    //获取Mqtt连接
    pub fn get_connect(&self) -> &Arc<dyn MqttConnect> {
        &self.connect
    }

    //获取当前压缩级别
    pub fn get_compress_level(&self) -> u8 {
        self.compress_level.load(Ordering::Relaxed)
    }

    //是否需要比较交差异
    pub fn is_compare(&self) -> bool {
        self.compare.load(Ordering::Relaxed) > 0
    }

    //获取当前版本号
    pub fn get_version(&self) -> u8 {
        self.version.load(Ordering::Relaxed)
    }

    //获取基础协议连接上下文的只读引用
    pub fn get_context(&self) -> &SocketContext {
        &self.context
    }

    //获取基础协议连接上下文的可写引用
    pub fn get_context_mut(&mut self) -> &mut SocketContext {
        &mut self.context
    }

    //获取基础协议连接的句柄
    pub fn get_handle(&self) -> BaseConnectHandle {
        BaseConnectHandle {
            connect: self.connect.clone(),
            compress_level: self.compress_level.clone(),
            compare: self.compare.clone(),
            version: self.version.clone(),
        }
    }
}

/*
* 基础应用协议连接句柄
*/
pub struct BaseConnectHandle {
    connect:            Arc<dyn MqttConnect>,   //请求的Mqtt连接
    compress_level:     Arc<AtomicU8>,          //压缩级别
    compare:            Arc<AtomicU8>,          //是否比较差异
    version:            Arc<AtomicU8>,          //版本号
}

impl BaseConnectHandle {
    //获取Mqtt连接
    pub fn get_connect(&self) -> &Arc<dyn MqttConnect> {
        &self.connect
    }

    //获取当前压缩级别
    pub fn get_compress_level(&self) -> u8 {
        self.compress_level.load(Ordering::Relaxed)
    }

    //是否需要比较交差异
    pub fn is_compare(&self) -> bool {
        self.compare.load(Ordering::Relaxed) > 0
    }

    //获取当前版本号
    pub fn get_version(&self) -> u8 {
        self.version.load(Ordering::Relaxed)
    }

    //发送指定主题的数据
    pub fn send(&self, topic: String, data: Vec<u8>) {
        if let Ok(bin) = handle_encode(self, &data[..]) {
            self.connect.send(&topic, Arc::new(bin));
        }
    }

    //回应指定请求
    pub fn reply(&self, data: Vec<u8>) {
        self.send(MQTT_RESPONSE_SYS_TOPIC.clone(), data);
    }

    //关闭当前连接
    pub fn close(&self, reason: Option<String>) {
        if let Some(e) = reason {
            self.connect.close(Err(Error::new(ErrorKind::Other, e)));
        } else {
            self.connect.close(Ok(()));
        }
    }
}

//编码基础协议请求
#[inline(always)]
fn handle_encode(connect: &BaseConnectHandle, bin: &[u8]) -> Result<Vec<u8>> {
    let compress_level = connect.get_compress_level();
    let version = connect.get_version();

    //编码头
    let mut head = match connect.is_compare() {
        false => {
            vec![
                (compress_level << 6) | (version & 0x1f),
            ]
        },
        true => {
            vec![
                (compress_level << 6) | 0x20 | (version & 0x1f),
            ]
        },
    };

    //编码体
    match compress_level {
        0 => head.put(bin),
        _ => {
            let mut buf = Vec::new();
            if let Err(e) = compress(&bin[..], &mut buf, CompressLevel::Low) {
                return Err(Error::new(ErrorKind::Other, format!("compress failed by rpc decode, reason: {:?}", e)));
            }
            head.put(bin)
        },
    }

    Ok(head)
}