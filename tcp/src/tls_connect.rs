use std::sync::Arc;
use std::ops::Range;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::io::{Result, Error, ErrorKind, Read, Write};

use crossbeam_channel::Sender;
use mio::{
    PollOpt, Token, Ready,
    net::TcpStream
};
use rustls::{WriteV, Session, ClientSession, ServerSession};
use log::warn;

use crate::{driver::{Socket, Stream, SocketHandle, SocketWakeup},
            buffer_pool::{ReadableView, WriteBufferHandle, WriteBufferPool},
            util::{pause, SocketReady, SocketContext, SocketEvent, TlsConfig, TlsSession}};
use iovec::IoVec;
use crate::driver::SocketStatus::Readed;

/*
* Tls连接读缓冲
*/
struct ReadBuffer {
    buf:            Vec<u8>,            //缓冲区
    need_size:      usize,              //缓冲区需要接收的字节数
    recv_pos:       usize,              //缓冲区接收位置
    read_pos:       usize,              //缓冲区已读位置
    buf_once:       Option<Vec<u8>>,    //临时缓冲区，用于缓存临时的大量数据
    recv_pos_once:  usize,              //临时缓冲区接收位置
    read_pos_once:  usize,              //临时缓冲区已读位置
}

impl ReadBuffer {
    //构建一个指定容量的读缓冲
    pub fn with_capacity(size: usize) -> Self {
        let mut buf = Vec::with_capacity(size);
        buf.resize(size, 0);

        ReadBuffer {
            buf,
            need_size: 0,
            recv_pos: 0,
            read_pos: 0,
            buf_once: None,
            recv_pos_once: 0,
            read_pos_once: 0,
        }
    }

    //判断缓冲区是否为空
    pub fn is_empty(&self) -> bool {
        self.buf_once.is_none() && (self.recv_pos == 0) && (self.read_pos == 0)
    }

    //判断当前是否使用临时缓冲区
    pub fn used_temp_buf(&self) -> bool {
        if let Some(buf) = &self.buf_once {
            //临时缓冲区存在
            if buf.capacity() > self.recv_pos_once {
                //临时缓冲区还未全部填充
                return true;
            }
        }

        false
    }

    //获取可读数据的长度
    pub fn readable_len(&self) -> usize {
        if let Some(_) = &self.buf_once {
            //当前有临时读缓冲区
            self.recv_pos_once - self.read_pos_once
        } else {
            //当前只有读缓冲区
            self.recv_pos - self.read_pos
        }
    }

    //为异步读准备读缓冲区
    pub fn ready(&mut self, len: usize) {
        if (len > self.buf.capacity()) || (self.buf.capacity() < ((self.recv_pos - self.read_pos) + len)) {
            //如果本次需要读取的数据长度大于当前缓冲区的最大容量，或如果本次需要读取指定有效长度的数据，且未读取数据长度加上本次需要读取的数据长度之和，大于当前缓冲区的最大容量
            //则提供未读取数据长度加上本次需要读取的数据长度之和的临时缓冲区，将未读数据复制到临时缓冲区的头部，以保证未读数据不丢失，且可以异步接收本次指定有效长度的数据
            if self.buf_once.is_some() && (self.recv_pos_once > self.read_pos_once) {
                //当前有临时缓冲区，且临时缓冲区还未填充完成，则只设置需要异步接收的剩余字节数
                let readable_len = self.recv_pos_once - self.read_pos_once;

                if readable_len >= len {
                    self.need_size = 0;
                } else {
                    self.need_size = len - readable_len;
                }
                return;
            }

            //当前没有临时缓冲区
            let mut buf: Vec<u8> = Vec::with_capacity(len);
            buf.resize(len, 0);

            //将缓冲区内未读的可读数据填充到临时缓冲区的首部，并重置缓冲区
            let readable_len = self.recv_pos - self.read_pos;
            if let Some(readable_buf) = self.window_ref(self.read_pos..self.recv_pos) {
                buf[0..readable_len].copy_from_slice(readable_buf);
                self.recv_pos = 0;
                self.read_pos = 0;
            }

            //重置临时缓冲区和缓冲位置
            self.buf_once = Some(buf);
            self.recv_pos_once = readable_len;
            self.read_pos_once = 0;

            //设置需要异步接收的剩余字节数
            if readable_len >= len {
                self.need_size = 0;
            } else {
                self.need_size = len - readable_len;
            }
        } else if (self.buf.capacity() - self.recv_pos) < len {
            //如果本次需要读取指定有效长度的数据，且当前缓冲区空闲容量不足，但清理已读数据后，可以接收本次指定长度的数据，则清理缓冲区，以保证可以异步接收本次指定有效长度的数据
            let readable_len = self.recv_pos - self.read_pos;

            //将缓冲区内未读的可读数据填充到缓冲区的首部，并设置缓冲位置
            unsafe {
                self.buf
                    .as_ptr()
                    .wrapping_offset(self.read_pos as isize)
                    .copy_to(self.buf.as_mut_ptr(), readable_len);
                self.recv_pos = readable_len;
                self.read_pos = 0;

                //设置需要异步接收的剩余字节数
                if readable_len >= len {
                    self.need_size = 0;
                } else {
                    self.need_size = len - readable_len;
                }
            }
        } else {
            //否则本次需要读取的数据长度，对缓冲区没有影响，则只设置需要异步接收的剩余字节数
            let readable_len = self.recv_pos - self.read_pos;

            if readable_len >= len {
                self.need_size = 0;
            } else {
                self.need_size = len - readable_len;
            }
        }
    }

