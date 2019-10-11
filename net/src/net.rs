use std::ops::Range;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::io::{Error, ErrorKind, Read, Result, Write, Cursor};
use std::time::{Duration, Instant};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::net::{Shutdown, IpAddr, Ipv4Addr, SocketAddr};
use std::thread;

use mio::{Events, Poll, PollOpt, Ready, Token};
use mio::net::{TcpListener, TcpStream};
use timer::{NetTimer, NetTimers, TimerCallback};
use ws::{read_header as http_read_header, read_ws_header as ws_read_header, get_send_buf};

use gray::GrayVersion;
use websocket::OwnedMessage;
use websocket::server::upgrade::sync::Upgrade;

use slab::Slab;
use apm::common::register_server_port;
use lib_timer::{TIMER, FuncRuner};

use api::WSControlType;
use data::{CloseFn, Config, ListenerFn, NetData, NetCounter, NetHandler, Protocol, RecvFn, SendClosureFn,
           RawSocket, State, RawStream, Websocket};

use now_millis;

const MAX_RECV_SIZE: usize = 16 * 1024;

//绑定tcp连接
fn bind_tcp(handler: &mut NetHandler, config: Config, func: ListenerFn) {
    if let Ok(lisn) = TcpListener::bind(&config.addr) {
        let entry = handler.slab.vacant_entry();
        let key = entry.key();

        println!(
            "bind_tcp listener {:?}: register, interest = {:?}",
            key,
            Ready::readable()
        );

        handler
            .poll
            .register(&lisn, Token(key), Ready::readable(), PollOpt::level())
            .unwrap();

        let data = NetData::TcpServer(func, lisn);
        entry.insert(data);

        //注册服务器端口信息
        handler.port = config.addr.port();
        register_server_port(config.addr);

        handler.counter = Some(NetCounter::new(config.addr.clone()));
    }
}

//处理绑定tcp连接
pub fn handle_bind(handler: &mut NetHandler, config: Config, func: ListenerFn) {
    match config.protocol {
        Protocol::TCP => bind_tcp(handler, config, func),
    }
}

//建立tcp连接
pub fn connect_tcp(handler: &mut NetHandler, config: Config, func: ListenerFn) {
    if let Ok(s) = TcpStream::connect(&config.addr) {
        let entry = handler.slab.vacant_entry();
        let key = entry.key();

        s.set_recv_buffer_size(MAX_RECV_SIZE).unwrap();

        let mut stream = RawStream::new(key, handler.recv_comings.clone(), handler.net_timers.clone());
        stream.interest.insert(Ready::writable());

        println!(
            "connect_tcp stream {:?}: register, interest = {:?}",
            stream.token, stream.interest
        );

        handler
            .poll
            .register(&s, Token(key), stream.interest, PollOpt::level())
            .unwrap();

        let stream = Arc::new(RwLock::new(stream));

        let local_addr = s.local_addr().unwrap();

        let data = NetData::TcpStream(stream.clone(), s);
        entry.insert(data);

        let socket = RawSocket::new(key, local_addr.clone(), None, handler.sender.clone());

        let param1 = Ok((socket, stream));
        let param2 = Ok(local_addr);
        func(param1, param2);
    }
}

//处理tcp连接
pub fn handle_connect(handler: &mut NetHandler, config: Config, func: ListenerFn) {
    match config.protocol {
        Protocol::TCP => connect_tcp(handler, config, func),
    }
}

// return bool to indicate that should close the stream
fn send_tcp(poll: &mut Poll, stream: &mut RawStream, mio: &mut TcpStream, v8: Arc<Vec<u8>>) -> bool {
    return match State::from_usize(stream.socket.as_ref().expect("socket not exist").state.load(Ordering::SeqCst)) {
        State::Closed => panic!("error, send_tcp must not to closed state!"),
        State::WouldClose => {
            // can't add sending data any longer
            true
        }
        State::Run => {
            if stream.send_remain_size + v8.len() > stream.max_send_size {
                true
            } else {
                if stream.send_remain_size == 0 {
                    stream.interest.insert(Ready::writable());
                    // println!(
                    //     "send_tcp stream {:?}: reregister, interest = {:?}",
                    //     stream.token, stream.interest
                    // );
                    poll.reregister(mio, stream.token, stream.interest, PollOpt::level())
                        .unwrap();
                }

                stream.send_remain_size += v8.len();
                stream.send_bufs.push_back(v8);
                false
            }
        }
    };
}

