use std::sync::Arc;
use std::collections::HashMap;

use fnv::FnvBuildHasher;

use tcp::{driver::{Socket, AsyncIOWait}, buffer_pool::WriteBuffer};

use crate::{connect::WsSocket,
            frame::{CLOSE_OPCODE, TEXT_OPCODE, BINARY_OPCODE, PING_OPCODE, PONG_OPCODE}};

/*
* Websocket子协议
*/
pub trait ChildProtocol<S: Socket, H: AsyncIOWait>: Send + Sync + 'static {
    //获取子协议名称
    fn protocol_name(&self) -> &str;

    //解码子协议
    fn decode_protocol(&self, connect: WsSocket<S, H>, context: &mut WsContext);
}

/*
* Websocket状态
*/
#[derive(Debug, Clone)]
pub enum WsStatus {
    HandShaking,    //正在握手中
    HandShaked,     //已握手
    Closing,        //正在关闭中
    Closed,         //已关闭
}

unsafe impl Send for WsStatus {}

/*
* Websocket帧类型
*/
#[derive(Debug, Clone)]
pub enum WsFrameType {
    Undefined,  //未定义
    Close,      //关闭帧
    Ping,       //Ping帧
    Pong,       //Pong帧
    Text,       //文本数据帧
    Binary,     //二进制数据帧
}

unsafe impl Send for WsFrameType {}

impl From<u8> for WsFrameType {
    fn from(opcode: u8) -> Self {
        match opcode {
            CLOSE_OPCODE => {
                WsFrameType::Close
            },
            PING_OPCODE => {
                WsFrameType::Ping
            },
            PONG_OPCODE => {
                WsFrameType::Pong
            },
            TEXT_OPCODE => {
                WsFrameType::Text
            },
            _ => {
                WsFrameType::Binary
            }
        }
    }
}

impl From<WsFrameType> for u8 {
    fn from(frame_type: WsFrameType) -> Self {
        match frame_type {
            WsFrameType::Ping => PING_OPCODE,
            WsFrameType::Pong => PONG_OPCODE,
            WsFrameType::Text => TEXT_OPCODE,
            WsFrameType::Binary => BINARY_OPCODE,
            _ => CLOSE_OPCODE,
        }
    }
}

impl WsFrameType {
    //是否是控制帧
    pub fn is_control(&self) -> bool {
        match self {
            &WsFrameType::Text | &WsFrameType::Binary => false,
            _ => true,
        }
    }
}

/*
* Websocket上下文
*/
#[derive(Debug)]
pub struct WsContext {
    status:         WsStatus,       //当前连接状态
    r#type:         WsFrameType,    //帧类型
    frames:         Vec<Vec<u8>>,   //Websocket帧缓冲
}

unsafe impl Send for WsContext {}

impl Default for WsContext {
    fn default() -> Self {
        WsContext {
            status: WsStatus::HandShaking,
            r#type: WsFrameType::Undefined,
            frames: Vec::with_capacity(3),
        }
    }
}

impl WsContext {
    //判断是否已握手
    pub fn is_handshaked(&self) -> bool {
        match &self.status {
            WsStatus::HandShaked => true,
            _ => false,
        }
    }

    //判断是否正在关闭
    pub fn is_closing(&self) -> bool {
        if let WsStatus::Closing = self.status {
            true
        } else {
            false
        }
    }

    //判断是否已关闭
    pub fn is_closed(&self) -> bool {
        if let WsStatus::Closed = self.status {
            true
        } else {
            false
        }
    }

    //设置连接状态
    pub fn set_status(&mut self, status: WsStatus) {
        self.status = status;
    }

    //是否是控制帧
    pub fn is_control(&self) -> bool {
        self.r#type.is_control()
    }

    //是否是二进制帧
    pub fn is_binary(&self) -> bool {
        !self.r#type.is_control()
    }

    //获取帧类型
    pub fn get_type(&self) -> WsFrameType {
        self.r#type.clone()
    }

    //设置帧类型
    pub fn set_type(&mut self, frame_type: u8) {
        //帧类型未定，则允许设置帧类型
        self.r#type = frame_type.into();
    }

    //弹出帧栈
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        self.frames.pop()
    }

    //压入帧栈
    pub fn push(&mut self, frame: Vec<u8>) {
        self.frames.push(frame);
    }

    //获取帧缓冲数据
    pub fn get_frames(&self) -> Vec<u8> {
        self.frames.concat()
    }

    //重置帧类型和帧缓冲
    pub fn reset(&mut self) {
        self.r#type = WsFrameType::Undefined;
        self.frames.clear();
    }
}
