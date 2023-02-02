use std::mem;
use std::sync::Arc;
use std::net::{SocketAddr, Shutdown};
use std::io::{ErrorKind, Result, Error};

use mio::Token;
use bytes::Buf;
use log::warn;

use tcp::{Socket, SocketHandle, SocketEvent,
          utils::{SocketContext, ContextHandle, Hibernate, Ready}};

use crate::{frame::{WsHead, WsPayload, WsFrame},
            utils::{ChildProtocol, WsFrameType, WsSession, WsStatus}};

///
/// 服务端状态码
///
const CLOSE_NORMAL_CODE: u16 = 1000;        //正常关闭
const CLOSE_GOING_AWAY_CODE: u16 = 1001;    //错误关闭

///
/// Websocket连接
///
pub struct WsSocket<S: Socket> {
    socket:         SocketHandle<S>,    //当前连接的Tcp连接句柄
    window_bits:    u8,                 //当前连接的压缩窗口大小
}

unsafe impl<S: Socket> Send for WsSocket<S> {}

impl<S: Socket> Clone for WsSocket<S> {
    fn clone(&self) -> Self {
        WsSocket {
            socket: self.socket.clone(),
            window_bits: self.window_bits,
        }
    }
}

/*
* Websocket连接同步方法
*/
impl<S: Socket> WsSocket<S> {
    /// 构建一个Websocket连接
    pub fn new(socket: SocketHandle<S>, window_bits: u8) -> Self {
        WsSocket {
            socket,
            window_bits,
        }
    }

    /// 线程安全的异步广播指定负载
    pub fn broadcast(connects: &[WsSocket<S>],
                     msg_type: WsFrameType,
                     payload: Vec<u8>) -> Result<()> {
        if connects.len() == 0 {
            //连接为空，则忽略
            return Ok(());
        }

        if let Some(mut buf) = WsFrame::<S>::single_with_window_bits_and_payload(msg_type,
                                                                                 connects[0].window_bits,
                                                                                 payload).into_write_buf() {
            for connect in connects {
                connect.socket.write_ready(buf.clone());
            }

            return Ok(());
        }

        Err(Error::new(ErrorKind::InvalidData, "Invalid payload"))
    }

    /// 线程安全的判断连接是否关闭
    pub fn is_closed(&self) -> bool {
        self.socket.is_closed()
    }

    /// 线程安全的获取Tcp连接令牌
    pub fn get_token(&self) -> &Token {
        self.socket.get_token()
    }

    /// 线程安全的获取Tcp连接唯一id
    pub fn get_uid(&self) -> usize {
        self.socket.get_uid()
    }

    /// 线程安全的获取Tcp连接的本地地址
    pub fn get_local(&self) -> &SocketAddr {
        self.socket.get_local()
    }

    /// 线程安全的获取Tcp连接的远端地址
    pub fn get_remote(&self) -> &SocketAddr {
        self.socket.get_remote()
    }

    /// 获取内部Tcp连接句柄
    pub fn get_inner(&self) -> &SocketHandle<S> {
        &self.socket
    }

    /// 获取连接会话的句柄
    pub fn get_session(&self) -> Option<ContextHandle<WsSession>> {
        return unsafe { (&*self.socket.get_context().get()).get::<WsSession>() }
    }

    /// 线程安全的设置Tcp连接超时定时器
    pub fn set_timeout(&self, timeout: usize, event: SocketEvent) {
        self.socket.set_timeout(timeout, event);
    }

    /// 线程安全的取消Tcp连接超时定时器
    pub fn unset_timeout(&self) {
        self.socket.unset_timeout();
    }

    /// 线程安全的判断是否是安全的Tcp连接
    pub fn is_security(&self) -> bool {
        self.socket.is_security()
    }

    /// 线程安全的异步发送指定负载
    pub fn send(&self, msg_type: WsFrameType,
                payload: Vec<u8>) -> Result<()> {
        if let Some(buf) = WsFrame::<S>::single_with_window_bits_and_payload(msg_type,
                                                                             self.window_bits,
                                                                             payload).into_write_buf() {
            return self.socket.write_ready(buf);
        }

        Err(Error::new(ErrorKind::InvalidData, "Invalid payload"))
    }

    /// 线程安全的异步休眠当前连接，直到被唤醒，返回空表示连接已关闭
    pub fn hibernate(&self, ready: Ready) -> Option<Hibernate<S>> {
        self.socket.hibernate(self.socket.clone(), ready)
    }

    /// 线程安全的唤醒被休眠的当前连接，如果当前连接未被休眠，则忽略
    /// 唤醒过程可能会被阻塞，这不会导致线程阻塞而是返回假，调用者可以继续尝试唤醒，直到返回真
    pub fn wakeup(&self, result: Result<()>) -> bool {
        self.socket.wakeup(result)
    }

    /// 线程安全的异步关闭当前连接
    pub fn close(&self, reason: Result<()>) -> Result<()> {
        close::<S>(&self.socket, reason)
    }
}