//处理tcp发送
pub fn handle_send(handler: &mut NetHandler, socket: usize, v8: Arc<Vec<u8>>) {
    let mut should_close = false;
    if let Some(data) = handler.slab.get_mut(socket) {
        should_close = match data {
            &mut NetData::TcpStream(ref s, ref mut mio) => {
                let len = v8.len();
                let mut s_data = s.write().unwrap();
                let r = send_tcp(&mut handler.poll, &mut s_data, mio, v8);

                handler.counter.as_ref().unwrap().output_byte.sum(len);

                r
            }
            _ => panic!("handle_send failed, NetData's error"),
        }
    }

    if should_close {
        handle_close(handler, socket, true);
    }
}

//关闭tcp连接
fn close_tcp(poll: &mut Poll, stream: &mut RawStream, mio: &mut TcpStream, force: bool) {
    if force || stream.send_remain_size == 0 {
        stream.socket.as_ref().expect("socket not exist").state.swap(State::Closed as usize, Ordering::SeqCst);
        stream.interest = Ready::empty();
        poll.deregister(mio).unwrap();
        println!("close_tcp stream deregister {:?}, shutdown!!!!!!!!!!", stream.token);
        if let Err(e) = mio.shutdown(Shutdown::Both) {
            println!("close tcp stream err, e: {:?}", e);
        }
    } else {
        stream.socket.as_ref()
                    .expect("socket not exist")
                    .state.compare_exchange(State::Run as usize, State::WouldClose as usize, Ordering::SeqCst, Ordering::Acquire);
    }
}

//处理关闭tcp连接
pub fn handle_close(handler: &mut NetHandler, socket: usize, force: bool) {
    if let Some(data) = handler.slab.get_mut(socket) {
        match data {
            &mut NetData::TcpServer(_, _) => panic!("invalid close: TcpServer "),
            &mut NetData::TcpStream(ref mut s, ref mut mio) => {
                close_tcp(&mut handler.poll, &mut s.write().unwrap(), mio, force);
                if let Some(func) = s.write().unwrap().close_callback.take() {
                    func(socket, Ok(()));
                }
                handler.counter.as_ref().unwrap().closed_count.sum(1);
            }
        }
    }

    if force {
        if handler.slab.contains(socket) {
            handler.slab.remove(socket);
        }
        
    }
}

//接受连接请求，建立tcp流，并为tcp流申请缓冲区
fn tcp_event(mio: &mut TcpListener, recv_comings: Arc<RwLock<Vec<Token>>>, net_timers: Arc<RwLock<NetTimers<TimerCallback>>>) -> (NetData, SocketAddr) {
    let (tcp_stream, addr) = mio.accept().unwrap();
    let s = RawStream::new(0, recv_comings, net_timers);
    tcp_stream.set_recv_buffer_size(MAX_RECV_SIZE).unwrap();
    (NetData::TcpStream(Arc::new(RwLock::new(s)), tcp_stream), addr)
}