    //从缓冲区的已读位置开始读取指定长度的数据
    pub fn read(&mut self, len: usize) -> Option<&[u8]> {
        //同步返回数据
        if self.recv_pos_once > 0 && (self.recv_pos_once > self.read_pos_once) {
            //临时缓冲区有数据，且未读完
            let recv_pos = self.recv_pos_once;
            let read_pos = self.read_pos_once;

            if (len == 0) && (recv_pos > read_pos) {
                //如果需要读取任意有效长度的数据，且当前临时缓冲区内有可读数据
                self.read_pos_once += recv_pos - read_pos; //更新已读位置
                return self.window_ref_once(read_pos..recv_pos);
            } else if (len > 0) && (len <= (recv_pos - read_pos)) {
                //如果需要读取指定有效长度的数据，且当前临时缓冲区内至少有指定有效长度的可读数据
                self.read_pos_once += len; //更新已读位置
                return self.window_ref_once(read_pos..read_pos + len);
            }
        } else {
            //缓冲区有数据
            let recv_pos = self.recv_pos;
            let read_pos = self.read_pos;

            if (len == 0) && (recv_pos > read_pos) {
                //如果需要读取任意有效长度的数据，且当前缓冲区内有可读数据
                self.read_pos += recv_pos - read_pos; //更新已读位置
                return self.window_ref(read_pos..recv_pos);
            } else if (len > 0) && (len <= (recv_pos - read_pos)) {
                //如果需要读取指定有效长度的数据，且当前缓冲区内至少有指定有效长度的可读数据
                self.read_pos += len; //更新已读位置
                return self.window_ref(read_pos..read_pos + len);
            }
        }

        //准备异步接收数据
        self.ready(len);

        None
    }

    //获取指定范围的临时缓冲区的可读引用
    fn window_ref_once(&self, range: Range<usize>) -> Option<&[u8]> {
        if let Some(buf) = &self.buf_once {
            if range.is_empty() || buf.capacity() < range.end {
                return None;
            }

            Some(&buf[range.start..range.end])
        } else {
            None
        }
    }

    //获取指定范围的缓冲区的可读引用
    fn window_ref(&self, range: Range<usize>) -> Option<&[u8]> {
        if range.is_empty() || self.buf.capacity() < range.end {
            return None;
        }

        Some(&self.buf.as_slice()[range.start..range.end])
    }
}