// 线程安全的关闭指定Websocket连接，关闭前向对端发送关闭帧
fn close<S: Socket>(handle: &SocketHandle<S>,
                    reason: Result<()>) -> Result<()> {
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

    //向对端发送关闭帧
    let frame = WsFrame::<S>::control_with_payload(WsFrameType::Close, payload); //创建关闭帧
    if let Err(e) = handle.write_ready(Vec::from(frame)) {
        return handle.close(Err(Error::new(ErrorKind::Other,
                                           format!("WebSocket send close frame failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                   handle.get_token(),
                                                   handle.get_remote(),
                                                   handle.get_local(),
                                                   e))));
    }

    //关闭当前连接
    return handle.close(reason);
}

/*
* Websocket连接异步方法
*/
impl<S: Socket> WsSocket<S> {
    /// 异步处理Tcp已读事件
    pub async fn handle_readed(handle: &SocketHandle<S>,
                               window_bits: u8,
                               protocol: Arc<dyn ChildProtocol<S>>) {
        let mut h: ContextHandle<WsSession> = unsafe { (&*handle.get_context().get()).get::<WsSession>().unwrap() };
        if h.as_ref().is_closed() || h.as_ref().is_closing() {
            //当前连接已关闭或正在关闭中，则忽略所有读
            return;
        }

        //读数据，并填充帧数据
        let mut frames = Vec::new();
        while unsafe { (&mut *handle.get_read_buffer().get()).as_ref().unwrap().remaining() } > 0 {
            let mut frame = WsFrame::<S>::default();
            WsFrame::read_head(handle, window_bits, &mut frame).await;

            frames.push(frame);
            if frames.len() > 128 {
                //分帧过多，则立即关闭当前连接
                close::<S>(handle, Err(Error::new(ErrorKind::Other,
                                                  format!("Websocket Read Failed, token: {:?}, remote: {:?}, local: {:?}, frame_len: {:?}, reason: out of frames",
                                                          handle.get_token(),
                                                          handle.get_remote(),
                                                          handle.get_local(),
                                                          frames.len()))));
                return;
            }
        }

        for mut frame in frames {
            if let Some(context) = h.as_mut() {
                //根据帧类型，处理帧数据
                if let WsPayload::Raw(payload) = frame.payload() {
                    //当前帧有祼负载，则缓冲负载
                    context.fill_msg(payload);
                }

                let head = frame.get_head();
                if head.is_single() {
                    //数据帧，且只有单帧，则设置帧类型，并开始消息处理
                    context.set_type(head.get_type());
                    context.finish_msg();

                    if let Err(e) = protocol
                        .decode_protocol(Self::new(handle.clone(), window_bits),
                                         context).await {
                        //协议处理失败，则立即关闭当前Ws连接
                        close::<S>(handle, Err(e));
                    }

                    //重置当前连接的当前帧，并继续读后续帧
                    context.reset();
                    continue;
                } else if head.is_first() {
                    //数据帧，当前是首帧，则设置帧类型，并继续读后续帧
                    context.set_type(head.get_type());
                    continue;
                } else if head.is_next() {
                    //数据帧，当前是后续帧，则继续读后续帧
                    continue;
                } else if head.is_finish() {
                    //数据帧，当前是结束帧，则开始消息处理
                    context.finish_msg();

                    if let Err(e) = protocol
                        .decode_protocol(Self::new(handle.clone(), window_bits),
                                         context).await {
                        //协议处理失败，则立即关闭当前Ws连接
                        close::<S>(handle, Err(e));
                    }

                    //重置当前连接的当前帧，并继续读后续帧
                    context.reset();
                    continue;
                } else {
                    //控制帧，则设置帧类型
                    context.set_type(head.get_type());
                }
            } else {
                //无法获取会话的可写引用，则表示有异常，立即关闭Ws连接
                close::<S>(handle, Err(Error::new(ErrorKind::Other,
                                                  format!("Websocket Read Failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid writable context",
                                                          handle.get_token(),
                                                          handle.get_remote(),
                                                          handle.get_local()))));
                continue;
            }

            //控制帧，则开始控制处理
            WsSocket::handle_control(handle, window_bits, &mut h).await;
        }
    }