//从tcp流上接收数据
fn stream_recv(stream: &mut RawStream, mio: &mut TcpStream, counter: &NetCounter) -> Option<Result<(RecvFn, Range<usize>)>> {
    if stream.recv_callback.is_none() {
        panic!("stream_recv failed, stream.recv_callback == None");
    }

    let mut r: Option<Result<(RecvFn, Range<usize>)>> = None;
    if stream.temp_recv_buf.is_none() {
        let would_block = loop {
            if stream.recv_size == 0 {
                let begin = stream.recv_buf_offset;
                match mio.read(&mut stream.recv_buf[begin..]) {
                    Ok(0) => {
                        break Ok(0);
                    },
                    Ok(size) => {
                        counter.input_byte.sum(size);

                        stream.recv_buf_offset += size;
                        break Ok(size);
                    }
                    Err(err) => {
                        if let ErrorKind::WouldBlock = err.kind() {
                            println!(
                                "{:?} recv wouldblock0, offset = {}",
                                stream.token, stream.recv_buf_offset
                            );
                            break Ok(0);
                        }
                        break Err(err);
                    }
                }
            } else {
                let begin = stream.recv_buf_offset;
                let end = stream.recv_callback_offset + stream.recv_size;
                match mio.read(&mut stream.recv_buf[begin..]) {
                    Ok(size) => {
                        if size == 0 {
                             break Ok(0);
                        }

                        counter.input_byte.sum(size);

                        stream.recv_buf_offset += size;
                        if stream.recv_buf_offset >= end {
                            break Ok(stream.recv_size);
                        }
                    }
                    Err(err) => {
                        if let ErrorKind::WouldBlock = err.kind() {
                            // println!(
                            //     "{:?} recv wouldblock1, offset = {}",
                            //     stream.token, stream.recv_buf_offset
                            // );
                            break Ok(0);
                        }
                        break Err(err);
                    }
                }
            }
            
        };

        match would_block {
            Ok(size) => {
                if stream.recv_size == 0 && stream.recv_buf_offset > stream.recv_callback_offset {
                    let start = stream.recv_callback_offset;
                    let end = stream.recv_buf_offset;
                    let func = stream.recv_callback.take().unwrap();
                    r = Some(Ok((func, start..end)));
                    
                    let size2 = end - start;
                    stream.recv_callback_offset += size2;
                } else {
                    if size == stream.recv_size && stream.recv_size != 0 {
                        let start = stream.recv_callback_offset;
                        let end = start + stream.recv_size;
                        let func = stream.recv_callback.take().unwrap();
                        r = Some(Ok((func, start..end)));

                        stream.recv_callback_offset += stream.recv_size;
                    }
                }
                
            }
            Err(err) => {
                r = Some(Err(err));
            }
        }
    } else {
        let would_block = loop {
            let mut offset = &mut stream.temp_recv_buf_offset;
            let mut buf = stream.temp_recv_buf.as_mut().unwrap();

            if *offset == buf.len() {
                panic!("Error, stream_recv: *offset == buf.len()");
            }

            match mio.read(&mut buf[*offset..]) {
                Ok(size) => {
                    *offset += size;
                    if *offset == buf.len() {
                        break Ok(());
                    }
                }
                Err(err) => {
                    if let ErrorKind::WouldBlock = err.kind() {
                        println!("{:?} recv wouldblock, offset = {}", stream.token, *offset);
                        break Ok(());
                    }
                    break Err(err);
                }
            }
        };

        match would_block {
            Ok(_) => {
                let buf = stream.temp_recv_buf.as_mut().unwrap();
                if stream.temp_recv_buf_offset == buf.len() {
                    let func = stream.recv_callback.take().unwrap();
                    r = Some(Ok((func, 0..buf.len())));
                }
            }
            Err(err) => {
                r = Some(Err(err));
            }
        }
    }
    return r;
}

//从tcp流上发送数据
fn stream_send(poll: &mut Poll, stream: &mut RawStream, mio: &mut TcpStream) -> bool {
    loop {
        let buf = stream.send_bufs.pop_front();
        if None == buf {
            mio.flush().unwrap();
            stream.interest.remove(Ready::writable());

            // println!(
            //     "stream_send {:?}: reregister, interest = {:?}",
            //     stream.token, stream.interest
            // );

            poll.reregister(mio, stream.token, stream.interest, PollOpt::level())
                .unwrap();
            break;
        }
        let buf = buf.unwrap();
        match mio.write(&buf[stream.send_buf_offset..]) {
            Ok(size) => {
                stream.send_remain_size -= size;
                stream.send_buf_offset += size;
                if stream.send_buf_offset < buf.len() {
                    stream.send_bufs.push_front(buf);
                } else {
                    stream.send_buf_offset = 0;
                }
            }
            Err(err) => {
                if let ErrorKind::WouldBlock = err.kind() {
                    println!("send would block: size = {}", stream.send_buf_offset);
                    break;
                } else {
                    println!("Send Error: {:?}", err);
                    //panic!("Send Error: {:?}", err);
                    return true;
                }
            }
        }
    }
    return false;
}

