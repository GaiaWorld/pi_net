use std::mem;
use std::sync::Arc;
use std::net::Shutdown;
use std::marker::PhantomData;
use std::io::{ErrorKind, Result, Error};

use mio::Token;

use tcp::{driver::{Socket, AsyncIOWait, SocketHandle, AsyncWriteTask},
          buffer_pool::WriteBuffer,
          util::{ContextHandle, SocketContext}};

use crate::{frame::{WsHead, WsPayload, WsFrame},
            util::{ChildProtocol, WsFrameType, WsContext, WsStatus}};

/*
* 服务端状态码
*/
const CLOSE_NORMAL_CODE: u16 = 1000;        //正常关闭
const CLOSE_GOING_AWAY_CODE: u16 = 1001;    //错误关闭

/*
* Websocket连接
*/
pub struct WsSocket<S: Socket, H: AsyncIOWait> {
    socket:         SocketHandle<S>,                //当前连接的Tcp连接句柄
    window_bits:    u8,                             //当前连接的压缩窗口大小
    marker:         PhantomData<H>,
}

unsafe impl<S: Socket, H: AsyncIOWait> Send for WsSocket<S, H> {}

/*
* Websocket连接同步方法
*/
impl<S: Socket, H: AsyncIOWait> WsSocket<S, H> {
    //构建一个Websocket连接
    pub fn new(socket: SocketHandle<S>, window_bits: u8) -> Self {
        WsSocket {
            socket,
            window_bits,
            marker: PhantomData,
        }
    }

    //线程安全的判断连接是否关闭
    pub fn is_closed(&self) -> bool {
        self.socket.is_closed()
    }

    //线程安全的判断是否写后立即刷新连接
    pub fn is_flush(&self) -> bool {
        self.socket.is_flush()
    }

    //线程安全的设置是否写后立即刷新连接
    pub fn set_flush(&self, flush: bool) {
        self.socket.set_flush(flush);
    }

    //线程安全的分配一个用于发送的写缓冲区
    pub fn alloc(&self) -> WriteBuffer {
        self.socket.alloc().ok().unwrap().unwrap()
    }

    //线程安全的异步广播指定负载
    pub fn broadcast(connects: &[WsSocket<S, H>], msg_type: WsFrameType, payload: WriteBuffer) -> Result<()> {
        if connects.len() == 0 {
            //连接为空，则忽略
            return Ok(());
        }

        if let Some(mut buf) = WsFrame::<S, H>::single_with_window_bits_and_payload(msg_type, connects[0].window_bits, payload).into_write_buf() {
            if let Some(handle) = buf.finish() {
                for connect in connects {
                    connect.socket.write(handle.clone());
                }
            }

            return Ok(());
        }

        Err(Error::new(ErrorKind::InvalidData, "invalid payload"))
    }

    //线程安全的异步发送指定负载
    pub fn send(&self, msg_type: WsFrameType, payload: WriteBuffer) -> Result<()> {
        if let Some(mut buf) = WsFrame::<S, H>::single_with_window_bits_and_payload(msg_type, self.window_bits, payload).into_write_buf() {
            if let Some(handle) = buf.finish() {
                return self.socket.write(handle);
            }

            return Ok(());
        }

        Err(Error::new(ErrorKind::InvalidData, "invalid payload"))
    }

    //线程安全的异步关闭当前连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        let payload= match &reason {
            Err(_) => {
                //错误关闭
                Some(vec![((CLOSE_GOING_AWAY_CODE >> 8) & 0xff) as u8, (CLOSE_GOING_AWAY_CODE & 0xff) as u8])
            },
            Ok(_) => {
                //正常关闭
                Some(vec![((CLOSE_NORMAL_CODE >> 8) & 0xff) as u8, (CLOSE_NORMAL_CODE & 0xff) as u8])
            },
        };

        //创建关闭帧
        let frame = WsFrame::<S, H>::control_with_payload(WsFrameType::Close, payload);
        let mut buf = self.alloc();
        buf.get_iolist_mut().push_back(Vec::from(frame).into());

        if let Some(handle) = buf.finish() {
            //向对端发送关闭帧，并关闭当前连接
            self.socket.write(handle);
            return self.socket.close(reason);
        }

        Ok(())
    }
}

