use std::ops::Range;
use std::cell::RefCell;
use std::sync::{Weak, Arc};
use std::collections::VecDeque;
use std::net::{Shutdown, SocketAddr};
use std::io::{Result, Error, ErrorKind, Read, Write};

use iovec::IoVec;
use crossbeam_channel::Sender;
use mio::{
    PollOpt, Token, Ready,
    net::TcpStream
};

use crate::util::pause;
use crate::driver::{Socket, Stream, SocketHandle};
use crate::buffer_pool::{ReadableView, WriteBufferHandle, WriteBufferPool};

/*
* Tcp连接读缓冲
*/
struct ReadBuffer {
    buf:            Vec<u8>,            //缓冲区
    need_size:      usize,              //缓冲区需要接收的字节数
    recv_pos:       usize,              //缓冲区接收位置
    read_pos:       usize,              //缓冲区已读位置
    buf_once:       Option<Vec<u8>>,    //临时缓冲区，用于缓存临时的大量数据
    recv_pos_once:  usize,              //临时缓冲区接收位置
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
        }
    }

    //判断缓冲区是否为空
    pub fn is_empty(&self) -> bool {
        (self.recv_pos == 0) && (self.read_pos == 0)
    }

    //为异步读准备读缓冲区
    pub fn ready(&mut self, len: usize) {
        self.need_size = len; //设置需要异步接收的字节数
        if len > self.buf.capacity() {
            //如果需要读取的数据长度大于当前缓冲区的有效容量，则提供指定长度的临时缓冲区，以保证可以异步接收指定有效长度的数据
            let mut buf: Vec<u8> = Vec::with_capacity(len);
            buf.resize(len, 0);

            //将缓冲区内未读的可读数据填充到临时缓冲区的首部，并重置缓冲区
            let readable_len = self.recv_pos - self.read_pos;
            if let Some(readable_buf) = self.window_ref(self.read_pos..self.recv_pos) {
                buf[0..readable_len].copy_from_slice(readable_buf);
                self.recv_pos = 0;
                self.read_pos = 0;
            }

            self.buf_once = Some(buf); //重置临时缓冲区
        } else if (len > 0) && (self.buf.capacity() < (self.read_pos + len)) {
            //如果需要读取指定有效长度的数据，且当前缓冲区空闲容量不足，则清理缓冲区，以保证可以异步接收指定有效长度的数据
            let readable_len = self.recv_pos - self.read_pos;

            //将缓冲区内未读的可读数据填充到缓冲区的首部，并设置缓冲位置
            unsafe {
                self.buf
                    .as_ptr()
                    .wrapping_offset(self.read_pos as isize)
                    .copy_to(self.buf.as_mut_ptr(), readable_len);
                self.recv_pos = readable_len;
                self.read_pos = 0;
            }
        }
    }

    //从缓冲区的已读位置开始读取指定长度的数据
    pub fn read(&mut self, len: usize) -> Option<&[u8]> {
        //同步返回数据
        if self.recv_pos_once > 0 {
            //临时缓冲区有数据
            self.recv_pos_once = 0; //重置临时缓冲区接收位置
            return Some(self.buf_once.as_ref().unwrap().as_slice());
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

    //获取指定范围的可读引用
    fn window_ref(&self, range: Range<usize>) -> Option<&[u8]> {
        if range.is_empty() || self.buf.capacity() < range.end {
            return None;
        }

        Some(&self.buf.as_slice()[range.start..range.end])
    }
}

/*
* Tcp连接写缓冲
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
        }

        shared
    }
}

/*
* Tcp连接
*/
pub struct TcpSocket {
    local:              SocketAddr,                                     //TCP连接本地地址
    remote:             SocketAddr,                                     //TCP连接远端地址
    token:              Option<Token>,                                  //连接令牌
    stream:             TcpStream,                                      //TCP流
    ready:              Ready,                                          //Tcp事件准备状态
    poll_opt:           PollOpt,                                        //Tcp事件轮询选项
    readable_rouser:    Option<Sender<Token>>,                          //可读事件唤醒器
    writable_rouser:    Option<Sender<(Token, WriteBufferHandle)>>,     //可写事件唤醒器
    readable_size:      usize,                                          //本次可读字节数
    read_buf:           Option<ReadBuffer>,                             //读缓冲
    write_buf:          Option<WriteBuffer>,                            //写缓冲
    flush:              bool,                                           //Tcp连接写刷新状态
    closed:             bool,                                           //Tcp连接关闭状态
    buffer_pool:        Option<Arc<WriteBufferPool>>,                   //Tcp连接写缓冲池
    handle:             Option<SocketHandle<TcpSocket>>,                //Tcp连接句柄
}

unsafe impl Send for TcpSocket {}
unsafe impl Sync for TcpSocket {}

impl Stream for TcpSocket {
    fn new(local: &SocketAddr,
           remote: &SocketAddr,
           token: Option<Token>,
           stream: TcpStream) -> Self {
        TcpSocket {
            local: local.clone(),
            remote: remote.clone(),
            token,
            stream: stream,
            ready: Ready::empty(),
            poll_opt: PollOpt::level(), //默认的连接事件轮询选项
            readable_rouser: None,
            writable_rouser: None,
            readable_size: 0,
            read_buf: None,
            write_buf: None,
            flush: false,
            closed: false,
            buffer_pool: None,
            handle: None,
        }
    }

    fn set_handle(&mut self, handle: Weak<RefCell<Self>>) {
        self.handle = Some(SocketHandle::new(handle));
    }

    fn get_stream(&self) -> &TcpStream {
        &self.stream
    }

    fn get_ready(&self) -> &Ready {
        &self.ready
    }

    fn set_ready(&mut self, ready: Ready) {
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

    fn set_readable_rouser(&mut self, rouser: Option<Sender<Token>>) {
        self.readable_rouser = rouser;
    }

    fn set_writable_rouser(&mut self, rouser: Option<Sender<(Token, WriteBufferHandle)>>) {
        self.writable_rouser = rouser;
    }

    fn set_write_buffer(&mut self, buffer: WriteBufferPool) {
        self.buffer_pool = Some(Arc::new(buffer));
    }

    fn recv(&mut self) -> Result<usize> {
        let mut recv_pos = self.read_buf.as_ref().unwrap().recv_pos;
        let readable_size = self.readable_size; //按需接收的字节数
        let need_recv_pos = recv_pos + self.read_buf.as_mut().unwrap().need_size; //本次读缓冲区的已接收需要达到的位置

        loop{
            match self.stream.read(&mut self.read_buf.as_mut().unwrap().buf[recv_pos..]) {
                Ok(0) => {
                    //在流内没有接收到任何数据，因为读缓冲区的容量至少大于0，所以需要继续尝试接收数据
                    pause();
                    continue;
                },
                Ok(len) => {
                    //在流内接收到数据
                    recv_pos += len; //临时接收位置
                    self.read_buf.as_mut().unwrap().recv_pos = recv_pos; //移动读缓冲区的已接收位置
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
                    //在流内接收时错误，则中断本次接收，等待下次完成接收
                    return Err(e);
                }
            }
        }
    }

    fn send(&mut self) -> Result<usize> {
        let mut send_pos = self.write_buf.as_ref().unwrap().send_pos;
        let write_pos = self.write_buf.as_ref().unwrap().write_pos;

        loop {
            //获取本次发送的IoVec
            let mut bufs = Vec::new();
            let mut shared = self.write_buf.as_mut().unwrap().pop();
            if let Some(s) = &shared {
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
            }

            if bufs.len() == 0 {
                //写缓冲区为空，则立即停止本次发送，并取消当前流的可写事件的关注
                self.ready.remove(Ready::writable());
                return Ok(0);
            }

            match self.stream.write_bufs(&bufs[..]) {
                Ok(0) => {
                    //在流内没有发送任何数据，则继续尝试发送数据
                    pause();
                    continue;
                },
                Ok(len) => {
                    //在流内发送数据
                    send_pos += len; //临时发送位置
                    self.write_buf.as_mut().unwrap().send_pos = send_pos; //移动写缓冲区的已发送位置
                    if send_pos < write_pos {
                        //写缓冲区数据还未发送完，则尝试继续发送数据
                        pause();
                        continue;
                    }

                    if self.flush {
                        //刷新流缓冲区，保证数据被立即发送
                        if let Err(e) = self.stream.flush() {
                            println!("!!!> Tcp Stream Flush Failed, reason: {:?}", e);
                        }
                    }

                    //已发送完写缓冲区内的数据，则完成本次发送，并取消当前流的可写事件的关注
                    self.ready.remove(Ready::writable());
                    return Ok(len);
                },
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                    //在流内发送时中断，则继续尝试发送数据
                    pause();
                    continue;
                },
                Err(e) => {
                    //在流内发送时错误，则中断本次发送，等待下次完成发送
                    return Err(e);
                },
            }
        }
    }
}

impl Socket for TcpSocket {
    fn is_closed(&self) -> bool {
        self.closed
    }

    fn is_flush(&self) -> bool {
        self.flush
    }

    fn get_handle(&self) -> SocketHandle<Self> {
        self.handle.as_ref().unwrap().clone()
    }

    fn set_flush(&mut self, flush: bool) {
        self.flush = flush;
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

    fn set_token(&mut self, token: Option<Token>) -> Option<Token> {
        let last = self.token.take();
        self.token = token;
        last
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
        self.read_buf.as_mut().unwrap().ready(size); //为读就绪准备读缓冲区
        self.ready.remove(Ready::writable()); //取消当前连接对可写事件的关注
        self.ready.insert(Ready::readable()); //设置当前连接需要关注可读事件
        self.readable_size = size;
        if let Some(rouser) = &self.readable_rouser {
            if let Some(token) = self.token {
                //唤醒连接，并通知连接需要再接收指定长度的数据
                if let Err(e) = rouser.send(token) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(())
    }

    fn read(&mut self, size: usize) -> Result<Option<&[u8]>> {
        if let Some(r) = self.read_buf.as_mut().unwrap().read(size) {
            //当前缓冲区有未读的指定长度的可读数据，则同步返回
            return Ok(Some(r));
        }

        //当前缓冲区没有未读的指定长度的可读数据，则需要通知连接读就绪，可以异步接收剩余的指定长度的数据
        self.ready.remove(Ready::writable()); //取消当前连接对可写事件的关注
        self.ready.insert(Ready::readable()); //设置当前连接需要关注可读事件
        self.readable_size = size;
        if let Some(rouser) = &self.readable_rouser {
            if let Some(token) = self.token {
                //唤醒连接，并通知连接需要再接收指定长度的数据
                if let Err(e) = rouser.send(token) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(None)
    }

    fn write_ready(&mut self, handle: WriteBufferHandle) -> Result<()> {
        self.ready.remove(Ready::readable()); //取消当前连接对可读事件的关注
        self.ready.insert(Ready::writable()); //设置当前连接需要关注可写事件
        if let Some(rouser) = &self.writable_rouser {
            if let Some(token) = self.token {
                //唤醒连接，并通知连接需要发送数据，必须在异步写入缓冲区成功后，才唤醒
                //因为异步写入缓冲区，且异步唤醒的原因，写缓冲区的数据可能会被上次唤醒所消耗，则出现多余的空唤醒
                //但因为一定是先写入缓冲区完成后再唤醒，所以不会出现写缓冲区有数据，且没唤醒的情况
                if let Err(e) = rouser.send((token, handle)) {
                    return Err(Error::new(ErrorKind::BrokenPipe, e));
                }
            }
        }

        Ok(())
    }

    fn write(&mut self, handle: WriteBufferHandle) {
        if let Some(writer) = &mut self.write_buf {
            writer.push(handle);
        }
    }

    fn close(&self, how: Shutdown) -> Result<()> {
        if self.closed {
            //已关闭，则忽略
            return Ok(());
        }

        self.stream.shutdown(how)
    }
}