use std::mem;
use std::sync::Arc;
use std::net::{SocketAddr, Shutdown};
use std::io::{ErrorKind, Result, Error};

use mio::Token;
use futures::future::LocalBoxFuture;
use bytes::Buf;
use log::warn;

use tcp::{Socket, SocketHandle, SocketEvent,
          utils::{SocketContext, ContextHandle, ContextAccessError, Hibernate, Ready}};

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

    /// 在所属线程（owner thread）取得普通会话句柄（context handle），不占用访问许可（access permit）。
    ///
    /// 无额外入参；空值（None）表示无会话（session）或取得结果（result）不可用，后者保留诊断。
    /// 返回值仅同线程（thread）使用；缓存（cache）可跨等待（await），视图（view）须短期归还。
    /// 正常取得为O(1)管理工作，无等待或网络I/O；错误诊断（diagnostic）沿宿主日志器（logger），
    /// 可有同步成本，不承诺固定时限。状态（state）边界沿SocketHandle::get_context。
    pub fn get_session(&self) -> Option<ContextHandle<WsSession>> {
        match self.socket.get_context() {
            Ok(context) => context.get::<WsSession>(),
            Err(error) => {
                warn!("Websocket session unavailable, token: {:?}, reason: {:?}", self.socket.get_token(), error);
                None
            },
        }
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
        let context = match handle.get_context() {
            Ok(context) => context,
            Err(error) => {
                warn!("Websocket read context unavailable, token: {:?}, reason: {:?}", handle.get_token(), error);
                return;
            },
        };
        let h = match context.get::<WsSession>() {
            Some(handle) => handle,
            None => return,
        };
        drop(context);
        let inactive = match h.try_read() {
            Ok(view) => view.as_ref().is_closed() || view.as_ref().is_closing(),
            Err(ContextAccessError::Closed) => return,
            Err(error) => {
                let _ = close::<S>(handle, Err(Error::new(ErrorKind::WouldBlock,
                    format!("Websocket read context access rejected: {:?}", error))));
                return;
            },
        };
        if inactive { return; }

        let mut frames = Vec::new();
        while unsafe { (&mut *handle.get_read_buffer().get()).as_ref().unwrap().remaining() } > 0 {
            let mut frame = WsFrame::<S>::default();
            WsFrame::read_head(handle, window_bits, &mut frame).await;
            frames.push(frame);
            if frames.len() > 128 {
                let _ = close::<S>(handle, Err(Error::new(ErrorKind::Other,
                    format!("Websocket Read Failed, token: {:?}, remote: {:?}, local: {:?}, frame_len: {:?}, reason: out of frames",
                        handle.get_token(), handle.get_remote(), handle.get_local(), frames.len()))));
                return;
            }
        }

        for mut frame in frames {
            let completed = frame.get_head().is_single() || frame.get_head().is_finish();
            let strict = completed && protocol.is_strict();
            // 原有子协议入口（child protocol entry）同步取得消息（message）并返回Future<'static>。
            // 先结束短许可（access permit），再等待（await）该返回值；完成后只访问原Entry。
            // 关联主线第43节；不把普通句柄（context handle）转为可长期持有的引用（reference）。
            let prepared: Result<(bool, Option<LocalBoxFuture<'static, Result<()>>>)> = (|| {
                let mut view = h.try_write().map_err(|error| Error::new(ErrorKind::WouldBlock,
                    format!("Websocket read context access rejected: {:?}", error)))?;
                let context = view.as_mut();
                if let WsPayload::Raw(payload) = frame.payload() {
                    context.fill_msg(payload);
                }
                let head = frame.get_head();
                if head.is_single() || head.is_finish() {
                    if strict {
                        let message = if head.is_single() { context.as_msg() } else { context.as_first_msg() };
                        if message.len() < 4 {
                            return Err(Error::new(ErrorKind::Other,
                                format!("Websocket check message nonce Failed, token: {:?}, remote: {:?}, local: {:?}, seed: {:?}, nonce_count: {:?}, reason: invalid message nonce length",
                                    handle.get_token(), handle.get_remote(), handle.get_local(), context.get_seed(), context.get_nonce_count())));
                        }
                        let nonce = u32::from_le_bytes(message[0..4].try_into().unwrap());
                        if !context.check_nonce(nonce) {
                            return Err(Error::new(ErrorKind::Other,
                                format!("Websocket check message nonce Failed, token: {:?}, remote: {:?}, local: {:?}, nonce: {:?}, seed: {:?}, nonce_count: {:?}, reason: invalid message nonce",
                                    handle.get_token(), handle.get_remote(), handle.get_local(), nonce, context.get_seed(), context.get_nonce_count())));
                        }
                    }
                    if head.is_single() { context.set_type(head.get_type()); }
                    context.finish_msg();
                    let decoded = protocol.decode_protocol(Self::new(handle.clone(), window_bits), context);
                    Ok((false, Some(decoded)))
                } else if head.is_first() {
                    context.set_type(head.get_type());
                    Ok((false, None))
                } else if head.is_next() {
                    Ok((false, None))
                } else {
                    context.set_type(head.get_type());
                    Ok((true, None))
                }
            })();
            let (control, decoded) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = close::<S>(handle, Err(error));
                    continue;
                },
            };
            if let Some(decoded) = decoded {
                if let Err(error) = decoded.await {
                    let _ = close::<S>(handle, Err(error));
                }
                let reset = h.try_write().map(|mut view| view.as_mut().reset());
                if let Err(error) = reset {
                    if error != ContextAccessError::Closed {
                        let _ = close::<S>(handle, Err(Error::new(ErrorKind::WouldBlock,
                            format!("Websocket reset context access rejected: {:?}", error))));
                    }
                }
            } else if control {
                Self::handle_control(handle, window_bits, &h).await;
            }
        }
    }

    /// 异步处理控制帧
    async fn handle_control(handle: &SocketHandle<S>,
                            window_bits: u8,
                            h: &ContextHandle<WsSession>) {
        let frame_type = match h.try_read() {
            Ok(view) => view.as_ref().get_type(),
            Err(ContextAccessError::Closed) => return,
            Err(error) => {
                let _ = handle.close(Err(Error::new(ErrorKind::WouldBlock,
                    format!("Websocket control context access rejected: {:?}", error))));
                return;
            },
        };
        match frame_type {
            wft@WsFrameType::Close | wft@WsFrameType::Ping => {
                let payload = (|| {
                    let mut view = h.try_write().map_err(|error| Error::new(ErrorKind::WouldBlock,
                        format!("Websocket control context access rejected: {:?}", error)))?;
                    let context = view.as_mut();
                    if matches!(wft, WsFrameType::Close) { context.set_status(WsStatus::Closing); }
                    let bin = context.pop_msg();
                    let payload = if bin.is_empty() { None }
                        else if matches!(wft, WsFrameType::Close) { Some(vec![bin[0], bin[1]]) }
                        else { Some(bin.to_vec()) };
                    Ok::<_, Error>(payload)
                })();
                let payload = match payload {
                    Ok(payload) => payload,
                    Err(error) => { let _ = handle.close(Err(error)); return; },
                };
                // 数据（data）已移入局部负载（payload）；网络（network）操作不保留访问许可（access permit）。
                let reply_type = if matches!(wft, WsFrameType::Close) { WsFrameType::Close } else { WsFrameType::Pong };
                Self::resp_control(handle, window_bits, reply_type, payload).await;
                let reset = h.try_write().map(|mut view| view.as_mut().reset());
                if let Err(error) = reset {
                    if error != ContextAccessError::Closed {
                        let _ = handle.close(Err(Error::new(ErrorKind::WouldBlock,
                            format!("Websocket control reset access rejected: {:?}", error))));
                    }
                }
            },
            WsFrameType::Pong => {
                // 原Pong处理（Pong handling）不增加业务动作。
            },
            wft => {
                let _ = handle.close(Err(Error::new(ErrorKind::Other,
                    format!("Websocket handle control frame failed, token: {:?}, remote: {:?}, local: {:?}, type: {:?}, reason: invalid control frame",
                        handle.get_token(), handle.get_remote(), handle.get_local(), wft))));
            },
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
        let context = match handle.get_context() {
            Ok(context) => context,
            Err(error) => {
                warn!("Websocket write context unavailable, token: {:?}, reason: {:?}", handle.get_token(), error);
                return;
            },
        };
        if let Some(h) = context.get::<WsSession>() {
            let closing = {
                let mut view = match h.try_write() {
                    Ok(view) => view,
                    Err(ContextAccessError::Closed) => return,
                    Err(error) => {
                        let _ = handle.close(Err(Error::new(ErrorKind::WouldBlock,
                            format!("Websocket write context access rejected: {:?}", error))));
                        return;
                    },
                };
                let session = view.as_mut();
                if session.is_closed() || session.is_handshaked() { return; }
                if session.is_closing() {
                    session.set_status(WsStatus::Closed);
                    true
                } else {
                    session.set_status(WsStatus::HandShaked);
                    false
                }
            };
            if closing { let _ = handle.close(Ok(())); }
        } else if !handle.is_security() {
            let _ = handle.close(Err(Error::new(ErrorKind::Other,
                format!("Websocket write failed, token: {:?}, remote: {:?}, local: {:?}, reason: websocket context empty",
                    handle.get_token(), handle.get_remote(), handle.get_local()))));
        }
    }

    /// 异步处理Tcp已关闭事件
    pub async fn handle_closed(handle: SocketHandle<S>,
                               window_bits: u8,
                               protocol: Arc<dyn ChildProtocol<S>>,
                               result: Result<()>) {
        //连接已关闭，则立即释放Tcp连接的上下文
        let context = match handle.get_context() {
            Ok(context) => context,
            Err(error) => {
                warn!("Websocket close context unavailable, token: {:?}, reason: {:?}", handle.get_token(), error);
                return;
            },
        };
        let removed = context.try_remove::<WsSession>();
        drop(context);
        match removed {
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
        let context = match handle.get_context() {
            Ok(context) => context,
            Err(error) => {
                warn!("Websocket timeout context unavailable, token: {:?}, reason: {:?}", handle.get_token(), error);
                return;
            },
        };
        let h = match context.get::<WsSession>() { Some(h) => h, None => return };
        drop(context);
        let timeout = {
            let mut view = match h.try_write() {
                Ok(view) => view,
                Err(ContextAccessError::Closed) => return,
                Err(error) => {
                    let _ = close::<S>(&handle, Err(Error::new(ErrorKind::WouldBlock,
                        format!("Websocket timeout context access rejected: {:?}", error))));
                    return;
                },
            };
            protocol.protocol_timeout(Self::new(handle.clone(), window_bits), view.as_mut(), event)
        };
        let result = timeout.await;
        let _ = close::<S>(&handle, result);
    }
}
