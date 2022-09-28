use std::mem;
use std::sync::Arc;
use std::net::Shutdown;
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::io::{ErrorKind, Result, Error, Write};

use bytes::Buf;

use tcp::{Socket, SocketHandle};

use crate::utils::{ChildProtocol, WsFrameType};

///
/// 结束帧标记
///
const FIN_FLAG: u8 = 1;

///
/// RSV默认标记
///
const DEFAULT_RSV_FLAG: u8 = 0;

///
/// 操作码
///
pub const FOLLOW_UP_OPCODE: u8 = 0x0;   //后续帧
pub const TEXT_OPCODE: u8 = 0x1;        //文本帧
pub const BINARY_OPCODE: u8 = 0x2;      //二进制帧
pub const CLOSE_OPCODE: u8 = 0x8;       //关闭帧
pub const PING_OPCODE: u8 = 0x9;        //ping帧
pub const PONG_OPCODE: u8 = 0xa;        //pong帧

///
/// 掩码默认标记
///
const DEFAULT_MASK_FLAG: u8 = 1;

///
/// Websocket掩码密钥
///
#[derive(Debug, Clone)]
enum WsMaskKey {
    Empty,              //没有掩码密钥
    Part(Vec<u8>),      //部分掩码密钥，用于解码上行Websocket帧头
    Complete(Vec<u8>),  //完整掩码密钥
}

///
/// Websocket负载长度
///
#[derive(Debug, Clone)]
pub enum WsPayloadLen {
    Part(Vec<u8>),  //部分长度数据
    Complete(u64),  //完整长度
}

///
/// Websocket帧头
///
#[derive(Debug, Clone)]
pub struct WsHead {
    fin:        u8,                 //是否完成
    rsv1:       u8,                 //扩展标记1，一般用于RFC7692，即每消息压缩扩展协议
    rsv2:       u8,                 //扩展标记2
    rsv3:       u8,                 //扩展标记3
    r#type:     u8,                 //类型，0x0后续、0x1文本、0x2二进制、0x8关闭、0x9 ping、0xa pong
    len:        WsPayloadLen,       //负载长度
    key:        WsMaskKey,          //掩码密钥
}

unsafe impl Send for WsHead {}

impl Default for WsHead {
    // 默认创建负载为0的关闭帧的头
    fn default() -> Self {
        WsHead {
            fin: FIN_FLAG,
            rsv1: 0,
            rsv2: 0,
            rsv3: 0,
            r#type: CLOSE_OPCODE,
            len: WsPayloadLen::Complete(0),
            key: WsMaskKey::Empty,
        }
    }
}

impl<'a> From<&'a [u8]> for WsHead {
    // 将二进制数据反序列化为Websocket帧头
    fn from(bin: &'a [u8]) -> Self {
        match bin.len() {
            WsHead::READ_HEAD_LEN => {
                //反序列化头数据长度为6的二进制数据
                let len;
                let key;
                match bin[1] & 0x7f {
                    127 => {
                        let mut vec = Vec::with_capacity(8);
                        vec.push(bin[2]);
                        vec.push(bin[3]);
                        vec.push(bin[4]);
                        vec.push(bin[5]);

                        len = WsPayloadLen::Part(vec);
                        key = WsMaskKey::Empty;
                    },
                    126 => {
                        len = WsPayloadLen::Complete((((bin[2] as u64) << 8) & 0xffff) | ((bin[3] as u64) & 0xff));
                        key = WsMaskKey::Part(vec![bin[4], bin[5]]);
                    },
                    n => {
                        len = WsPayloadLen::Complete(n as u64);
                        key = WsMaskKey::Complete(vec![bin[2], bin[3], bin[4], bin[5]]);
                    },
                }

                WsHead {
                    fin: get_bit(bin[0], 7),
                    rsv1: get_bit(bin[0], 6),
                    rsv2: get_bit(bin[0], 5),
                    rsv3: get_bit(bin[0], 4),
                    r#type: bin[0] & 0xf,
                    len,
                    key,
                }
            },
            len => {
                //反序列化其它头数据长度的头数据
                match len {
                    8 => {
                        WsHead {
                            fin: get_bit(bin[0], 7),
                            rsv1: get_bit(bin[0], 6),
                            rsv2: get_bit(bin[0], 5),
                            rsv3: get_bit(bin[0], 4),
                            r#type: bin[0] & 0xf,
                            len: WsPayloadLen::Complete((((bin[2] as u64) << 8) & 0xffff) | ((bin[3] as u64) & 0xff)),
                            key: WsMaskKey::Complete(vec![bin[4], bin[5], bin[6], bin[7]]),
                        }
                    },
                    14 => {
                        let real_len = (((bin[2] as u64) << 56) & 0xffffffffffffffff)
                            | (((bin[3] as u64) << 48) & 0xffffffffffffff)
                            | (((bin[4] as u64) << 40) & 0xffffffffffff)
                            | (((bin[5] as u64) << 32) & 0xffffffffff)
                            | (((bin[6] as u64) << 24) & 0xffffffff)
                            | (((bin[7] as u64) << 16) & 0xffffff)
                            | (((bin[8] as u64) << 8) & 0xffff)
                            | ((bin[9] as u64) & 0xff);

                        WsHead {
                            fin: get_bit(bin[0], 7),
                            rsv1: get_bit(bin[0], 6),
                            rsv2: get_bit(bin[0], 5),
                            rsv3: get_bit(bin[0], 4),
                            r#type: bin[0] & 0xf,
                            len: WsPayloadLen::Complete(real_len),
                            key: WsMaskKey::Complete(vec![bin[10], bin[11], bin[12], bin[13]]),
                        }
                    },
                    _ => {
                        //无效的二进制数据长度
                        panic!("from binary to head failed, invalid head data len, len: {:?}", len);
                    }
                }
            },
        }
    }
}