//处理网络请求
pub fn handle_net(sender: Sender<SendClosureFn>, receiver: Receiver<SendClosureFn>) {
    let mut handler = NetHandler {
        port: 0,
        sender: sender,
        slab: Slab::<NetData>::new(),
        poll: Poll::new().unwrap(),
        recv_comings: Arc::new(RwLock::new(Vec::<Token>::new())),
        net_timers: Arc::new(RwLock::new(NetTimers::new())),
        counter: None,
    }; //创建处理器

    let mut events = Events::with_capacity(1024); //初始化mio事件池

    let one_sec = Duration::from_millis(10);

    let slow = Duration::from_millis(1);
    //tcp事件处理循环
    loop {
        thread::sleep(Duration::from_millis(1));
        let now = Instant::now();
        let start = Instant::now();
        // recv_comings
        for &Token(id) in handler.recv_comings.read().unwrap().iter() {
            if let Some(data) = handler.slab.get_mut(id) {
                match data {
                    &mut NetData::TcpStream(ref mut s, ref mio) => {
                        let mut stream = &mut s.write().unwrap();
                        stream.interest.insert(Ready::readable());
                        // println!(
                        //     "recv_comings stream {:?}: reregister, interest = {:?}",
                        //     stream.token, stream.interest
                        // );

                        handler
                            .poll
                            .reregister(mio, stream.token, stream.interest, PollOpt::level())
                            .unwrap();
                    }
                    _ => panic!("recv_comings failed!"),
                }
            }
        }
        handler.recv_comings.write().unwrap().clear();

        if start.elapsed() >= slow {
            println!("!!!!!!net poll recv coming, time: {:?}", start.elapsed());
        }
        let start = Instant::now();

        // handle event from net
        handler.poll.poll(&mut events, Some(Duration::from_millis(0))).unwrap();
        let mut events_len = 0;
        for event in &events {
            events_len += 1;

            let Token(token) = event.token();
            let readiness = event.readiness();
            let mut net_data: Option<NetData> = None;
            let mut net_addr: Option<SocketAddr> = None;

            let mut close_id: Option<usize> = None;
            if let Some(data) = handler.slab.get_mut(token) {
                match data {
                    &mut NetData::TcpServer(_, ref mut mio) => {
                        //处理请求tcp连接事件
                        handler.counter.as_ref().unwrap().connect_count.sum(1);

                        if readiness.is_readable() {
                            let (data, addr) = tcp_event(mio, handler.recv_comings.clone(), handler.net_timers.clone());
                            net_data = Some(data);
                            net_addr = Some(addr);

                            handler.counter.as_ref().unwrap().accepted_count.sum(1);
                        } else if readiness.is_writable() {
                            // TODO error callback
                            panic!("TODO tcp_event error callback");
                        }
                    }
                    &mut NetData::TcpStream(ref s, ref mut mio) => {
                        //处理tcp流接收事件
                        if readiness.is_readable() {
                            let mut is_close = false;
                            
                            let recv_r = stream_recv(&mut s.write().unwrap(), mio, handler.counter.as_ref().unwrap());

                            if let Some(r) = recv_r {
                                match r {
                                    Ok((func, range)) => {
                                        {
                                            let stream = &mut s.write().unwrap();
                                            stream.interest.remove(Ready::readable());

                                            // println!(
                                            //     "recv_buf1 stream {:?}: reregister, interest = {:?}",
                                            //     stream.token, stream.interest
                                            // );

                                            handler
                                                .poll
                                                .reregister(
                                                    mio,
                                                    stream.token,
                                                    stream.interest,
                                                    PollOpt::level(),
                                                )
                                                .unwrap();
                                        }

                                        let v;
                                        let is_temp_recv =
                                            !s.read().unwrap().temp_recv_buf.is_none();

                                        {
                                            if is_temp_recv {
                                                v = s.write().unwrap().temp_recv_buf.take().unwrap()
                                                    [..]
                                                    .iter()
                                                    .cloned()
                                                    .collect();
                                            } else {
                                                v = s.read().unwrap().recv_buf[range]
                                                    .iter()
                                                    .cloned()
                                                    .collect();
                                            }
                                        }

                                        func(Ok(Arc::new(v)));

                                    }
                                    Err(_) => {
                                        is_close = true;
                                    }
                                }
                            } else {
                                println!("{}, net trace, stream recv wouldblock, token: {:?}, peer: {:?}", now_millis(), s.read().unwrap().token, mio.peer_addr());
                            }

                            if is_close {
                                let Token(id) = s.read().unwrap().token;
                                close_id = Some(id);
                            }
                        } else if readiness.is_writable() {
                            // println!("write_buf1 stream");
                            let close =
                                stream_send(&mut handler.poll, &mut s.write().unwrap(), mio);
                            if close {
                                let Token(id) = s.read().unwrap().token;
                                close_id = Some(id);
                            }
                        }
                    }
                }
            }

            if !close_id.is_none() {
                handle_close(&mut handler, close_id.unwrap(), true);
            }

            let mut key: Option<usize> = None;
            if let Some(mut data) = net_data {
                let entry = handler.slab.vacant_entry();
                key = Some(entry.key());
                match data {
                    NetData::TcpStream(ref mut stream, ref mut mio) => {
                        let mut s = &mut stream.write().unwrap();
                        s.token = Token(entry.key());

                        // println!(
                        //     "net_data stream {:?}: register, interest = {:?}",
                        //     s.token, s.interest
                        // );

                        handler
                            .poll
                            .register(mio, s.token, s.interest, PollOpt::level())
                            .unwrap();
                    }
                    //TODO: UDP
                    _ => panic!("invalid net_data type!"),
                }
                entry.insert(data);
            }

            if let Some(k) = key {
                let socket;
                socket = RawSocket::new(k,
                                        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), handler.port),
                                        net_addr,
                                        handler.sender.clone());
                match handler.slab.get(token).unwrap() {
                    &NetData::TcpServer(ref callback, _) => {
                        let tcp_data = handler.slab.get(k).unwrap();
                        if let &NetData::TcpStream(ref stream, ref mio) = tcp_data {

                            callback(Ok((socket, stream.clone())), mio.peer_addr());
                        } else {
                            // TODO: UDP
                        }
                    }
                    _ => {}
                }
            }
        }

        if start.elapsed() >= slow {
            println!("!!!!!!net poll handle events, size: {:?}, time: {:?}", events_len, start.elapsed());
        }
        let start = Instant::now();

        // handle recv from logic thread
        while let Ok(func) = receiver.try_recv() {
            func(&mut handler);
        }

        if start.elapsed() >= slow {
            println!("!!!!!!net poll handle ext function, time: {:?}", start.elapsed());
        }
        let start = Instant::now();
        
        //轮询定时器
        handler.net_timers.write().unwrap().poll();

        // handle timeout net
        let mut tokens: Vec<usize> = vec![];
        for (_, val) in handler.slab.iter() {
            match val {
                &NetData::TcpStream(ref s, ref _mio) => {
                    let s = &mut s.read().unwrap();
                    let token_id = s.timeout_token.load(Ordering::Relaxed);
                    if s.recv_callback.is_none() || (token_id == usize::max_value()) {
                        continue;
                    }
                    tokens.push(token_id);
                }
                _ => {}
            }
        }

        if start.elapsed() >= slow {
            println!("!!!!!!net poll handle timeout, time: {:?}", start.elapsed());
        }
        let start = Instant::now();

        if tokens.len() > 0 {
            println!("---------- close tokens's len = {}", tokens.len());
        }
        for id in tokens {
            handle_close(&mut handler, id, true);
        }

        if start.elapsed() >= slow {
            println!("!!!!!!net poll handle close connect, time: {:?}", start.elapsed());
        }

        if now.elapsed() >= slow {
            println!("!!!!!!net poll time: {:?}", now.elapsed());
        }
    }
}

