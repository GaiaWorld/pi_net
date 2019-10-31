use std::mem;
use std::sync::Arc;
use std::marker::PhantomData;
use std::net::{SocketAddr, Shutdown};
use std::io::{ErrorKind, Result, Error};

use mio::Token;
use log::warn;

use tcp::{driver::{Socket, AsyncIOWait, SocketHandle, AsyncWriteTask},
          buffer_pool::WriteBuffer,
          util::{ContextHandle, SocketContext, SocketEvent}};

use crate::{frame::{WsHead, WsPayload, WsFrame},
            util::{ChildProtocol, WsFrameType, WsSession, WsStatus}};

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

impl<S: Socket, H: AsyncIOWait> Clone for WsSocket<S, H> {
    fn clone(&self) -> Self {
        WsSocket {
            socket: self.socket.clone(),
            window_bits: self.window_bits,
            marker: PhantomData,
        }
    }
}

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

    //线程安全的获取Tcp连接令牌
    pub fn get_token(&self) -> Option<&Token> {
        self.socket.get_token()
    }

    //线程安全的获取Tcp连接的本地地址
    pub fn get_local(&self) -> &SocketAddr {
        self.socket.get_local()
    }

    //线程安全的获取Tcp连接的远端地址
    pub fn get_remote(&self) -> &SocketAddr {
        self.socket.get_remote()
    }

    //获取连接会话的句柄
    pub fn get_session(&self) -> Option<ContextHandle<WsSession>> {
        if let Some(handle) = self.socket.as_handle() {
            return handle.as_ref().borrow_mut().get_context().get::<WsSession>()
        }

        None
    }

    //线程安全的获取Tcp连接的唯一id
    pub fn get_uid(&self) -> Option<usize> {
        self.socket.get_uid()
    }

    //线程安全的设置Tcp连接超时定时器
    pub fn set_timeout(&self, timeout: usize, event: SocketEvent) {
        self.socket.set_timeout(timeout, event);
    }

    //线程安全的取消Tcp连接超时定时器
    pub fn unset_timeout(&self) {
        self.socket.unset_timeout();
    }

    //线程安全的判断是否是安全的Tcp连接
    pub fn is_security(&self) -> bool {
        self.socket.is_security()
    }

    //线程安全的分配一个用于发送的写缓冲区
    pub fn alloc(&self) -> WriteBuffer {
        self.socket.alloc().ok().unwrap().unwrap()
    }

    //线程安全的异步发送指定负载
    pub fn send(&self, msg_type: WsFrameType, payload: WriteBuffer) -> Result<()> {
        if let Some(buf) = WsFrame::<S, H>::single_with_window_bits_and_payload(msg_type, self.window_bits, payload).into_write_buf() {
            if let Some(handle) = buf.finish() {
                return self.socket.write(handle);
            }

            return Ok(());
        }

        Err(Error::new(ErrorKind::InvalidData, "invalid payload"))
    }

    //线程安全的异步关闭当前连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        close::<S, H>(&self.socket, reason)
    }
}

//线程安全的关闭指定Websocket连接，关闭前向对端发送关闭帧
fn close<S: Socket, H: AsyncIOWait>(handle: &SocketHandle<S>, reason: Result<()>) -> Result<()> {
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
    if let Ok(Some(mut buf)) = handle.alloc() {
        buf.get_iolist_mut().push_back(Vec::from(frame).into());
        if let Some(h) = buf.finish() {
            //向对端发送关闭帧，并关闭当前连接
            handle.write(h);
            return handle.close(reason);
        }
    }

    Ok(())
}

/*
* Websocket连接异步方法
*/
impl<S: Socket, H: AsyncIOWait> WsSocket<S, H> {
    //异步处理Tcp已读事件
    pub async fn handle_readed(handle: &SocketHandle<S>,
                               waits: &H,
                               window_bits: u8,
                               protocol: Arc<dyn ChildProtocol<S, H>>) {
        let mut h = handle.as_handle().unwrap().as_ref().borrow_mut().get_context().get::<WsSession>().unwrap();
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
                context.append(payload);
            }

