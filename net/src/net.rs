use std::ops::Range;
use std::sync::{Arc, RwLock};
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::time::{Duration, Instant};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::net::{Shutdown, SocketAddr};

use mio::{Events, Poll, PollOpt, Ready, Token};
use mio::net::{TcpListener, TcpStream};
use timer::{NetTimer};

use slab::Slab;

use data::{CloseFn, Config, ListenerFn, NetData, NetHandler, Protocol, RecvFn, SendClosureFn,
           Socket, State, Stream};

const MAX_RECV_SIZE: usize = 16 * 1024;

fn bind_tcp(handler: &mut NetHandler, addr: SocketAddr, _config: Config, func: ListenerFn) {
    if let Ok(lisn) = TcpListener::bind(&addr) {
        let entry = handler.slab.vacant_entry();
        let key = entry.key();

        println!(
            "listener {:?}: register, interest = {:?}",
            key,
            Ready::readable()
        );

        handler
            .poll
            .register(&lisn, Token(key), Ready::readable(), PollOpt::level())
            .unwrap();

        let data = NetData::TcpServer(func, lisn);
        entry.insert(data);
    }
}

pub fn handle_bind(handler: &mut NetHandler, addr: SocketAddr, config: Config, func: ListenerFn) {
    match config.protocol {
        Protocol::TCP => bind_tcp(handler, addr, config, func),
    }
}