/*
* Tls连接写缓冲
*/
struct WriteBuffer {
    queue:      VecDeque<WriteBufferHandle>,    //缓冲区队列
    handle:     Option<WriteBufferHandle>,      //当前待发送缓冲区
    write_pos:  usize,                          //缓冲区已写位置
    send_pos:   usize,                          //缓冲区已发送位置
}

impl WriteBuffer {
    //构建一个指定容量的写缓冲
    pub fn new() -> Self {
        WriteBuffer {
            queue: VecDeque::with_capacity(3),
            handle: None,
            write_pos: 0,
            send_pos: 0,
        }
    }

    //待发送的写缓冲数量
    pub fn size(&self) -> usize {
        self.queue.len()
    }

    //增加一个待写入的写缓冲句柄
    pub fn push(&mut self, handle: WriteBufferHandle) {
        self.queue.push_back(handle);
    }

    //线程安全的获取当前写缓冲
    pub fn pop(&mut self) -> Option<Arc<ReadableView>> {
        let mut shared = None;
        if self.send_pos >= self.write_pos {
            //当前写缓冲区的数据已发送完，则取出下一个写缓冲区的数据
            if let Some(handle) = self.queue.pop_front() {
                shared = handle.get_shared();
                self.handle = Some(handle);
            }

            //设置当前写缓冲区的位置，并填充写缓冲区
            if let Some(s) = &shared {
                self.write_pos = s.get_iovec().iter().map(|vec| { (*vec).len() }).sum();
                self.send_pos = 0;
            }
        } else {
            if let Some(handle) = &self.handle {
                //当前写缓冲区的数据未发送完，则返回当前未发送完的写缓冲区数据
                shared = handle.get_shared();
            }
        }

        shared
    }

    //移除当前写缓冲
    pub fn remove(&mut self) {
        if let Some(_) = self.handle.take() {
            ();
        }
    }
}

/*
* Tls连接批量写适配器
*/
struct WriteVAdapter<'a>(&'a mut TcpStream);

impl<'a> WriteV for WriteVAdapter<'a> {
    fn writev(&mut self, bytes: &[&[u8]]) -> Result<usize> {
        let mut bufs = Vec::with_capacity(bytes.len());
        for byte in bytes {
            if let Some(iovec) = IoVec::from_bytes(byte) {
                bufs.push(iovec);
            }
        }

        self.0.write_bufs(bufs.as_slice())
    }
}

impl<'a> WriteVAdapter<'a> {
    //构建指定Tcp流的Tls连接批量写适配器
    pub fn new(stream: &'a mut TcpStream) -> Self {
        WriteVAdapter(stream)
    }
}

/*
* Tls连接
*/
pub struct TlsSocket {
    local:          SocketAddr,                                             //Tls连接本地地址
    remote:         SocketAddr,                                             //Tls连接远端地址
    token:          Option<Token>,                                          //连接令牌
    uid:            Option<usize>,                                          //连接唯一id
    stream:         TcpStream,                                              //TCP流
    ready:          SocketReady,                                            //Tls事件准备状态
    poll_opt:       PollOpt,                                                //Tls事件轮询选项
    rouser:         Option<Sender<(Token, SocketWakeup)>>,                  //事件唤醒器
    close_listener: Option<Sender<(Token, Result<()>)>>,                    //关闭事件监听器
    timer_listener: Option<Sender<(Token, Option<(usize, SocketEvent)>)>>,  //定时事件监听器
    readable_size:  usize,                                                  //本次可读字节数
    read_buf:       Option<ReadBuffer>,                                     //读缓冲
    write_buf:      Option<WriteBuffer>,                                    //写缓冲
    flush:          Arc<AtomicBool>,                                        //Tls连接写刷新状态
    closed:         Arc<AtomicBool>,                                        //Tls连接关闭状态
    buffer_pool:    Option<Arc<WriteBufferPool>>,                           //Tls连接写缓冲池
    handle:         Option<SocketHandle<TlsSocket>>,                        //Tls连接句柄
    context:        SocketContext,                                          //Tls连接上下文
    timer:          Option<usize>,                                          //定时器句柄
    tls_session:    TlsSession,                                             //传输层安全会话
}