/*
* Websocket连接异步方法
*/
impl<S: Socket, H: AsyncIOWait> WsSocket<S, H> {
    //异步处理Tcp已读事件
    pub async fn handle_readed(handle: &SocketHandle<S>,
                               waits: &H,
                               window_bits: u8,
                               protocol: Option<Arc<dyn ChildProtocol<S, H>>>) {
        let mut h = handle.as_handle().unwrap().as_ref().borrow_mut().get_context().get::<WsContext>().unwrap();
        if h.as_ref().is_closed() || h.as_ref().is_closing() {
            //当前连接已关闭或正在关闭中，则忽略所有读
            return;
        }

        //读数据，并填充帧数据
        let mut frame = WsFrame::<S, H>::default();
        WsFrame::read_head(handle, waits, window_bits, &mut frame).await;

        if let Some(context) = h.as_mut() {
            //根据帧类型，处理帧数据
            if let WsPayload::Raw(payload) = frame.payload() {
                //当前帧有祼负载，则缓冲负载
                context.push(payload);
            }

            let head = frame.get_head();
            if head.is_single() {
                //数据帧，且只有单帧，则设置帧类型，并开始消息处理
                context.set_type(head.get_type());
                if let Some(p) = protocol {
                    p.decode_protocol(Self::new(handle.clone(), window_bits), context);
                }

                return;
            } else if head.is_first() {
                //数据帧，当前是首帧，则设置帧类型，并继续读后续帧
                context.set_type(head.get_type());

                if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                    //继续读失败，则立即关闭连接
                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket read next frame failed, reason: {:?}", e))));
                }

                return;
            } else if head.is_next() {
                //数据帧，当前是后续帧，则继续读后续帧
                if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                    //继续读失败，则立即关闭连接
                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket read next frame failed, reason: {:?}", e))));
                }

