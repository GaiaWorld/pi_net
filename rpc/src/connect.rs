use std::sync::Arc;
use std::net::SocketAddr;
use std::io::{Result, Error, ErrorKind};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicIsize, Ordering};

use bytes::BufMut;

use gray::GrayVersion;
use compress::{CompressLevel, compress, uncompress};

use mqtt::broker::MQTT_RESPONSE_SYS_TOPIC;
use mqtt::session::MqttConnect;
use base::connect::BaseConnectHandle;

/*
* 解码Rpc请求
*/
#[inline(always)]
pub fn decode(connect: &RpcConnect, bin: &[u8]) -> Result<(u32, Vec<u8>)> {
    if bin.len() < 6 {
        return Err(Error::new(ErrorKind::Other, "rpc decode failed, reason: invalid len"));
    }

    //解码头，并更新当前连接的头信息
    connect.timeout.store(bin[4], Ordering::Relaxed);

    let rid = (bin[3] as u32) << 24 & 0xffffffff | (bin[2] as u32) << 16 & 0xffffff | (bin[1] as u32) << 8 & 0xffff | (bin[0] & 0xff) as u32;
    Ok((rid, Vec::from(&bin[5..])))
}

/*
* 编码Rpc请求
*/
#[inline(always)]
pub fn encode(connect: &RpcConnect, rid: u32, bin: &[u8]) -> Result<Vec<u8>> {
    let timeout = connect.get_timeout();

    //编码头
    let mut head = vec![
        (rid & 0xff) as u8,
        ((rid >> 8) & 0xff) as u8,
        ((rid >> 16) & 0xff) as u8,
        ((rid >> 24) & 0xff) as u8,
        timeout
    ];

    //编码体
    head.put(bin);

    Ok(head)
}

/*
* Rpc连接
*/
pub struct RpcConnect {
    gray:               AtomicIsize,            //灰度，负数代表无灰度
    handle:             BaseConnectHandle,      //请求的基础协议连接句柄
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
        if let Some(uid) = self.handle.get_connect().get_uid() {
            return uid;
        }

        0
    }
}

impl RpcConnect {
    //构建Rpc连接
    pub fn new(handle: BaseConnectHandle) -> Self {
        RpcConnect {
            gray: AtomicIsize::new(-1),
            handle,
            timeout: AtomicU8::new(0),
        }
    }

    //获取超时时长
    pub fn get_timeout(&self) -> u8 {
        self.timeout.load(Ordering::Relaxed)
    }

    //获取连接的连接令牌
    pub fn get_token(&self) -> Option<usize> {
        self.handle.get_connect().get_token()
    }

    //获取连接的本地地址
    pub fn get_local_addr(&self) -> Option<SocketAddr> {
        self.handle.get_connect().get_local_addr()
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
        self.handle.get_connect().get_remote_addr()
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
    pub fn send(&self, topic: String, rid: u32, data: Vec<u8>) {
        if let Ok(bin) = encode(self, rid, &data[..]) {
            self.handle.send(topic, bin);
        }
    }

    //回应指定请求
    pub fn reply(&self, rid: u32, data: Vec<u8>) {
        self.send(MQTT_RESPONSE_SYS_TOPIC.clone(), rid, data);
    }

    //关闭当前连接
    pub fn close(&self, reason: Option<String>) {
        self.handle.close(reason);
    }
}