impl RawStream {
    pub fn new(id: usize, recv_comings: Arc<RwLock<Vec<Token>>>, net_timers: Arc<RwLock<NetTimers<TimerCallback>>>) -> Self {
        let mut recv_buf = Vec::with_capacity(MAX_RECV_SIZE);
        unsafe {
            recv_buf.set_len(MAX_RECV_SIZE);
        }

        Self {
            token: Token(id),
            interest: Ready::empty(),

            max_send_size: 0,
            send_buf_offset: 0,
            send_remain_size: 0,
            send_bufs: VecDeque::new(),

            recv_timeout: None,
            recv_buf_offset: 0,
            recv_callback_offset: 0,

            recv_buf: recv_buf,
            recv_size: 0,
            temp_recv_buf_offset: 0,
            recv_comings: recv_comings,
            recv_timer: Arc::new(AtomicUsize::new(usize::max_value())),
            timeout_token: Arc::new(AtomicUsize::new(usize::max_value())),
            temp_recv_buf: None,

            recv_callback: None,
            close_callback: None,

            net_timers: net_timers,
            websocket: Websocket::None,
            websocket_buf: vec![],
            socket: None,
        }
    }

    pub fn set_close_callback(&mut self, func: CloseFn) {
        self.close_callback = Some(func);
    }

    /// size's unit: byte
    pub fn set_send_buf_size(&mut self, size: usize) {
        self.max_send_size = size;
    }

    /// time's unit: ms
    pub fn set_recv_timeout(&mut self, time: usize) {
        self.recv_timeout = Some(Duration::from_millis(time as u64));
    }

