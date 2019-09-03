


/*
* 结束帧标记
*/
const FIN_FLAG: u8 = 1;

/*
* RSV默认标记
*/
const DEFAULT_RSV_FLAG: u8 = 0;

/*
* 操作码
*/
const FOLLOW_UP_OPCODE: u8 = 0x0;   //后续帧
const TEXT_OPCODE: u8 = 0x1;        //文本帧
const BINARY_OPCODE: u8 = 0x2;      //二进制帧
const CLOSE_OPCODE: u8 = 0x8;       //关闭帧
const PING_OPCODE: u8 = 0x9;        //ping帧
const PONG_OPCODE: u8 = 0xa;        //pong帧

/*
* 掩码默认标记
*/
const DEFAULT_MASK_FLAG: u8 = 1;

/*
* Websocket掩码密钥
*/
#[derive(Debug, Clone)]
enum WsMaskKey {
    Empty,              //没有掩码密钥
    Part(Vec<u8>),      //部分掩码密钥，用于解码上行Websocket帧头
    Complete(Vec<u8>),  //完整掩码密钥
}

/*
* Websocket负载长度
*/
#[derive(Debug, Clone)]
pub enum WsPayloadLen {
    Part(Vec<u8>),  //部分长度数据
    Complete(u64),  //完整长度
}

/*
* Websocket帧头
*/
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