impl From<WsHead> for Vec<u8> {
    // 将Websocket帧头序列化为二进制数据
    fn from(head: WsHead) -> Self {
        if let WsPayloadLen::Complete(len) = head.len {
            let mut vec = Vec::with_capacity(len as usize + 2);

            //序列化首字节
            let mut b0 = set_bit(0, 7, head.fin);
            b0 = set_bit(b0, 6, head.rsv1);
            b0 = set_bit(b0, 5, head.rsv2);
            b0 = set_bit(b0, 4, head.rsv3) | (head.r#type & 0xf);
            vec.push(b0);

            //序列化掩码标记
            let mut mask_key = None;
            let mut b1 = 0u8;
            match head.key {
                WsMaskKey::Empty => {
                    b1 = set_bit(b1, 7, 0);
                },
                WsMaskKey::Complete(key) => {
                    b1 = set_bit(b1, 7, 1);
                    mask_key = Some(key);
                },
                WsMaskKey::Part(_) => {
                    //无效的掩码密钥
                    panic!("from head to binary failed, invalid mask key");
                }
            }

            //序列化负载长度
            if len > 0xffff {
                //需要8个字节负载长度
                b1 = b1 | 0x7f;
                vec.push(b1);

                vec.push(((len >> 56) & 0xff) as u8);
                vec.push(((len >> 48) & 0xff) as u8);
                vec.push(((len >> 40) & 0xff) as u8);
                vec.push(((len >> 32) & 0xff) as u8);
                vec.push(((len >> 24) & 0xff) as u8);
                vec.push(((len >> 16) & 0xff) as u8);
                vec.push(((len >> 8) & 0xff) as u8);
                vec.push((len & 0xff) as u8);
            } else if len >= 0x7e {
                //需要2个字节负载长度
                b1 = b1 | 0x7e;
                vec.push(b1);

                vec.push(((len >> 8) & 0xff) as u8);
                vec.push((len & 0xff) as u8);
            } else {
                //需要1个字节负载长度
                b1 = b1 | (len as u8);
                vec.push(b1);
            }

            //序列化掩码密钥
            if let Some(elemnts) = mask_key {
                for elemnt in elemnts {
                    vec.push(elemnt);
                }
            }

            vec
        } else {
            //无效的二进制数据长度
            panic!("from head to binary failed, invalid head data len");
        }
    }
}

impl WsHead {
    /// 初始读的头大小
    pub const READ_HEAD_LEN: usize = 6;

    /// 判断头是否完整
    pub fn is_complete(&self) -> bool {
        if let WsPayloadLen::Complete(_) = self.len {
            if let WsMaskKey::Complete(_) = self.key {
                return true;
            }
        }

        false
    }

    /// 还需要后续多少个字节才完整
    pub fn need_size(&self) -> usize {
        if let WsPayloadLen::Part(_) = self.len {
            8
        } else {
            if let WsMaskKey::Part(_) = self.key {
                2
            } else {
                0
            }
        }
    }

