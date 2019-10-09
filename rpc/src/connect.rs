use std::sync::Arc;
use std::net::SocketAddr;
use std::io::{Result, Error, ErrorKind};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicIsize, Ordering};

use bytes::BufMut;

use gray::GrayVersion;
use compress::{CompressLevel, compress, uncompress};

use mqtt::broker::MQTT_RESPONSE_SYS_TOPIC;
use mqtt::session::MqttConnect;

/*
* 解码Rpc请求
*/
#[inline(always)]
pub fn decode(connect: &RpcConnect, bin: &[u8]) -> Result<Vec<u8>> {
    if bin.len() < 7 {
        return Err(Error::new(ErrorKind::Other, "rpc decode failed, reason: invalid len"));
    }

    //解码头，并更新当前连接的头信息
    let compress_level = bin[0] >> 5;
    connect.compress_level.store(compress_level, Ordering::Relaxed);
    connect.version.store(bin[0] & 0x1f, Ordering::Relaxed);
    connect.rid.store((bin[4] as u32) << 24 & 0xffffffff | (bin[3] as u32) << 16 & 0xffffff | (bin[2] as u32) << 8 & 0xffff | (bin[1] & 0xff) as u32, Ordering::Relaxed);
    connect.timeout.store(bin[5], Ordering::Relaxed);

    match compress_level {
        0 => Ok(Vec::from(&bin[6..])),
        _ => {
            let mut buf = Vec::new();
            if let Err(e) = uncompress(&bin[6..], &mut buf) {
                //解压缩失败
                return Err(Error::new(ErrorKind::Other, format!("uncompress failed by rpc decode, reason: {:?}", e)));
            }

            Ok(buf)
        }
    }
}

/*
* 编码Rpc请求
*/
#[inline(always)]
pub fn encode(connect: &RpcConnect, bin: &[u8]) -> Result<Vec<u8>> {
    let compress_level = connect.get_compress_level();
    let version = connect.get_version();
    let rid = connect.get_rid();
    let timeout = connect.get_timeout();

    //编码头
    let mut head = vec![
        (compress_level << 5) | (version & 0x1f),
        ((rid >> 24) & 0xff) as u8,
        ((rid >> 16) & 0xff) as u8,
        ((rid >> 8) & 0xff) as u8,
        (rid & 0xff) as u8,
        timeout
    ];

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
* Rpc连接
*/
pub struct RpcConnect {
    gray:               AtomicIsize,            //灰度，负数代表无灰度
    connect:            Arc<dyn MqttConnect>,   //请求的Mqtt连接
    compress_level:     AtomicU8,               //压缩级别
    version:            AtomicU8,               //版本号
    rid:                AtomicU32,              //请求id
    timeout:            AtomicU8,               //超时时长，单位秒
}

unsafe impl Send for RpcConnect {}
unsafe impl Sync for RpcConnect {}

impl GrayVersion for RpcConnect {
    fn get_gray(&self) -> &Option<usize> {
        let gray = self.gray.load(Ordering::Relaxed);
        if gray < 0 {
            return &None;
        }

        &None //TODO 修改GrayVersion后再实现...
    }

    fn set_gray(&mut self, gray: Option<usize>) {
        if let Some(n) = gray {
            self.gray.store(n as isize, Ordering::SeqCst);
        } else {
            self.gray.store(-1, Ordering::SeqCst);
        }
    }

    fn get_id(&self) -> usize {
        if let Some(uid) = self.connect.get_uid() {
            return uid;
        }

        0
    }
}

impl RpcConnect {
    //构建Rpc连接
    pub fn new(connect: Arc<dyn MqttConnect>) -> Self {
        RpcConnect {
            gray: AtomicIsize::new(-1),
            connect,
            compress_level: AtomicU8::new(0),
            version: AtomicU8::new(0),
            rid: AtomicU32::new(0),
            timeout: AtomicU8::new(0),
        }
    }

    //获取当前压缩级别
    pub fn get_compress_level(&self) -> u8 {
        self.compress_level.load(Ordering::Relaxed)
    }

    //获取当前版本号
    pub fn get_version(&self) -> u8 {
        self.version.load(Ordering::Relaxed)
    }

    //获取当前请求id
    pub fn get_rid(&self) -> u32 {
        self.rid.load(Ordering::Relaxed)
    }

    //获取超时时长
    pub fn get_timeout(&self) -> u8 {
        self.timeout.load(Ordering::Relaxed)
    }

    //获取连接的连接令牌
    pub fn get_token(&self) -> Option<usize> {
        self.connect.get_token()
    }

    //获取连接的本地地址
    pub fn get_local_addr(&self) -> Option<SocketAddr> {
        self.connect.get_local_addr()
    }

    //获取连接的本地ip
    pub fn get_local_ip(&self) -> Option<String> {
        if let Some(addr) = self.get_local_addr() {
            return Some(addr.ip().to_string());
        }

        None
    }

    //获取连接的本地端口
    pub fn get_local_port(&self) -> Option<u16> {
        if let Some(addr) = self.get_local_addr() {
            return Some(addr.port());
        }

        None
    }

    //获取连接的对端地址
    pub fn get_remote_addr(&self) -> Option<SocketAddr> {
        self.connect.get_remote_addr()
    }

    //获取连接的对端ip
    pub fn get_remote_ip(&self) -> Option<String> {
        if let Some(addr) = self.get_remote_addr() {
            return Some(addr.ip().to_string());
        }

        None
    }

    //获取连接的对端端口
    pub fn get_remote_port(&self) -> Option<u16> {
        if let Some(addr) = self.get_remote_addr() {
            return Some(addr.port());
        }

        None
    }

    //发送指定主题的数据
    pub fn send(&self, topic: String, data: Vec<u8>) {
        if let Ok(bin) = encode(self, &data[..]) {
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