            let head = frame.get_head();
            if head.is_single() {
                //数据帧，且只有单帧，则设置帧类型，并开始消息处理
                context.set_type(head.get_type());
                if let Err(e) = protocol.decode_protocol(Self::new(handle.clone(), window_bits), context) {
                    //协议处理失败，则立即关闭当前Ws连接
                    close::<S, H>(handle, Err(e));
                }

                //重置当前连接的当前帧，并继续读后续帧
                context.reset();
                if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                    //继续读失败，则立即关闭Ws连接
                    close::<S, H>(handle, Err(Error::new(ErrorKind::Other, format!("websocket read next message failed, reason: {:?}", e))));
                }

                return;
            } else if head.is_first() {
                //数据帧，当前是首帧，则设置帧类型，并继续读后续帧
                context.set_type(head.get_type());

                if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                    //继续读失败，则立即关闭Ws连接
                    close::<S, H>(handle, Err(Error::new(ErrorKind::Other, format!("websocket read next frame failed, reason: {:?}", e))));
                }

                return;
            } else if head.is_next() {
                //数据帧，当前是后续帧，则继续读后续帧
                if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                    //继续读失败，则立即关闭Ws连接
                    close::<S, H>(handle, Err(Error::new(ErrorKind::Other, format!("websocket read next frame failed, reason: {:?}", e))));
                }

                return;
            } else if head.is_finish() {
                //数据帧，当前是结束帧，则开始消息处理
                if let Err(e) = protocol.decode_protocol(Self::new(handle.clone(), window_bits), context) {
                    //协议处理失败，则立即关闭当前Ws连接
                    close::<S, H>(handle, Err(e));
                }

                //重置当前连接的当前帧，并继续读后续帧
                context.reset();
                if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                    //继续读失败，则立即关闭Ws连接
                    close::<S, H>(handle, Err(Error::new(ErrorKind::Other, format!("websocket read next message failed, reason: {:?}", e))));
                }

                return;
            } else {
                //控制帧，则设置帧类型
                context.set_type(head.get_type());
            }
        } else {
            //无法获取会话的可写引用，则表示有异常，立即关闭Ws连接
            close::<S, H>(handle, Err(Error::new(ErrorKind::Other, format!("Websocket Read Failed, reason: invalid writable context"))));
            return;
        }

        //控制帧，则开始控制处理
        WsSocket::handle_control(handle, waits, window_bits, h).await;
    }

    //异步处理控制帧
    async fn handle_control(handle: &SocketHandle<S>,
                            waits: &H,
                            window_bits: u8,
                            mut h: ContextHandle<WsSession>) {
        let frame_type = h.as_ref().get_type();
        match frame_type {
            wft@WsFrameType::Close => {
                //处理关闭帧
                if let Some(context) = h.as_mut() {
                    //修改当前连接为正在关闭中
                    context.set_status(WsStatus::Closing);

                    //填充负载
                    let payload;
                    let bin = context.as_buf();
                    if bin.len() == 0 {
                        //没有关闭状态码
                        payload = None;
                    } else {
                        //有关闭状态码
                        payload = Some(vec![bin[0], bin[1]]);
                    }

                    //响应关闭控制帧，并不再继续读连接的数据
                    WsSocket::resp_control(handle, waits, window_bits, wft, payload).await;

                    context.reset(); //重置当前连接的当前帧
                } else {
                    //无法获取会话的可写引用，则表示有异常，立即关闭Tcp连接
                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handle close frame failed, reason: invalid writable context"))));
                    return;
                }
            },
            WsFrameType::Ping => {
                if let Some(context) = h.as_mut() {
                    //处理Ping帧
                    let bin = context.as_buf();
                    if bin.len() == 0 {
                        //没有Ping负载，写入响应的Pong控制帧
                        WsSocket::resp_control(handle, waits, window_bits, WsFrameType::Pong, None).await;
                    } else {
                        //有Ping负载，写入响应的Pong控制帧
                        WsSocket::resp_control(handle, waits, window_bits, WsFrameType::Pong, Some(bin.to_vec())).await;
                    }

                    //继续读连接的数据
                    if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
                        //继续读失败，则立即关闭Tcp连接
                        handle.close(Err(Error::new(ErrorKind::Other, format!("websocket read next frame failed after handle ping, reason: {:?}", e))));
                    }

                    context.reset(); //重置当前连接的当前帧
                } else {
                    //无法获取会话的可写引用，则表示有异常，立即关闭Tcp连接
                    handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handle ping frame failed, reason: invalid writable context"))));
                    return;
                }
            },
            WsFrameType::Pong => {
                //TODO 处理Pong帧
            },
            wft => {
                //无效的控制帧，则立即关闭Tcp连接
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
        if let Some(mut h) = handle.as_handle().unwrap().as_ref().borrow().get_context().get::<WsSession>() {
            if let Some(context) = h.as_mut() {
                if context.is_closed() || context.is_handshaked() {
                    //当前连接已关闭或连接已握手，则忽略写成功事件
                    return;
                } else if context.is_closing() {
                    //当前连接正在关闭，且已发送回应的关闭帧，则修改当前连接状态为已关闭，立即释放可写会话，并立即关闭Tcp连接
                    context.set_status(WsStatus::Closed);
                    handle.close(Ok(()));
                } else {
                    //当前连接正在握手，则修改当前连接状态为已握手，并立即释放可写会话
                    context.set_status(WsStatus::HandShaked);
                }
            } else {
                //无法获取会话的可写引用，则表示有异常，立即关闭Tcp连接
                handle.close(Err(Error::new(ErrorKind::Other, format!("websocket write failed, reason: invalid writable context"))));
                return;
            }
        } else {
            //连接会话为空，则立即关闭Tcp连接
            handle.close(Err(Error::new(ErrorKind::Other, format!("websocket write failed, reason: websocket context empty"))));
            return;
        }

        //握手完成，准备异步接收客户端发送的Websocket数据帧
        if let Err(e) = handle.as_handle().unwrap().as_ref().borrow_mut().read_ready(WsHead::READ_HEAD_LEN) {
            //准备读失败，则立即关闭Tcp连接
            handle.close(Err(Error::new(ErrorKind::Other, format!("websocket handshanke Ok, but read ready error, reason: {:?}", e))));
        }
    }

    //异步处理Tcp已关闭事件
    pub async fn handle_closed(handle: SocketHandle<S>, waits: H, window_bits: u8, protocol: Arc<dyn ChildProtocol<S, H>>, result: Result<()>) {
        //连接已关闭，则立即释放Tcp连接的上下文
        match handle.as_handle().unwrap().as_ref().borrow_mut().get_context_mut().remove::<WsSession>() {
            Err(e) => {
                warn!("!!!> Free Context Failed by Websocket Close, uid: {:?}, local: {:?}, remote: {:?}, reason: {:?}", handle.get_uid(), handle.get_local(), handle.get_remote(), e);
            },
            Ok(opt) => {
                if let Some(context) = opt {
                    //关闭连接子协议
                    protocol.close_protocol(Self::new(handle.clone(), window_bits), context, result);
                }
            },
        }
    }

    //异步处理Tcp已超时事件
    pub async fn handle_timeouted(handle: SocketHandle<S>, waits: H, window_bits: u8, protocol: Arc<dyn ChildProtocol<S, H>>, event: SocketEvent) {
        let mut h = handle.as_handle().unwrap().as_ref().borrow_mut().get_context().get::<WsSession>().unwrap();
        if let Some(context) = h.as_mut() {
            if let Err(e) = protocol.protocol_timeout(Self::new(handle.clone(), window_bits), context, event) {
                //协议超时处理失败，则立即关闭当前Ws连接
                close::<S, H>(&handle, Err(e));
            } else {
                //协议超时处理成功，则立即关闭当前Ws连接
                close::<S, H>(&handle, Ok(()));
            }
        }
    }
}