pub fn connect_tcp(handler: &mut NetHandler, config: Config, func: ListenerFn) {
    if let Ok(s) = TcpStream::connect(&config.server_addr.unwrap()) {
        let entry = handler.slab.vacant_entry();
        let key = entry.key();

        s.set_recv_buffer_size(MAX_RECV_SIZE).unwrap();

        let mut stream = Stream::new(key, handler.recv_comings.clone());
        stream.interest.insert(Ready::writable());

        println!(
            "stream {:?}: register, interest = {:?}",
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

        let socket = Socket::new(key, handler.sender.clone());

        let param1 = Ok((socket, stream));
        let param2 = Ok(local_addr);
        func(param1, param2);
    }
}

pub fn handle_connect(handler: &mut NetHandler, config: Config, func: ListenerFn) {
    match config.protocol {
        Protocol::TCP => connect_tcp(handler, config, func),
    }
}

// return bool to indicate that should close the stream
fn send_tcp(poll: &mut Poll, stream: &mut Stream, mio: &mut TcpStream, v8: Arc<Vec<u8>>) -> bool {
    return match stream.state {
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
                    println!(
                        "stream {:?}: reregister, interest = {:?}",
                        stream.token, stream.interest
                    );
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

pub fn handle_send(handler: &mut NetHandler, stream: usize, v8: Arc<Vec<u8>>) {
    let mut should_close = false;
    if let Some(data) = handler.slab.get_mut(stream) {
        should_close = match data {
            &mut NetData::TcpStream(ref s, ref mut mio) => {
                let mut s_data = s.write().unwrap();
                let r = send_tcp(&mut handler.poll, &mut s_data, mio, v8);
                r
            }
            _ => panic!("handle_send failed, NetData's error"),
        }
    }

    if should_close {
        handle_close(handler, stream, true);
    }
}

fn close_tcp(poll: &mut Poll, stream: &mut Stream, mio: &mut TcpStream, force: bool) {
    if force || stream.send_remain_size == 0 {
        stream.state = State::Closed;
        stream.interest = Ready::empty();
        poll.deregister(mio).unwrap();
        println!("stream deregister {:?}, shutdown!!!!!!!!!!", stream.token);
        mio.shutdown(Shutdown::Both).unwrap();
    } else {
        match stream.state {
            State::Run => stream.state = State::WouldClose,
            _ => {}
        }
    }
}

pub fn handle_close(handler: &mut NetHandler, stream: usize, force: bool) {
    if let Some(data) = handler.slab.get_mut(stream) {
        match data {
            &mut NetData::TcpServer(_, _) => panic!("invalid close: TcpServer "),
            &mut NetData::TcpStream(ref mut s, ref mut mio) => {
                close_tcp(&mut handler.poll, &mut s.write().unwrap(), mio, force);
                let func = s.write().unwrap().close_callback.take().unwrap();
                func.call_box((stream, Ok(())));
            }
        }
    }

    if force {
        handler.slab.remove(stream);
    }
}

fn tcp_event(mio: &mut TcpListener, recv_comings: Arc<RwLock<Vec<Token>>>) -> Option<NetData> {
    let (tcp_stream, _) = mio.accept().unwrap();
    let s = Stream::new(0, recv_comings);
    tcp_stream.set_recv_buffer_size(MAX_RECV_SIZE).unwrap();
    Some(NetData::TcpStream(Arc::new(RwLock::new(s)), tcp_stream))
}

// return bool indicate that should close the net imm;
fn stream_recv(stream: &mut Stream, mio: &mut TcpStream) -> Option<Result<(RecvFn, Range<usize>)>> {
    if stream.recv_callback.is_none() {
        panic!("stream_recv failed, stream.recv_callback == None");
    }

    let mut r: Option<Result<(RecvFn, Range<usize>)>> = None;
    if stream.temp_recv_buf.is_none() {
        let would_block = loop {
            let begin = stream.recv_buf_offset;
            let end = stream.recv_callback_offset + stream.recv_size;
            match mio.read(&mut stream.recv_buf[begin..end]) {
                Ok(size) => {
                    stream.recv_buf_offset += size;
                    if stream.recv_buf_offset == end {
                        break Ok(());
                    }
                }
                Err(err) => {
                    if let ErrorKind::WouldBlock = err.kind() {
                        println!(
                            "{:?} recv wouldblock, offset = {}",
                            stream.token, stream.recv_buf_offset
                        );
                        break Ok(());
                    }
                    break Err(err);
                }
            }
        };

        match would_block {
            Ok(_) => {
                assert!(stream.recv_buf_offset - stream.recv_callback_offset <= stream.recv_size);

                if stream.recv_buf_offset - stream.recv_callback_offset == stream.recv_size {
                    let start = stream.recv_callback_offset;
                    let end = start + stream.recv_size;
                    let func = stream.recv_callback.take().unwrap();
                    r = Some(Ok((func, start..end)));

                    stream.recv_callback_offset += stream.recv_size;
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

fn stream_send(poll: &mut Poll, stream: &mut Stream, mio: &mut TcpStream) -> bool {
    loop {
        let buf = stream.send_bufs.pop_front();
        if None == buf {
            mio.flush().unwrap();

            println!("stream send finish!!!!!!!!");

            stream.interest.remove(Ready::writable());

            println!(
                "stream {:?}: reregister, interest = {:?}",
                stream.token, stream.interest
            );

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
                    panic!("Send Error: {:?}", err);
                    // return true;
                }
            }
        }
    }
    return false;
}

pub fn handle_net(sender: Sender<SendClosureFn>, receiver: Receiver<SendClosureFn>) {
    let mut handler = NetHandler {
        sender: sender,
        slab: Slab::<NetData>::new(),
        poll: Poll::new().unwrap(),
        recv_comings: Arc::new(RwLock::new(Vec::<Token>::new())),
    };

    let mut events = Events::with_capacity(1024);

    let one_sec = Duration::from_secs(1);

    loop {
        // recv_comings
        for &Token(id) in handler.recv_comings.read().unwrap().iter() {
            let data = handler.slab.get_mut(id).unwrap();
            match data {
                &mut NetData::TcpStream(ref mut s, ref mio) => {
                    let mut stream = &mut s.write().unwrap();
                    stream.interest.insert(Ready::readable());
                    println!(
                        "stream {:?}: reregister, interest = {:?}",
                        stream.token, stream.interest
                    );

                    handler
                        .poll
                        .reregister(mio, stream.token, stream.interest, PollOpt::level())
                        .unwrap();
                }
                _ => panic!("recv_comings failed!"),
            }
        }
        handler.recv_comings.write().unwrap().clear();

        // handle event from net
        handler.poll.poll(&mut events, Some(one_sec)).unwrap();
        for event in &events {
            let Token(token) = event.token();
            let readiness = event.readiness();
            let mut net_data: Option<NetData> = None;

            let mut close_id: Option<usize> = None;
            if let Some(data) = handler.slab.get_mut(token) {
                match data {
                    &mut NetData::TcpServer(_, ref mut mio) => {
                        if readiness.is_readable() {
                            net_data = tcp_event(mio, handler.recv_comings.clone());
                        } else if readiness.is_writable() {
                            // TODO error callback
                            panic!("TODO tcp_event error callback");
                        }
                    }
                    &mut NetData::TcpStream(ref s, ref mut mio) => {
                        if readiness.is_readable() {
                            let mut is_close = false;

                            let recv_r = stream_recv(&mut s.write().unwrap(), mio);

                            if let Some(r) = recv_r {
                                match r {
                                    Ok((func, range)) => {
                                        {
                                            let stream = &mut s.write().unwrap();
                                            stream.interest.remove(Ready::readable());

                                            println!(
                                                "stream {:?}: reregister, interest = {:?}",
                                                stream.token, stream.interest
                                            );

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
                                println!("stream_recv: return None!");
                            }

                            if is_close {
                                let Token(id) = s.read().unwrap().token;
                                close_id = Some(id);
                            }
                        } else if readiness.is_writable() {
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

                        println!(
                            "stream {:?}: register, interest = {:?}",
                            s.token, s.interest
                        );

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
                let socket = Socket::new(k, handler.sender.clone());
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

        // handle recv from logic thread
        while let Ok(func) = receiver.try_recv() {
            func.call_box((&mut handler,));
        }

        // handle timeout net
        let mut tokens: Vec<usize> = vec![];
        for (_, val) in handler.slab.iter() {
            match val {
                &NetData::TcpStream(ref s, ref _mio) => {
                    let s = &mut s.read().unwrap();
                    if s.recv_callback.is_none() || s.recv_timer.is_none() {
                        continue;
                    }
                    let timer = s.recv_timer.as_ref().unwrap();
                    match timer.poll() {
                        Some(Token(id)) => {
                            tokens.push(id);
                        },
                        None => (),
                    }
                }
                _ => {}
            }
        }

        if tokens.len() > 0 {
            println!("---------- close tokens's len = {}", tokens.len());
        }
        for id in tokens {
            handle_close(&mut handler, id, true);
        }
    }
}

impl Stream {
    pub fn new(id: usize, recv_comings: Arc<RwLock<Vec<Token>>>) -> Self {
        let mut recv_buf = Vec::with_capacity(MAX_RECV_SIZE);
        unsafe {
            recv_buf.set_len(MAX_RECV_SIZE);
        }

        Self {
            state: State::Run,
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
            recv_timer: None,
            temp_recv_buf: None,

            recv_callback: None,
            close_callback: None,
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

    /// size's unit: byte
    pub fn recv(&mut self, size: usize, func: RecvFn) -> Option<(RecvFn, Result<Arc<Vec<u8>>>)> {
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

        if size <= buf_offset - cb_offset {
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
        let timeout = self.recv_timeout.unwrap();
        let timer = NetTimer::new();
        timer.set_timeout(timeout, self.token);
        self.recv_size = size;
        self.recv_callback = Some(func);
        self.recv_timer = Some(timer);
        self.recv_comings.write().unwrap().push(self.token);

        return None;
    }
}

impl Socket {
    pub fn new(id: usize, sender: Sender<SendClosureFn>) -> Self {
        Self {
            socket: id,
            sender: sender,
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