    pub fn set_socket(&mut self, socket: RawSocket) {
        self.socket = Some(socket);
    }

    /// size's unit: byte
    pub fn recv_handle(&mut self, size: usize, func: RecvFn) -> Option<(RecvFn, Result<Arc<Vec<u8>>>)> {
        if !self.recv_callback.is_none() {
            return Some((func, Err(Error::new(
                ErrorKind::Other,
                "Recive callback can't set twice",
            ))));
        }
        
        self.temp_recv_buf = None;
        self.temp_recv_buf_offset = 0;

        let cb_offset = self.recv_callback_offset;
        let buf_offset = self.recv_buf_offset;
        if size == 0 && buf_offset > cb_offset {
            let param = self.recv_buf[cb_offset..buf_offset]
                .iter()
                .cloned()
                .collect();
            let size2 = buf_offset - cb_offset;
            self.recv_callback_offset += size2;
            return Some((func, Ok(Arc::new(param))));
        } else if size > 0 && size <= buf_offset - cb_offset {
            let param = self.recv_buf[cb_offset..cb_offset + size]
                .iter()
                .cloned()
                .collect();
            self.recv_callback_offset += size;
            return Some((func, Ok(Arc::new(param))));
        } else if size > MAX_RECV_SIZE {
            let mut buf = Vec::with_capacity(size);
            unsafe {
                buf.set_len(size);
            }

            let len = buf_offset - cb_offset;
            buf[0..len].copy_from_slice(&self.recv_buf[cb_offset..buf_offset]);
            self.recv_buf_offset = 0;
            self.recv_callback_offset = 0;
            self.temp_recv_buf = Some(buf);
        } else if self.recv_callback_offset + size > MAX_RECV_SIZE {
            let len = self.recv_buf_offset - self.recv_callback_offset;
            move_vec(&mut self.recv_buf, self.recv_callback_offset, 0, len);
            self.recv_callback_offset = 0;
            self.recv_buf_offset = len;
        }

        //移除未执行的定时器
        let last_timer_ref = self.recv_timer.load(Ordering::Relaxed);
        if last_timer_ref != usize::max_value() {
            TIMER.cancel(last_timer_ref);
        }

        //设置新的接收超时定时器
        let timeout = self.recv_timeout.unwrap();
        let Token(token_id) = self.token;
        let timeout_token = self.timeout_token.clone();
        let new_timer_ref = TIMER.set_timeout(FuncRuner::new(Box::new(move || {
            timeout_token.store(token_id, Ordering::Relaxed);
        })), timeout.as_millis() as u32);
        self.recv_timer.store(new_timer_ref, Ordering::Relaxed);

        self.recv_size = size;
        self.recv_callback = Some(func);
        self.recv_comings.write().unwrap().push(self.token);

        return None;
    }
}