impl Default for WsHead {
    //默认创建负载为0的关闭帧的头
    fn default() -> Self {
        WsHead {
            fin: 1,
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
    //将二进制数据反序列化为Websocket帧头
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
    //将Websocket帧头序列化为二进制数据
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
            } else if len > 0x7e {
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
    //初始读的头大小
    pub const READ_HEAD_LEN: usize = 6;

    //判断头是否完整
    pub fn is_complete(&self) -> bool {
        if let WsPayloadLen::Complete(_) = self.len {
            if let WsMaskKey::Complete(_) = self.key {
                return true;
            }
        }

        false
    }

    //还需要后续多少个字节才完整
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

    //将剩余二进制数据序列化Websocket帧头剩余部分
    pub fn from_last(&mut self, last: &[u8]) -> Result<(), String> {
        match last.len() {
            2 => {
                if let WsMaskKey::Part(part) = &mut self.key {
                    part.push(last[0]);
                    part.push(last[1]);
                } else {
                    return Err(format!("invalid mask key part, key: {:?}", self.key));
                }
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

                if let WsMaskKey::Part(part) = &mut self.key {
                    part.push(last[4]);
                    part.push(last[5]);
                    part.push(last[6]);
                    part.push(last[7]);
                } else {
                    return Err(format!("invalid mask key part, key: {:?}", self.key));
                }
            },
            len => {
                return Err(format!("invalid last len, len: {:?}", len));
            },
        }

        Ok(())
    }

    //是否是结束帧
    #[inline(always)]
    pub fn is_finish(&self) -> bool {
        self.fin == FIN_FLAG
    }

    //设置是否为结束帧
    #[inline(always)]
    pub fn set_finish(&mut self, fin: bool) {
        if fin {
            self.fin = 1;
        } else {
            self.fin = 0;
        }
    }

    //是否有rsv1
    #[inline(always)]
    pub fn is_rsv1(&self) -> bool {
        self.rsv1 != DEFAULT_RSV_FLAG
    }

    //设置是否打开rsv1
    #[inline(always)]
    pub fn set_rsv1(&mut self, rsv1: bool) {
        if rsv1 {
            self.rsv1 = 1;
        } else {
            self.rsv1 = 0;
        }
    }

    //是否有rsv2
    #[inline(always)]
    pub fn is_rsv2(&self) -> bool {
        self.rsv2 != DEFAULT_RSV_FLAG
    }

    //设置是否打开rsv2
    #[inline(always)]
    pub fn set_rsv2(&mut self, rsv2: bool) {
        if rsv2 {
            self.rsv2 = 1;
        } else {
            self.rsv2 = 0;
        }
    }

    //是否有rsv3
    #[inline(always)]
    pub fn is_rsv3(&self) -> bool {
        self.rsv3 != DEFAULT_RSV_FLAG
    }

    //设置是否打开rsv3
    #[inline(always)]
    pub fn set_rsv3(&mut self, rsv3: bool) {
        if rsv3 {
            self.rsv3 = 1;
        } else {
            self.rsv3 = 0;
        }
    }

    //是否是控制帧
    #[inline(always)]
    pub fn is_control(&self) -> bool {
        match self.r#type {
            CLOSE_OPCODE | PING_OPCODE | PONG_OPCODE => true,
            _ => false,
        }
    }

    //是否是关闭帧
    #[inline(always)]
    pub fn is_close(&self) -> bool {
        self.r#type == CLOSE_OPCODE
    }

    //设置为关闭帧
    #[inline(always)]
    pub fn set_close(&mut self) {
        self.r#type == CLOSE_OPCODE;
    }

    //是否是Ping帧
    #[inline(always)]
    pub fn is_ping(&self) -> bool {
        self.r#type == PING_OPCODE
    }

    //设置为Ping帧
    #[inline(always)]
    pub fn set_ping(&mut self) {
        self.r#type = PING_OPCODE;
    }

    //是否是Pong帧
    #[inline(always)]
    pub fn is_pong(&self) -> bool {
        self.r#type == PONG_OPCODE
    }

    //设置为Pong帧
    #[inline(always)]
    pub fn set_pong(&mut self) {
        self.r#type = PONG_OPCODE;
    }

    //是否是数据帧
    #[inline(always)]
    pub fn is_data(&self) -> bool {
        !self.is_control()
    }

    //是否是后续帧
    #[inline(always)]
    pub fn is_follow(&self) -> bool {
        self.r#type == FOLLOW_UP_OPCODE
    }

    //设置为后续帧
    #[inline(always)]
    pub fn set_follow(&mut self) {
        self.r#type = FOLLOW_UP_OPCODE;
    }

    //是否是文本帧
    #[inline(always)]
    pub fn is_text(&self) -> bool {
        self.r#type == TEXT_OPCODE
    }

    //设置为文本帧
    #[inline(always)]
    pub fn set_text(&mut self) {
        self.r#type = TEXT_OPCODE;
    }

    //是否是二进制帧
    #[inline(always)]
    pub fn is_binary(&self) -> bool {
        self.r#type == BINARY_OPCODE
    }

    //设置为二进制帧
    #[inline(always)]
    pub fn set_binary(&mut self) {
        self.r#type = BINARY_OPCODE;
    }

    //获取负载长度
    pub fn len(&self) -> u64 {
        if let WsPayloadLen::Complete(len) = self.len {
            len
        } else {
            0
        }
    }

    //设置负载长度
    pub fn set_len(&mut self, len: usize) {
        self.len = WsPayloadLen::Complete(len as u64);
    }

    //获取掩码密钥
    #[inline(always)]
    pub fn get_key(&self) -> Option<&[u8]> {
        if let WsMaskKey::Complete(key) = &self.key {
            Some(key)
        } else {
            None
        }
    }

    //设置掩码密钥
    #[inline(always)]
    pub fn set_key(&mut self, mask_key: Option<Vec<u8>>) {
        if let Some(key) = mask_key {
            self.key = WsMaskKey::Complete(key);
        } else {
            self.key = WsMaskKey::Empty;
        }
    }
}

/*
* Websocket帧
*/
pub struct WsFrame<'a> {
    head:       WsHead,             //头
    payload:    Option<&'a [u8]>,   //负载
}

impl<'a> WsFrame<'a> {
    //获取负载
    pub fn payload(&'a self) -> &'a Option<&'a [u8]> {
        &self.payload
    }
}

//获取指定字节的指定位的值
#[inline(always)]
fn get_bit(byte: u8, nth: u8) -> u8 {
    if byte & (1 << nth) == 0 {
        0
    } else {
        1
    }
}

//设置指定字节的指定位的值
#[inline(always)]
fn set_bit(byte: u8, nth: u8, val: u8) -> u8 {
    if val == 0 {
        byte & !(1 << nth)
    } else {
        byte | (1 << nth)
    }
}