                return;
            } else if head.is_finish() {
                //数据帧，当前是结束帧，则开始消息处理
                if let Some(p) = protocol {
                    p.decode_protocol(Self::new(handle.clone(), window_bits), context);
                }

                return;
            } else {
                //控制帧，则设置帧类型
                context.set_type(head.get_type());
            }
        } else {
            //无法获取上下文的可写引用，则表示有异常，立即关闭连接
            handle.close(Err(Error::new(ErrorKind::Other, format!("Websocket Read Failed, reason: invalid writable context"))));
            return;
        }

        //控制帧，则开始控制处理
        WsSocket::handle_control(handle, waits, window_bits, h).await;
    }

    //异步处理控制帧
    async fn handle_control(handle: &SocketHandle<S>,
                            waits: &H,
                            window_bits: u8,
                            mut h: ContextHandle<WsContext>) {
        let frame_type = h.as_ref().get_type();
        let bin = h.as_ref().get_frames();
        match frame_type {
            wft@WsFrameType::Close => {
                //处理关闭帧
                if let Some(context) = h.as_mut() {
                    //修改当前连接为正在关闭中，并立即释放可写上下文
                    context.set_status(WsStatus::Closing);
                } else {
                    //无法获取上下文的可写引用，则表示有异常，立即关闭连接
                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handle control failed, reason: invalid writable context"))));
                    return;
                }

                //填充负载
                let payload;
                if bin.len() == 0 {
                    //没有关闭状态码
                    payload = None;
                } else {
                    //有关闭状态码
                    payload = Some(vec![bin[0], bin[1]]);
                }

                //响应关闭控制帧，并不再继续读连接的数据
                WsSocket::resp_control(handle, waits, window_bits, wft, payload).await;
            },
            WsFrameType::Ping => {
                //处理Ping帧
                if bin.len() == 0 {
                    //没有Ping负载，写入响应的Pong控制帧
                    WsSocket::resp_control(handle, waits, window_bits, WsFrameType::Pong, None).await;
                } else {
                    //有Ping负载，写入响应的Pong控制帧
                    WsSocket::resp_control(handle, waits, window_bits, WsFrameType::Pong, Some(bin)).await;
                }

                //继续读连接的数据
                if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                    //继续读失败，则立即关闭连接
                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket read next frame failed after handle ping, reason: {:?}", e))));
                }
            },
            WsFrameType::Pong => {
                //TODO 处理Pong帧
            },
            wft => {
                //无效的控制帧，则立即关闭连接
                handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handle control frame failed, type: {:?}, reason: invalid control frame", wft))));
            }
        }
    }

    //异步回应控制帧
    async fn resp_control(handle: &SocketHandle<S>,
                          waits: &H,
                          window_bits: u8,
                          frame_type: WsFrameType,
                          payload: Option<Vec<u8>>) {
        let mut buf = handle.as_handle().as_ref().unwrap().borrow().get_write_buffer().alloc().ok().unwrap().unwrap();

        match frame_type {
            wft@WsFrameType::Close | wft@WsFrameType::Pong => {
                //回应关闭帧或回应Ping帧
                buf.get_iolist_mut().push_back(Vec::from(WsFrame::<S, H>::control_with_payload(wft, payload)).into());

                if let Some(buf_handle) = buf.finish() {
                    if let Err(e) = AsyncWriteTask::async_write(handle.clone(), waits.clone(), buf_handle).await {
                        handle.close(Err(Error::new(ErrorKind::Other, format!("webSocket write error by response control frame, reason: {:?}", e))));
                    }
                }
            },
            _ => (), //忽略其它控制帧回应
        }
    }

    //异步处理Tcp已写事件
    pub async fn handle_writed(handle: SocketHandle<S>, waits: H) {
        if let Some(mut h) = handle.as_handle().unwrap().as_ref().borrow().get_context().get::<WsContext>() {
            if let Some(context) = h.as_mut() {
                if context.is_closed() || context.is_handshaked() {
                    //当前连接已关闭或连接已握手，则忽略写成功事件
                    return;
                } else if context.is_closing() {
                    //当前连接正在关闭，且已发送回应的关闭帧，则修改当前连接状态为已关闭，立即释放可写上下文，并立即关闭连接
                    context.set_status(WsStatus::Closed);
                    handle.close(Ok(()));
                } else {
                    //当前连接正在握手，则修改当前连接状态为已握手，并立即释放可写上下文
                    context.set_status(WsStatus::HandShaked);
                }
            } else {
                //无法获取上下文的可写引用，则表示有异常，立即关闭连接
                handle.close(Err(Error::new(ErrorKind::Other, format!("websocket write failed, reason: invalid writable context"))));
                return;
            }
        } else {
            //连接上下文为空，则立即关闭连接
            handle.close(Err(Error::new(ErrorKind::Other, format!("websocket write failed, reason: websocket context empty"))));
            return;
        }

        //握手完成，准备异步接收客户端发送的Websocket数据帧
        if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
            //准备读失败，则立即关闭连接
            handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handshanke Ok, but read ready error, reason: {:?}", e))));
        }
    }

    //异步处理Tcp已关闭事件
    pub async fn handle_closed(handle: SocketHandle<S>, waits: H, result: Result<()>) {
        let mut is_closed = false;
        if let Some(mut h) = handle.as_handle().unwrap().as_ref().borrow().get_context().get::<WsContext>() {
            is_closed = h.as_ref().is_closed();
        }

        if is_closed {
            //连接已关闭，则立即释放连接上下文
            if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().get_context_mut().remove::<WsContext>() {
                println!("!!!> Free Context Failed by Websocket Close, reason: {:?}", e);
            }
        }

        if let Err(e) = result {
            println!("!!!> Websocket Closed by Error, reason: {:?}", e);
        }
    }
}