unsafe impl Send for TlsSocket {}
unsafe impl Sync for TlsSocket {}

impl Stream for TlsSocket {
    fn new(local: &SocketAddr,
           remote: &SocketAddr,
           token: Option<Token>,
           stream: TcpStream,
           tls_cfg: TlsConfig) -> Self {
        let tls_session = match tls_cfg {
            TlsConfig::Client(cfg) => {
                unimplemented!();
            },
            TlsConfig::Server(cfg) => {
                TlsSession::Server(ServerSession::new(&cfg))
            },
            TlsConfig::Empty => panic!("create tls socket failed, invalid tls config"),
        };

        TlsSocket {
            local: local.clone(),
            remote: remote.clone(),
            token,
            uid: None,
            stream: stream,
            ready: SocketReady::empty(),
            poll_opt: PollOpt::level(), //默认的连接事件轮询选项
            rouser: None,
            close_listener: None,
            timer_listener: None,
            readable_size: 0,
            read_buf: None,
            write_buf: None,
            flush: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            buffer_pool: None,
            handle: None,
            context: SocketContext::empty(),
            timer: None,
            tls_session,
        }
    }

    fn set_handle(&mut self, shared: &Arc<RefCell<Self>>) {
        self.handle = Some(SocketHandle::new(shared));
    }

    fn get_stream(&self) -> &TcpStream {
        &self.stream
    }

    fn set_token(&mut self, token: Option<Token>) -> Option<Token> {
        let last = self.token.take();
        self.token = token;
        last
    }

    fn set_uid(&mut self, uid: usize) -> Option<usize> {
        let last = self.uid.take();
        self.uid = Some(uid);
        last
    }

    fn get_ready(&self) -> Ready {
        self.ready.get()
    }

    fn set_ready(&self, ready: Ready) {
        self.ready.insert(ready);
    }

    fn unset_ready(&mut self, ready: Ready) {
        self.ready.remove(ready);
    }

    fn get_poll_opt(&self) -> &PollOpt {
        &self.poll_opt
    }

    fn set_poll_opt(&mut self, opt: PollOpt) {
        self.poll_opt.insert(opt);
    }

    fn unset_poll_opt(&mut self, opt: PollOpt) {
        self.poll_opt.remove(opt);
    }

    fn set_rouser(&mut self, rouser: Option<Sender<(Token, SocketWakeup)>>) {
        self.rouser = rouser;
    }

    fn set_close_listener(&mut self, listener: Option<Sender<(Token, Result<()>)>>) {
        self.close_listener = listener;
    }

    fn set_timer_listener(&mut self, listener: Option<Sender<(Token, Option<(usize, SocketEvent)>)>>) {
        self.timer_listener = listener;
    }

    fn set_timer_handle(&mut self, timer: usize) -> Option<usize> {
        let handle = self.unset_timer_handle();
        self.timer = Some(timer);
        handle
    }

    fn unset_timer_handle(&mut self) -> Option<usize> {
        self.timer.take()
    }

    fn set_write_buffer(&mut self, buffer: WriteBufferPool) {
        self.buffer_pool = Some(Arc::new(buffer));
    }

