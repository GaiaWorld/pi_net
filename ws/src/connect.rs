use std::sync::Arc;
use std::net::Shutdown;
use std::marker::PhantomData;

use mio::Token;

use tcp::driver::{Socket, AsyncIOWait, SocketHandle, AsyncReadTask};

use crate::{frame::{WsHead, WsFrame}, util::ChildProtocol};

/*
* Websocket状态
*/
pub enum WsStatus {
    HandShaking,    //正在握手中
    HandShaked,     //已握手
}

/*
* Websocket上下文
*/
pub struct WsContext {
    status: WsStatus,   //当前连接状态
}

impl Default for WsContext {
    fn default() -> Self {
        WsContext {
            status: WsStatus::HandShaking,
        }
    }
}

impl WsContext {
    //判断是否已握手
    pub fn is_handshaked(&self) -> bool {
        match &self.status {
            WsStatus::HandShaking => false,
            WsStatus::HandShaked => true,
        }
    }

    //设置连接状态
    pub fn set_status(&mut self, status: WsStatus) {
        self.status = status;
    }
}

/*
* Websocket连接
*/

pub struct WsSocket<S: Socket, H: AsyncIOWait> {
    socket:         SocketHandle<S>,                //当前连接的Tcp连接句柄
    window_bits:    u8,                             //当前连接的压缩窗口大小
    protocol:       Option<Arc<dyn ChildProtocol>>, //当前连接的子协议
    marker:         PhantomData<H>,
}

unsafe impl<S: Socket, H: AsyncIOWait> Send for WsSocket<S, H> {}

impl<S: Socket, H: AsyncIOWait> WsSocket<S, H> {
    //构建一个Websocket连接
    pub fn new(socket: SocketHandle<S>, window_bits: u8, protocol: Option<Arc<dyn ChildProtocol>>) -> Self {
        WsSocket {
            socket,
            window_bits,
            protocol,
            marker: PhantomData,
        }
    }

    //异步读Websocket帧头
    pub async fn read_head(handle: &SocketHandle<S>, waits: &H, window_bits: u8, protocol: Option<Arc<dyn ChildProtocol>>) {
        let mut is_first = true; //是否首次接收
        let mut size = WsHead::READ_HEAD_LEN;

        let mut head = WsHead::default();
        loop {
            match AsyncReadTask::async_read(handle.clone(), waits.clone(), size).await {
                Err(e) => {
                    println!("!!!> Websocket Read Frame Head Failed, reason: {:?}", e);
                    handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                    return;
                },
                Ok(bin) if is_first => {
                    head = WsHead::from(bin);
                    match head.need_size() {
                        0 => {
                            //头已读完成
                            break;
                        },
                        r => {
                            //还需要接收指定字节数的头数据
                            is_first = false;
                            size = r;
                            continue;
                        }
                    }
                },
                Ok(bin) => {
                    if let Err(e) = head.from_last(bin) {
                        println!("!!!> Websocket Read Frame Head Failed, reason: {:?}", e);
                        handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                        return;
                    }

                    if let None = head.get_key() {
                        //客户端上行数据没有掩码，则立即断开连接
                        println!("!!!> Websocket Read Frame Head Failed, reason: invalid client mask");
                        handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                        return;
                    }

                    //头已读完成
                    break;
                }
            }
        }

        WsSocket::<S, H>::read_payload(handle, waits, &head).await;
    }

    //异步读Websocket帧负载
    async fn read_payload(handle: &SocketHandle<S>, waits: &H, head: &WsHead) {
        let len = head.len() as usize; //负载长度

        match AsyncReadTask::async_read(handle.clone(), waits.clone(), len).await {
            Err(e) => {
                println!("!!!> Websocket Read Frame Payload Failed, reason: {:?}", e);
                handle.as_handle().as_ref().unwrap().borrow().close(Shutdown::Both);
                return;
            },
            Ok(bin) => {
                //负载读完成
                println!("!!!!!!payload: {:?}", String::from_utf8_lossy(bin));
            },
        }
    }
}