    /// 将剩余二进制数据序列化Websocket帧头剩余部分
    pub fn from_last(&mut self, last: &[u8]) -> GenResult<(), String> {
        match last.len() {
            2 => {
                let mut part = Vec::with_capacity(4);
                if let WsMaskKey::Part(p) = &self.key {
                    part.push(p[0]);
                    part.push(p[1]);
                    part.push(last[0]);
                    part.push(last[1]);
                } else {
                    return Err(format!("invalid mask key part, key: {:?}", self.key));
                }
                self.key = WsMaskKey::Complete(part);
            },
            8 => {
                let real_len;
                if let WsPayloadLen::Part(part) = &self.len {
                    real_len = (((part[0] as u64) << 56) & 0xffffffffffffffff)
                        | (((part[1] as u64) << 48) & 0xffffffffffffff)
                        | (((part[2] as u64) << 40) & 0xffffffffffff)
                        | (((part[3] as u64) << 32) & 0xffffffffff)
                        | (((last[0] as u64) << 24) & 0xffffffff)
                        | (((last[1] as u64) << 16) & 0xffffff)
                        | (((last[2] as u64) << 8) & 0xffff)
                        | ((last[3] as u64) & 0xff);
                } else {
                    return Err(format!("invalid payload len part, key: {:?}", self.len));
                }
                self.len = WsPayloadLen::Complete(real_len);

                let mut part = Vec::with_capacity(4);
                part.push(last[4]);
                part.push(last[5]);
                part.push(last[6]);
                part.push(last[7]);
                self.key = WsMaskKey::Complete(part);
            },
            len => {
                return Err(format!("invalid last len, len: {:?}", len));
            },
        }

        Ok(())
    }

    /// 设置结束帧标记
    #[inline(always)]
    pub fn set_finish(&mut self, fin: bool) {
        if fin {
            self.fin = 1;
        } else {
            self.fin = 0;
        }
    }

    /// 是否有rsv1
    #[inline(always)]
    pub fn is_rsv1(&self) -> bool {
        self.rsv1 != DEFAULT_RSV_FLAG
    }

    /// 设置是否打开rsv1
    #[inline(always)]
    pub fn set_rsv1(&mut self, rsv1: bool) {
        if rsv1 {
            self.rsv1 = 1;
        } else {
            self.rsv1 = 0;
        }
    }

    /// 是否有rsv2
    #[inline(always)]
    pub fn is_rsv2(&self) -> bool {
        self.rsv2 != DEFAULT_RSV_FLAG
    }

    /// 设置是否打开rsv2
    #[inline(always)]
    pub fn set_rsv2(&mut self, rsv2: bool) {
        if rsv2 {
            self.rsv2 = 1;
        } else {
            self.rsv2 = 0;
        }
    }

    /// 是否有rsv3
    #[inline(always)]
    pub fn is_rsv3(&self) -> bool {
        self.rsv3 != DEFAULT_RSV_FLAG
    }

    /// 设置是否打开rsv3
    #[inline(always)]
    pub fn set_rsv3(&mut self, rsv3: bool) {
        if rsv3 {
            self.rsv3 = 1;
        } else {
            self.rsv3 = 0;
        }
    }

    /// 是否是控制帧
    #[inline(always)]
    pub fn is_control(&self) -> bool {
        match self.r#type {
            CLOSE_OPCODE | PING_OPCODE | PONG_OPCODE => true,
            _ => false,
        }
    }

    /// 是否是关闭帧
    #[inline(always)]
    pub fn is_close(&self) -> bool {
        self.r#type == CLOSE_OPCODE
    }

    /// 设置为关闭帧
    #[inline(always)]
    pub fn set_close(&mut self) {
        self.r#type == CLOSE_OPCODE;
    }

    /// 是否是Ping帧
    #[inline(always)]
    pub fn is_ping(&self) -> bool {
        self.r#type == PING_OPCODE
    }

    /// 设置为Ping帧
    #[inline(always)]
    pub fn set_ping(&mut self) {
        self.r#type = PING_OPCODE;
    }

    /// 是否是Pong帧
    #[inline(always)]
    pub fn is_pong(&self) -> bool {
        self.r#type == PONG_OPCODE
    }