    //连接关闭状态不阻止接收
    fn recv(&mut self) -> Result<usize> {
        let mut recv_pos;
        let used_temp_buf = self.read_buf.as_ref().unwrap().used_temp_buf();
        if used_temp_buf {
            //当前需要接收数据并填充临时缓冲区，则获取当前临时缓冲区的接收位置
            recv_pos = self.read_buf.as_mut().unwrap().recv_pos_once;
        } else {
            //当前需要接收数据并填充缓冲区，则获取当前缓冲区的接收位置
            recv_pos = self.read_buf.as_mut().unwrap().recv_pos;
        }
        let readable_size = self.readable_size; //按需接收的字节数
        let need_recv_pos = recv_pos + self.read_buf.as_mut().unwrap().need_size; //本次读缓冲区的已接收需要达到的位置

        loop{
            match self.tls_recv(used_temp_buf, recv_pos) {
                Ok(0) => {
                    //从Tls会话读缓冲区内未读取到数据，表示当前Tls会话正在握手中，则设置Tls会话的就绪状态，并立即返回，以保证继续握手
                    self.ready.remove(Ready::readable());
                    self.tls_set_ready();
                    return Ok(0);
                },
                Ok(len) => {
                    //当前Tls会话已握手，在流内接收到数据
                    recv_pos += len; //临时接收位置
                    if used_temp_buf {
                        //移动临时缓冲区的已接收位置
                        self.read_buf.as_mut().unwrap().recv_pos_once = recv_pos;
                    } else {
                        //移动缓冲区的已接收位置
                        self.read_buf.as_mut().unwrap().recv_pos = recv_pos;
                    }

                    if self.read_buf.as_mut().unwrap().need_size <= len {
                        //如果本次接收的字节数大于等于读缓冲区需要接收的字节数，则重置读缓冲区需要接收的字节数
                        self.read_buf.as_mut().unwrap().need_size = 0;
                    } else {
                        //如果本次接收的字节数小于读缓冲区需要接收的字节数，则从读缓冲区需要接收的字节数中减去本次已接收的字节数
                        self.read_buf.as_mut().unwrap().need_size -= len;
                    }

                    if (readable_size > 0) && (recv_pos < need_recv_pos) {
                        //如果当前已接收字节数，未达到需要接收字节数，则继续尝试接收数据
                        pause();
                        continue;
                    }

                    //已接收足够的数据，则完成本次接收，并取消对当前流的可读事件的关注
                    self.ready.remove(Ready::readable());
                    return Ok(len);
                },
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                    //在流内接收时中断，则继续尝试接收数据
                    pause();
                    continue;
                },
                Err(e) => {
                    //在流内接收时错误，则中断本次接收
                    return Err(e);
                }
            }
        }
    }

    //连接关闭状态不阻止发送
    fn send(&mut self) -> Result<usize> {
        loop {
            //获取本次发送的IoVec
            let mut bufs = Vec::new();
            let mut shared = self.write_buf.as_mut().unwrap().pop();

            //获取当前写缓冲的发送和写入位置
            let mut send_pos = self.write_buf.as_ref().unwrap().send_pos;
            let write_pos = self.write_buf.as_ref().unwrap().write_pos;

            if let Some(s) = &shared {
                //当前连接写缓冲区内有待发送数据，则准备发送
                let mut len;
                let mut pos = send_pos;
                for buf in s.get_iovec() {
                    len = buf.len(); //当前缓冲区的大小
                    if pos > len {
                        //已覆盖当前缓冲区，则继续下一个缓冲区
                        pos -= len;
                        continue;
                    }

                    bufs.push(buf); //加入缓冲列表
                    pos = 0; //将位置设置为0，保证将后续缓冲区全部加入缓冲列表
                }

                //将数据写入Tls会话缓冲区
                match self.tls_write(&bufs[..]) {
                    Ok(len) => {
                        //在Tls会话内写入数据
                        send_pos += len; //临时发送位置
                        self.write_buf.as_mut().unwrap().send_pos = send_pos; //移动写缓冲区的已发送位置
                        if send_pos < write_pos {
                            //写缓冲区数据还未全部写入Tls会话缓冲区，则尝试继续写入数据
                            pause();
                            continue;
                        }

                        //已发送完当前写缓冲区内的数据，则完成本次发送，清理当前写缓冲句柄，并继续写入当前连接写缓冲区内下一个写缓冲句柄
                        self.write_buf.as_mut().unwrap().remove();
                        continue;
                    },
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                        //在Tls会话内写入中断，则继续尝试写入数据
                        pause();
                        continue;
                    },
                    Err(e) => {
                        //在在Tls会话内写入错误，则中断本次写入
                        return Err(e);
                    },
                }
            }

            //发送Tls会话写缓冲区内的数据
            match self.tls_send() {
                Ok(len) => {
                    self.ready.remove(Ready::writable());
                    if send_pos == 0 {
                        //当前Tls会话正在握手中，则设置Tls会话的就绪状态，并立即返回，以保证继续握手
                        self.tls_set_ready();
                    }

                    return Ok(send_pos); //返回Tls会话写缓冲区的待写数据长度
                },
                reason => {
                    return reason;
                }
            }
        }
    }
}