/// websocket
pub fn recv(stream: Arc<RwLock<RawStream>>, size: usize, func: RecvFn) -> Option<(RecvFn, Result<Arc<Vec<u8>>>)> {
    let stream2 = stream.clone();
    let websocket;
    let websocket_buf;
    {
        websocket = stream.read().unwrap().websocket.clone();
        websocket_buf = stream.read().unwrap().websocket_buf.clone();
    }
    //是否已完成HTTP握手
    match websocket {
        Websocket::None => {
            let stream2 = stream.clone();
            let http_func = Box::new(move |r: Result<Upgrade<Cursor<Vec<u8>>>>| {
                match r {
                    Err(e) => println!("!!!> WS Handshake Error, e: {:?}", e),
                    Ok(mut ws) => {
                        let stream = stream2.clone();
                        {
                            let socket2;
                            {
                                let stream = &stream.read().unwrap();
                                socket2 = stream.socket.clone();
                            }
                            //发送回应包
                            let send_buf = get_send_buf(&mut ws).unwrap();
                            socket2.unwrap().send_bin(Arc::new(send_buf));
                        }

                        //修改握手状态
                        stream.write().unwrap().websocket = Websocket::Bin(0, 0);
                        recv(stream2.clone(), size, func);
                    },
                }
            });
            //请求握手包
            http_read_header(stream, vec![], http_func);
        },

        Websocket::Bin(offset, len) => {
            //判断缓存中是否取完
            if len >= size {
                let stream = stream2.clone();
                //从缓存中取数据
                let end = offset + size;
                //let buf = websocket_buf.clone();
                let v = &websocket_buf[offset..end];
                let v = Vec::from(v);
                //写入ws缓存
                stream.write().unwrap().websocket = Websocket::Bin(offset + size, len - size);
                return Some((func, Ok(Arc::new(v))));
            } else {
                let ws_func = Box::new(move |r: Result<OwnedMessage>| {
                    let stream = stream2.clone();
                    let o_msg = r.unwrap();
                    match o_msg {
                        OwnedMessage::Binary(buf) => {
                            if buf.len() >= size {
                                let v = Vec::from(&buf[0..size]);
                                let len = buf.len();

                                //写入ws缓存
                                stream.write().unwrap().websocket = Websocket::Bin(size, len - size);
                                stream.write().unwrap().websocket_buf = buf;
                                func(Ok(Arc::new(v)))
                            }
                        }
                        OwnedMessage::Text(_body) => {
                            func(Err(Error::new(ErrorKind::Other, "not bin")));
                        }
                        OwnedMessage::Close(close) => {
                            //立即回应关闭消息，且立即关闭连接
                            let mut socket = None;
                            {
                                let mut s = stream.write().unwrap();
                                if let Ok(_) = s.socket.as_ref()
                                                    .expect("socket not exist")
                                                    .state
                                                    .compare_exchange(State::Run as usize, State::WouldClose as usize, Ordering::SeqCst, Ordering::Acquire) {
                                    //客户端通知关闭，则回应关闭
                                    socket = s.socket.clone();
                                } else if let Ok(_) = s.socket.as_ref()
                                                    .expect("socket not exist")
                                                    .state
                                                    .compare_exchange(State::WouldClose as usize, State::Closed as usize, Ordering::SeqCst, Ordering::Acquire) {
                                    //客户端回应关闭，则立即关闭
                                    s.socket.as_ref().expect("socket not exist").rclose(true);
                                }
                            }

                            if let Some(s) = socket {
                                if let Some(data) = close {
                                    if let Ok(bin) = data.into_bytes() {
                                        if let Ok(reason) = String::from_utf8(bin) {
                                            //返回客户端关闭原因
                                            return s.send_control(WSControlType::Close(1000, reason));
                                        }
                                    }
                                }
                                s.send_control(WSControlType::Close(1000, "client closed connect".to_string()));
                            }
                        }
                        _ => {
                            //TODO ping包等数据包
                            func(Err(Error::new(ErrorKind::Other, "not bin")));
                        }
                    }


                });
                //从网络中等待websocket包
                ws_read_header(stream, ws_func);
            }
        },
    }
    None
}

// pub fn get_websocket_buf(stream: Arc<RwLock<Stream>>, pack: Vec<u8>, size: usize, func: Box<FnOnce(Result<Arc<Vec<u8>>>)>) {
//     let func2;
//     {
//         let stream = stream.clone();
//         func2 = Box::new(move |data: Result<Arc<Vec<u8>>>| {
//             let mut pack = vec![];
//             pack.extend_from_slice(data.unwrap().as_slice());
//             let size = 1;
//             //取到数据后返回

//             get_websocket_buf(stream, pack, size, func);
//         });
//     }

//     let r = stream.write().unwrap().recv(size, func2);
//     if let Some((func, data)) = r {
//         func(data);
//     }
// }

impl RawSocket {
    pub fn new(id: usize, local: SocketAddr, peer: Option<SocketAddr>, sender: Sender<SendClosureFn>) -> Self {
        Self {
            state: Arc::new(AtomicUsize::new(State::Run as usize)),
            socket: id,
            sender: sender,
            gray: None,
            peer,
            local,
        }
    }
}

fn move_vec(v: &mut Vec<u8>, src_offset: usize, dst_offset: usize, size: usize) {
    let len = v.len();
    if src_offset + size > len || dst_offset + size > len {
        panic!("move_vec failed!");
    }

    unsafe {
        let v = v.as_mut_ptr();
        v.wrapping_offset(src_offset as isize)
            .copy_to(v.wrapping_offset(dst_offset as isize), size);
    }
}

impl GrayVersion for RawSocket {
    fn get_gray(&self) -> &Option<usize>{
        &self.gray
    }

    fn set_gray(&mut self, gray: Option<usize>){
        self.gray = gray;
    }

    fn get_id(&self) -> usize{
        self.socket
    }
}