    /// 设置为Pong帧
    #[inline(always)]
    pub fn set_pong(&mut self) {
        self.r#type = PONG_OPCODE;
    }

    /// 是否是数据帧
    #[inline(always)]
    pub fn is_data(&self) -> bool {
        !self.is_control()
    }

    /// 是否是后续帧
    #[inline(always)]
    pub fn is_follow(&self) -> bool {
        self.r#type == FOLLOW_UP_OPCODE
    }

    /// 设置为后续帧
    #[inline(always)]
    pub fn set_follow(&mut self) {
        self.r#type = FOLLOW_UP_OPCODE;
    }

    /// 是否是文本帧
    #[inline(always)]
    pub fn is_text(&self) -> bool {
        self.r#type == TEXT_OPCODE
    }

    /// 设置为文本帧
    #[inline(always)]
    pub fn set_text(&mut self) {
        self.r#type = TEXT_OPCODE;
    }

    /// 是否是二进制帧
    #[inline(always)]
    pub fn is_binary(&self) -> bool {
        self.r#type == BINARY_OPCODE | CLOSE_OPCODE | PING_OPCODE | PONG_OPCODE
    }

    /// 设置为二进制帧
    #[inline(always)]
    pub fn set_binary(&mut self) {
        self.r#type = BINARY_OPCODE;
    }