impl Socket for TlsSocket {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn is_flush(&self) -> bool {
        self.flush.load(Ordering::SeqCst)
    }

    fn get_handle(&self) -> SocketHandle<Self> {
        self.handle.as_ref().unwrap().clone()
    }

    fn set_flush(&self, flush: bool) {
        self.flush.store(flush, Ordering::SeqCst);
    }

    fn get_local(&self) -> &SocketAddr {
        &self.local
    }

    fn get_remote(&self) -> &SocketAddr {
        &self.remote
    }

    fn get_token(&self) -> Option<&Token> {
        self.token.as_ref()
    }

    fn get_uid(&self) -> Option<&usize> {
        self.uid.as_ref()
    }

    fn get_context(&self) -> &SocketContext {
        &self.context
    }

    fn get_context_mut(&mut self) -> &mut SocketContext {
        &mut self.context
    }

    fn set_timeout(&self, timeout: usize, event: SocketEvent) {
        if let Some(listener) = &self.timer_listener {
            if let Some(token) = self.token {
                listener.send((token, Some((timeout, event))));
            }
        }
    }

    fn unset_timeout(&self) {
        if let Some(listener) = &self.timer_listener {
            if let Some(token) = self.token {
                listener.send((token, None));
            }
        }
    }

    fn init_buffer_capacity(&mut self, read_size: usize, _write_size: usize) {
        if self.read_buf.is_none() {
            self.read_buf = Some(ReadBuffer::with_capacity(read_size));
        }

        if self.write_buf.is_none() {
            self.write_buf = Some(WriteBuffer::new())
        }
    }

    fn get_write_buffer(&self) -> &WriteBufferPool {
        self.buffer_pool.as_ref().unwrap().as_ref()
    }