    /// 异步处理控制帧
    async fn handle_control(handle: &SocketHandle<S>,
                            window_bits: u8,
                            h: &mut ContextHandle<WsSession>) {
        let frame_type = h.as_ref().get_type();
        match frame_type {
            wft@WsFrameType::Close => {
                //处理关闭帧
                if let Some(context) = h.as_mut() {
                    //修改当前连接为正在关闭中
                    context.set_status(WsStatus::Closing);

                    //填充负载
                    let payload;
                    let bin = context.pop_msg();
                    if bin.len() == 0 {
                        //没有关闭状态码
                        payload = None;
                    } else {
                        //有关闭状态码
                        payload = Some(vec![bin[0], bin[1]]);
                    }

                    //响应关闭控制帧，并不再继续读连接的数据
                    WsSocket::resp_control(handle,
                                           window_bits,
                                           wft,
                                           payload).await;

                    context.reset(); //重置当前连接的当前帧
                } else {
                    //无法获取会话的可写引用，则表示有异常，立即关闭Tcp连接
                    handle.close(Err(Error::new(ErrorKind::Other,
                                                format!("Websocket handle close frame failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid writable context",
                                                        handle.get_token(),
                                                        handle.get_remote(),
                                                        handle.get_local()))));
                    return;
                }
            },
            WsFrameType::Ping => {
                if let Some(context) = h.as_mut() {
                    //处理Ping帧
                    let bin = context.pop_msg();
                    if bin.len() == 0 {
                        //没有Ping负载，写入响应的Pong控制帧
                        WsSocket::resp_control(handle,
                                               window_bits,
                                               WsFrameType::Pong,
                                               None).await;
                    } else {
                        //有Ping负载，写入响应的Pong控制帧
                        WsSocket::resp_control(handle,
                                               window_bits,
                                               WsFrameType::Pong,
                                               Some(bin.to_vec())).await;
                    }

                    context.reset(); //重置当前连接的当前帧
                } else {
                    //无法获取会话的可写引用，则表示有异常，立即关闭Tcp连接
                    handle.close(Err(Error::new(ErrorKind::Other,
                                                format!("Websocket handle ping frame failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid writable context",
                                                        handle.get_token(),
                                                        handle.get_remote(),
                                                        handle.get_local()))));
                    return;
                }
            },
            WsFrameType::Pong => {
                //TODO 处理Pong帧
            },
            wft => {
                //无效的控制帧，则立即关闭Tcp连接
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket handle control frame failed, token: {:?}, remote: {:?}, local: {:?}, type: {:?}, reason: invalid control frame",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    wft))));
            }
        }
    }

    /// 异步回应控制帧
    async fn resp_control(handle: &SocketHandle<S>,
                          window_bits: u8,
                          frame_type: WsFrameType,
                          payload: Option<Vec<u8>>) {
        match frame_type {
            wft@WsFrameType::Close | wft@WsFrameType::Pong => {
                //回应关闭帧或回应Ping帧
                let buf = Vec::from(WsFrame::<S>::control_with_payload(wft, payload));
                if let Err(e) = handle.write_ready(buf) {
                    handle.close(Err(Error::new(ErrorKind::Other,
                                                format!("WebSocket write error by response control frame, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                        handle.get_token(),
                                                        handle.get_remote(),
                                                        handle.get_local(),
                                                        e))));
                }
            },
            _ => (), //忽略其它控制帧回应
        }
    }

    /// 异步处理Tcp已写事件
    pub async fn handle_writed(handle: SocketHandle<S>) {
        if let Some(mut h) = unsafe { (&*handle.get_context().get()).get::<WsSession>() } {
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
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket write failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid writable context",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local()))));
                return;
            }
        } else {
            if !handle.is_security() {
                //非安全连接，且连接会话为空，则立即关闭Tcp连接
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Websocket write failed, token: {:?}, remote: {:?}, local: {:?}, reason: websocket context empty",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local()))));
                return;
            }
        }

        //握手完成或连接正在关闭，准备异步接收客户端发送的Websocket数据帧，则预填充连接读缓冲区
        // unsafe { (&mut *handle.get_read_buffer().get()).try_fill().await; }
    }

    /// 异步处理Tcp已关闭事件
    pub async fn handle_closed(handle: SocketHandle<S>,
                               window_bits: u8,
                               protocol: Arc<dyn ChildProtocol<S>>,
                               result: Result<()>) {
        //连接已关闭，则立即释放Tcp连接的上下文
        match unsafe { (&mut *handle.get_context().get()).remove::<WsSession>() } {
            Err(e) => {
                warn!("Free Context Failed by Websocket Close, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                    handle.get_token(),
                    handle.get_remote(),
                    handle.get_local(),
                    e);
            },
            Ok(opt) => {
                if let Some(context) = opt {
                    //关闭连接子协议
                    protocol
                        .close_protocol(Self::new(handle.clone(), window_bits),
                                        context,
                                        result).await;
                }
            },
        }
    }

    /// 异步处理Tcp已超时事件
    pub async fn handle_timeouted(handle: SocketHandle<S>,
                                  window_bits: u8,
                                  protocol: Arc<dyn ChildProtocol<S>>,
                                  event: SocketEvent) {
        let mut h = unsafe { (&*handle.get_context().get()).get::<WsSession>().unwrap() };
        if let Some(context) = h.as_mut() {
            if let Err(e) = protocol
                .protocol_timeout(Self::new(handle.clone(), window_bits),
                                  context,
                                  event).await {
                //协议超时处理失败，则立即关闭当前Ws连接
                close::<S>(&handle, Err(e));
            } else {
                //协议超时处理成功，则立即关闭当前Ws连接
                close::<S>(&handle, Ok(()));
            }
        }
    }
}