    /// 是否是单帧数据
    #[inline(always)]
    pub fn is_single(&self) -> bool {
        (self.fin == FIN_FLAG) && (self.r#type == TEXT_OPCODE || self.r#type == BINARY_OPCODE)
    }

    /// 是否是多帧数据的首帧
    #[inline(always)]
    pub fn is_first(&self) -> bool {
        (self.fin != FIN_FLAG) && (self.r#type == TEXT_OPCODE || self.r#type == BINARY_OPCODE)
    }

    /// 是否是后续数据帧，且不是结束帧
    #[inline(always)]
    pub fn is_next(&self) -> bool {
        (self.fin != FIN_FLAG) && (self.r#type == FOLLOW_UP_OPCODE)
    }

    /// 是否是多帧数据的结束帧
    #[inline(always)]
    pub fn is_finish(&self) -> bool {
        (self.fin == FIN_FLAG) && (self.r#type == FOLLOW_UP_OPCODE)
    }

    /// 获取帧类型
    pub fn get_type(&self) -> u8 {
        self.r#type
    }

    /// 获取负载长度
    pub fn len(&self) -> u64 {
        if let WsPayloadLen::Complete(len) = self.len {
            len
        } else {
            0
        }
    }

    /// 设置负载长度
    pub fn set_len(&mut self, len: usize) {
        self.len = WsPayloadLen::Complete(len as u64);
    }

    /// 获取掩码密钥
    #[inline(always)]
    pub fn get_key(&self) -> Option<&[u8]> {
        if let WsMaskKey::Complete(key) = &self.key {
            Some(key)
        } else {
            None
        }
    }

    /// 设置掩码密钥
    #[inline(always)]
    pub fn set_key(&mut self, mask_key: Option<Vec<u8>>) {
        if let Some(key) = mask_key {
            self.key = WsMaskKey::Complete(key);
        } else {
            self.key = WsMaskKey::Empty;
        }
    }
}

///
/// Websocket负载
///
pub enum WsPayload {
    Empty,                  //空负载
    Raw(Vec<u8>),           //祼负载，一般用于接收数据帧、接收发送控制帧或发送数据帧
}

unsafe impl Send for WsPayload {}

impl Default for WsPayload {
    fn default() -> Self {
        WsPayload::Empty
    }
}

impl WsPayload {
    /// 是否是空负载
    pub fn is_empty(&self) -> bool {
        if let WsPayload::Empty = self {
            return true;
        }

        false
    }

    /// 是否是祼负载
    pub fn is_raw(&self) -> bool {
        if let WsPayload::Raw(_) = self {
            return true;
        }

        false
    }

    /// 取出负载
    pub fn take(&mut self) -> Self {
        mem::take(self)
    }
}

///
/// Websocket帧
///
pub struct WsFrame<S: Socket> {
    head:       WsHead,         //头
    payload:    WsPayload,      //负载
    marker:     PhantomData<S>,
}

unsafe impl<S: Socket> Send for WsFrame<S> {}

impl<S: Socket> Default for WsFrame<S> {
    fn default() -> Self {
        WsFrame {
            head: WsHead::default(),
            payload: WsPayload::Empty,
            marker: PhantomData,
        }
    }
}

impl<S: Socket> From<WsFrame<S>> for Vec<u8> {
    fn from(frame: WsFrame<S>) -> Self {
        match frame.payload {
            WsPayload::Raw(payload) => {
                let bin = Vec::from(frame.head);
                (vec![bin, payload]).concat()
            },
            _ => {
                //当前帧无负载
                Vec::from(frame.head)
            },
        }
    }
}

/*
* Websocket帧同步方法
*/
impl<S: Socket> WsFrame<S> {
    /// 构建控制帧
    pub fn control_with_payload(frame_type: WsFrameType,
                                payload: Option<Vec<u8>>) -> Self {
        let head;
        let real_payload;
        if let Some(p) = payload {
            //有负载
            head = WsHead {
                fin: FIN_FLAG,
                rsv1: 0,
                rsv2: 0,
                rsv3: 0,
                r#type: frame_type.into(),
                len: WsPayloadLen::Complete(p.len() as u64),
                key: WsMaskKey::Empty,
            };
            real_payload = WsPayload::Raw(p);
        } else {
            //无负载
            head = WsHead {
                fin: FIN_FLAG,
                rsv1: 0,
                rsv2: 0,
                rsv3: 0,
                r#type: frame_type.into(),
                len: WsPayloadLen::Complete(0),
                key: WsMaskKey::Empty,
            };
            real_payload = WsPayload::Empty;
        }

        WsFrame {
            head,
            payload: real_payload,
            marker: PhantomData,
        }
    }

    /// 构建单帧数据帧
    pub fn single_with_window_bits_and_payload(frame_type: WsFrameType,
                                               window_bits: u8,
                                               mut payload: Vec<u8>) -> Self {
        if window_bits == 0 {
            let head = WsHead {
                fin: FIN_FLAG,
                rsv1: 0,
                rsv2: 0,
                rsv3: 0,
                r#type: frame_type.into(),
                len: WsPayloadLen::Complete(payload.len() as u64),
                key: WsMaskKey::Empty,
            };

            WsFrame {
                head,
                payload: WsPayload::Raw(payload),
                marker: PhantomData,
            }
        } else {
            let head = WsHead {
                fin: FIN_FLAG,
                rsv1: 1,
                rsv2: 0,
                rsv3: 0,
                r#type: frame_type.into(),
                len: WsPayloadLen::Complete(payload.len() as u64),
                key: WsMaskKey::Empty,
            };
            //TODO 需要实现压缩
            unimplemented!()
        }
    }

    /// 获取头只读引用
    pub fn get_head(&self) -> &WsHead {
        &self.head
    }

    /// 获取头可写引用
    pub fn get_head_mut(&mut self) -> &mut WsHead {
        &mut self.head
    }

    /// 设置头
    pub fn set_head(&mut self, head: WsHead) {
        self.head = head;
    }

    /// 获取负载
    pub fn payload(&mut self) -> WsPayload {
        self.payload.take()
    }

    /// 将负载反序列化为当前帧负载
    pub fn from_payload(&mut self, bin: &[u8]) {
        let mut payload = vec![];
        match self.payload.take() {
            WsPayload::Empty => {
                //当前帧没有负载，则创建指定负载容量的空缓冲，并设置负载
                let mut buf = Vec::with_capacity(self.head.len() as usize);
                buf.extend_from_slice(bin);
                payload = buf;
            }
            WsPayload::Raw(mut p) => {
                //当前帧已经有祼负载，则清空当前负载
                p.clear();
                payload = p;
            },
        }

        mask(&mut payload[..], self.head.get_key());
        self.payload = WsPayload::Raw(payload);
    }

    /// 将帧序列化为写缓冲
    pub fn into_write_buf(self) -> Option<Vec<u8>> {
        match self.payload {
            WsPayload::Raw(buf) => {
                Some(vec![Vec::from(self.head), buf].concat())
            },
            _ => None,
        }
    }
}

/*
* Websocket帧异步方法
*/
impl<S: Socket> WsFrame<S> {
    /// 异步读Websocket帧头
    pub async fn read_head(handle: &SocketHandle<S>,
                           window_bits: u8,
                           frame: &mut WsFrame<S>) {
        let mut is_first = true; //是否首次接收
        let mut ready_len = WsHead::READ_HEAD_LEN; //初始化就绪字节数
        loop {
            if let Some(buf) = unsafe { (&mut *handle.get_read_buffer().get()) } {
                if buf.remaining() < ready_len {
                    //当前缓冲区没有帧头数据，则异步准备读取，并继续尝试读帧头数据
                    match handle.read_ready(ready_len) {
                        Err(len) => {
                            ready_len = len;
                        },
                        Ok(value) => {
                            ready_len = value.await;
                        }
                    }

                    if ready_len == 0 {
                        //当前连接已关闭，则立即退出
                        return;
                    }
                    continue;
                }

                if is_first {
                    let head = WsHead::from(buf.copy_to_bytes(ready_len).as_ref());
                    match head.need_size() {
                        0 => {
                            //头已读完成
                            frame.set_head(head);
                            break;
                        },
                        r => {
                            //还需要接收指定字节数的头数据
                            is_first = false;
                            ready_len = r;
                            frame.set_head(head);
                            continue;
                        }
                    }
                } else {
                    if let Err(e) = frame.get_head_mut().from_last(buf.copy_to_bytes(ready_len).as_ref()) {
                        handle.close(Err(Error::new(ErrorKind::Other,
                                                    format!("Websocket read frame head failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                            handle.get_token(),
                                                            handle.get_remote(),
                                                            handle.get_local(),
                                                            e))));
                        return;
                    }

                    if let None = frame.get_head().get_key() {
                        //客户端上行数据没有掩码，则立即断开连接
                        handle.close(Err(Error::new(ErrorKind::Other,
                                                    format!("Websocket read frame head failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid client mask",
                                                            handle.get_token(),
                                                            handle.get_remote(),
                                                            handle.get_local()))));
                        return;
                    }

                    //头已读完成
                    break;
                }
            } else {
                //读缓冲区已关闭，则立即断开连接
                handle.close(Err(Error::new(ErrorKind::BrokenPipe,
                                            format!("Websocket read frame head failed, token: {:?}, remote: {:?}, local: {:?}, reason: read buffer stream closed",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local()))));
                return;
            }
        }

        WsFrame::<S>::read_payload(handle, frame).await;
    }

    /// 异步读Websocket帧负载
    async fn read_payload(handle: &SocketHandle<S>, frame: &mut WsFrame<S>) {
        let mut ready_len = frame.get_head().len() as usize;
        if ready_len > 0 {
            //有负载，则继续异步读负载，并填充Websocket帧
            loop {
                if let Some(buf) = unsafe { (&mut *handle.get_read_buffer().get()) } {
                    if buf.remaining() < ready_len {
                        //当前缓冲区没有负载数据，则异步准备读取，并继续尝试读负载数据
                        match handle.read_ready(ready_len) {
                            Err(len) => {
                                ready_len = len;
                            },
                            Ok(value) => {
                                ready_len = value.await;
                            }
                        }

                        if ready_len == 0 {
                            //当前连接已关闭，则立即退出
                            return;
                        }
                        continue;
                    }

                    //负载读完成
                    frame.from_payload(buf.copy_to_bytes(ready_len).as_ref());

                    return;
                } else {
                    //读缓冲区已关闭，则立即关闭当前Websocket连接
                    handle.close(Err(Error::new(ErrorKind::BrokenPipe,
                                                format!("Websocket read frame payload failed, token: {:?}, remote: {:?}, local: {:?}, reason: read buffer stream closed",
                                                        handle.get_token(),
                                                        handle.get_remote(),
                                                        handle.get_local()))));
                    return;
                }
            }
        }
    }
}

// 获取指定字节的指定位的值
#[inline(always)]
fn get_bit(byte: u8, nth: u8) -> u8 {
    if byte & (1 << nth) == 0 {
        0
    } else {
        1
    }
}

// 设置指定字节的指定位的值
#[inline(always)]
fn set_bit(byte: u8, nth: u8, val: u8) -> u8 {
    if val == 0 {
        byte & !(1 << nth)
    } else {
        byte | (1 << nth)
    }
}

// 通过掩码密钥进行编解码
#[inline(always)]
fn mask(bin: &mut [u8], mask_key: Option<&[u8]>) {
    if let Some(key) = mask_key {
        let a = key[0];
        let b = key[1];
        let c = key[2];
        let d = key[3];

        let mut i = 0;
        let mut len = bin.len();
        while(len > 0) {
            if len >= 32 {
                bin[i] = bin[i] ^ a;
                bin[i + 1] = bin[i + 1] ^ b;
                bin[i + 2] = bin[i + 2] ^ c;
                bin[i + 3] = bin[i + 3] ^ d;
                bin[i + 4] = bin[i + 4] ^ a;
                bin[i + 5] = bin[i + 5] ^ b;
                bin[i + 6] = bin[i + 6] ^ c;
                bin[i + 7] = bin[i + 7] ^ d;
                bin[i + 8] = bin[i + 8] ^ a;
                bin[i + 9] = bin[i + 9] ^ b;
                bin[i + 10] = bin[i + 10] ^ c;
                bin[i + 11] = bin[i + 11] ^ d;
                bin[i + 12] = bin[i + 12] ^ a;
                bin[i + 13] = bin[i + 13] ^ b;
                bin[i + 14] = bin[i + 14] ^ c;
                bin[i + 15] = bin[i + 15] ^ d;
                bin[i + 16] = bin[i + 16] ^ a;
                bin[i + 17] = bin[i + 17] ^ b;
                bin[i + 18] = bin[i + 18] ^ c;
                bin[i + 19] = bin[i + 19] ^ d;
                bin[i + 20] = bin[i + 20] ^ a;
                bin[i + 21] = bin[i + 21] ^ b;
                bin[i + 22] = bin[i + 22] ^ c;
                bin[i + 23] = bin[i + 23] ^ d;
                bin[i + 24] = bin[i + 24] ^ a;
                bin[i + 25] = bin[i + 25] ^ b;
                bin[i + 26] = bin[i + 26] ^ c;
                bin[i + 27] = bin[i + 27] ^ d;
                bin[i + 28] = bin[i + 28] ^ a;
                bin[i + 29] = bin[i + 29] ^ b;
                bin[i + 30] = bin[i + 30] ^ c;
                bin[i + 31] = bin[i + 31] ^ d;
                i += 32;
                len -= 32;
            } else if len >= 16 {
                bin[i] = bin[i] ^ a;
                bin[i + 1] = bin[i + 1] ^ b;
                bin[i + 2] = bin[i + 2] ^ c;
                bin[i + 3] = bin[i + 3] ^ d;
                bin[i + 4] = bin[i + 4] ^ a;
                bin[i + 5] = bin[i + 5] ^ b;
                bin[i + 6] = bin[i + 6] ^ c;
                bin[i + 7] = bin[i + 7] ^ d;
                bin[i + 8] = bin[i + 8] ^ a;
                bin[i + 9] = bin[i + 9] ^ b;
                bin[i + 10] = bin[i + 10] ^ c;
                bin[i + 11] = bin[i + 11] ^ d;
                bin[i + 12] = bin[i + 12] ^ a;
                bin[i + 13] = bin[i + 13] ^ b;
                bin[i + 14] = bin[i + 14] ^ c;
                bin[i + 15] = bin[i + 15] ^ d;
                i += 16;
                len -= 16;
            } else if len >= 8 {
                bin[i] = bin[i] ^ a;
                bin[i + 1] = bin[i + 1] ^ b;
                bin[i + 2] = bin[i + 2] ^ c;
                bin[i + 3] = bin[i + 3] ^ d;
                bin[i + 4] = bin[i + 4] ^ a;
                bin[i + 5] = bin[i + 5] ^ b;
                bin[i + 6] = bin[i + 6] ^ c;
                bin[i + 7] = bin[i + 7] ^ d;
                i += 8;
                len -= 8;
            } else if len >= 4 {
                bin[i] = bin[i] ^ a;
                bin[i + 1] = bin[i + 1] ^ b;
                bin[i + 2] = bin[i + 2] ^ c;
                bin[i + 3] = bin[i + 3] ^ d;
                i += 4;
                len -= 4;
            } else if len == 3 {
                bin[i] = bin[i] ^ a;
                bin[i + 1] = bin[i + 1] ^ b;
                bin[i + 2] = bin[i + 2] ^ c;
                i += 3;
                len -= 3;
            } else if len == 2 {
                bin[i] = bin[i] ^ a;
                bin[i + 1] = bin[i + 1] ^ b;
                i += 2;
                len -= 2;
            } else {
                bin[i] = bin[i] ^ key[i % 4];
                i += 1;
                len -= 1;
            }
        }
    }
}