    fn read_ready(&mut self, size: usize) -> Result<()> {
        if self.is_closed() {
            //连接已关闭，则返回错误
            return Err(Error::new(ErrorKind::BrokenPipe, "socket closed"));
        }

        if let Some(buf) = self.read_buf.as_mut() {
            let readable_len = buf.readable_len();
            if readable_len >= size {
                //如果当前读缓冲区的未读数据足够，表示读就绪，异步唤醒连接继续执行已读回调
                if let Some(rouser) = &self.rouser {
                    if let Some(token) = self.token {
                        //唤醒连接，并通知连接需要再接收指定长度的数据
                        if let Err(e) = rouser.send((token, SocketWakeup::Read(false))) {
                            return Err(Error::new(ErrorKind::BrokenPipe, e));
                        }
                    }
                }
            } else {
                //如果当前读缓冲区的未读数据不足，则设置本次实际需要读的字节数，并异步唤醒连接准备继续接收
                if size == 0 {
                    self.readable_size = 0;
                } else {
                    self.readable_size = size - readable_len;
                }

                buf.ready(self.readable_size); //为读就绪准备读缓冲区
                if let Some(rouser) = &self.rouser {
                    if let Some(token) = self.token {
                        //唤醒连接，并通知连接需要再接收指定长度的数据
                        if let Err(e) = rouser.send((token, SocketWakeup::Read(true))) {
                            return Err(Error::new(ErrorKind::BrokenPipe, e));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn read(&mut self, size: usize) -> Result<Option<&[u8]>> {
        if self.is_closed() {
            //连接已关闭，则返回错误
            return Err(Error::new(ErrorKind::BrokenPipe, "socket closed"));
        }

        let readable_len = self.read_buf.as_ref().unwrap().readable_len();
        if let Some(r) = self.read_buf.as_mut().unwrap().read(size) {
            //当前缓冲区有未读的指定长度的可读数据，则同步返回
            return Ok(Some(r));
        }

        //当前缓冲区没有未读的指定长度的可读数据，需要通知连接读就绪，可以异步接收剩余的指定长度的数据
        if size == 0 {
            self.readable_size = 0;
        } else {
            self.readable_size = size - readable_len;
        }

        if let Some(rouser) = &self.rouser {
            if let Some(token) = self.token {
                //唤醒连接，并通知连接需要再接收指定长度的数据
                if let Err(e) = rouser.send((token, SocketWakeup::Read(true))) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(None)
    }

    fn write_ready(&self, handle: WriteBufferHandle) -> Result<()> {
        if self.is_closed() {
            //连接已关闭，则返回错误
            return Err(Error::new(ErrorKind::BrokenPipe, "socket closed"));
        }

        if let Some(rouser) = &self.rouser {
            if let Some(token) = self.token {
                //唤醒连接，并通知连接需要发送数据
                //因为异步写入缓冲区，且异步唤醒的原因，写缓冲区的数据可能会被上次唤醒所消耗，则出现多余的空唤醒
                //但因为一定是先写入缓冲区完成后再唤醒，所以不会出现写缓冲区有数据，且没唤醒的情况
                if let Err(e) = rouser.send((token, SocketWakeup::Write(handle))) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(())
    }

    //连接关闭不阻止写入
    fn write(&mut self, handle: WriteBufferHandle) {
        if let Some(writer) = &mut self.write_buf {
            writer.push(handle);
        }
    }

    fn close(&self, reason: Result<()>) -> Result<()> {
        //更新连接状态为已关闭
        if self.closed.compare_and_swap(false, true, Ordering::SeqCst) {
            //当前已关闭，则忽略
            return Ok(());
        }

        //通知连接关闭
        if let Some(listener) = &self.close_listener {
            if let Some(token) = self.token {
                if let Err(e) = listener.send((token, reason)) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(())
    }
}

impl TlsSocket {
    //根据Tls会话的就绪状态，设置Tcp流的就绪状态
    fn tls_set_ready(&self) {
        match &self.tls_session {
            TlsSession::Client(session) => {
                unimplemented!();
            },
            TlsSession::Server(session) => {
                let r = session.wants_read();
                let w = session.wants_write();
                if r && w {
                    self.ready.insert(Ready::readable() | Ready::writable());
                } else if r {
                    self.ready.insert(Ready::readable());
                } else if w {
                    self.ready.insert(Ready::writable());
                }
            },
        }
    }

    //从Tls会话中接收数据
    fn tls_recv(&mut self, used_temp_buf: bool, recv_pos: usize) -> Result<usize> {
        match &mut self.tls_session {
            TlsSession::Client(session) => {
                unimplemented!();
            },
            TlsSession::Server(session) => {
                match session.read_tls(&mut self.stream) {
                    Ok(0) => {
                        //从Tcp流中无法读取Tls数据，表示连接已关闭或EOF
                        Err(Error::new(ErrorKind::Other, "tls session closed"))
                    },
                    Ok(_) => {
                        //从Tcp流中读取到Tls数据，则处理Tls数据帧
                        if let Err(e) = session.process_new_packets() {
                            //处理Tls数据帧错误，则立即返回错误原因
                            return Err(Error::new(ErrorKind::InvalidInput, e));
                        }

                        //从Tls会话的读缓冲区中读取已处理的数据
                        if used_temp_buf {
                            //当前使用临时读缓冲区
                            if let Some(buf) = &mut self.read_buf.as_mut().unwrap().buf_once {
                                //临时读缓冲区存在
                                session.read(&mut buf[recv_pos..])
                            } else {
                                Err(Error::new(ErrorKind::Other, "invalid temp buffer"))
                            }
                        } else {
                            //当前使用读缓冲区
                            session.read(&mut self.read_buf.as_mut().unwrap().buf[recv_pos..])
                        }
                    },
                    Err(e) if (&e).kind() == ErrorKind::WouldBlock => {
                        //从Tcp流中读取Tls数据阻塞，则尝试从Tls会话的读缓冲区中读取已处理的数据
                        if used_temp_buf {
                            //当前使用临时读缓冲区
                            if let Some(buf) = &mut self.read_buf.as_mut().unwrap().buf_once {
                                //临时读缓冲区存在
                                match session.read(&mut buf[recv_pos..]) {
                                    Err(e_) => {
                                        //从Tls会话的读缓冲区中读取已处理的数据错误，则立即返回错误原因
                                        Err(e_)
                                    },
                                    Ok(0) => {
                                        //Tls会话的读缓冲区没有数据，则返回从Tcp流中读取Tls数据阻塞
                                        Err(e)
                                    },
                                    result => result, //返回从Tls会话的读缓冲区中读取已处理数据的长度
                                }
                            } else {
                                Err(Error::new(ErrorKind::Other, "invalid temp buffer"))
                            }
                        } else {
                            //当前使用读缓冲区
                            match session.read(&mut self.read_buf.as_mut().unwrap().buf[recv_pos..]) {
                                Err(e_) => {
                                    //从Tls会话的读缓冲区中读取已处理的数据错误，则立即返回错误原因
                                    Err(e_)
                                },
                                Ok(0) => {
                                    //Tls会话的读缓冲区没有数据，则返回从Tcp流中读取Tls数据阻塞
                                    Err(e)
                                },
                                result => result, //返回从Tls会话的读缓冲区中读取已处理数据的长度
                            }
                        }
                    },
                    reason => reason,
                }
            },
        }
    }

    //向Tls会话写入数据
    fn tls_write(&mut self, bufs: &[&IoVec]) -> Result<usize> {
        match &mut self.tls_session {
            TlsSession::Client(session) => {
                unimplemented!();
            },
            TlsSession::Server(session) => {
                let mut total_len = 0; //写入成功的明文数据长度

                #[cfg(any(windows))]
                {
                    for buf in bufs {
                        if let Err(e) = session.write_all(buf) {
                            //写部分数据失败，则立即返回错误原因
                            return Err(e);
                        }

                        //写部分数据成功，则继续写剩余数据
                        total_len += buf.len();
                    }
                }
                #[cfg(any(unix))]
                {
                    for buf in bufs {
                        if let Err(e) = session.write_all(buf) {
                            //写部分数据失败，则立即返回错误原因
                            return Err(e);
                        }

                        //写部分数据成功，则继续写剩余数据
                        total_len += buf.len();
                    }
                }

                Ok(total_len)
            },
        }
    }

    //从Tls会话中发送数据
    fn tls_send(&mut self) -> Result<usize> {
        match &mut self.tls_session {
            TlsSession::Client(session) => {
                unimplemented!();
            },
            TlsSession::Server(session) => {
                #[cfg(any(windows))]
                {
                    //发送Tls数据帧
                    let mut total_len = 0;
                    loop {
                        match session.write_tls(&mut self.stream) {
                            Ok(0) => {
                                //发送完成，则返回
                                return Ok(total_len);
                            },
                            Ok(len) => {
                                if self.flush.load(Ordering::Relaxed) {
                                    //刷新流缓冲区，保证数据被立即发送
                                    if let Err(e) = session.flush() {
                                        warn!("!!!> Tls Stream Flush Failed, reason: {:?}", e);
                                    }
                                }

                                total_len += len;
                            },
                            reason => {
                                return reason;
                            },
                        }
                    }
                }
                #[cfg(any(unix))]
                {
                    //发送Tls数据帧
                    match session.writev_tls(&mut WriteVAdapter::new(&mut self.stream)) {
                        Ok(len) => {
                            if self.is_flush() {
                                //刷新流缓冲区，保证数据被立即发送
                                if let Err(e) = session.flush() {
                                    warn!("!!!> Tls Stream Flush Failed, reason: {:?}", e);
                                }
                            }

                            if !session.wants_write() {
                                //还没有发送完成，则立即返回发送被阻塞
                                return Err(Error::new(ErrorKind::WouldBlock, format!("tls send would block, len: {}", len)));
                            }

                            Ok(len)
                        },
                        reason => reason,
                    }
                }
            },
        }
    }
}
