use std::thread;
use std::boxed::FnBox;
use std::sync::{Arc, RwLock};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::io::{Cursor, Result, Write, ErrorKind, Error};

use tls;
use data::{Config, ListenerFn, RecvFn, State, Protocol, Websocket};
use api::WSControlType;

use hyper::buffer::BufReader;
use hyper::http::h1::parse_request;
use hyper::header::{Headers};

use mio::Token;

use websocket::ws::Message as MessageTrait;
use websocket::OwnedMessage;
use websocket::Message;
use websocket::server::upgrade::{WsUpgrade, validate};
use websocket::server::upgrade::sync::{Buffer, Upgrade};
use websocket::ws::util::header as dfh;
use websocket::message::CloseData;
use websocket::ws::Sender as SenderT;
use websocket::sender::{Sender as WsSender};
use websocket::dataframe::{DataFrame, Opcode};
use websocket::result::{WebSocketResult, WebSocketError};

//websocket的tls socket
#[cfg(not(test))]
impl tls::TlsSocket {
    //通知当前tls连接发送指定的tcp数据
    pub fn send_bin(&self, buf: Arc<Vec<u8>>) {
        let socket = self.socket;
        let send = Box::new(move |handler: &mut tls::TlsHandler| {
            tls::handle_send(handler, socket, buf);
        });
        self.sender.send(send).unwrap();
    }

    //通知当前tls连接发送指定的websocket数据包
    pub fn send(&self, buf: Arc<Vec<u8>>) {
        //编码ws数据包
        let mut sender = WsSender::new(false);
        let mut reader = Cursor::new(vec![]);
        let buf  = Vec::from(buf.as_slice());
        let message = OwnedMessage::Binary(buf);
        sender.send_dataframe(&mut reader, &message).is_ok();
        let buf = Arc::new(reader.into_inner());

        self.send_bin(buf);
    }

    //通知当前tls连接发送指定的websocket控制包
    pub fn send_control(&self, msg: WSControlType) {
        //编码ws控制包
        let mut sender = WsSender::new(false);
        let mut reader = Cursor::new(vec![]);
        let socket = self.socket;
        let message = match msg {
            WSControlType::Close(state, reason) => {
                OwnedMessage::Close(Some(CloseData::new(state, reason)))
            },
            WSControlType::Ping(bin) => {
                OwnedMessage::Ping(bin)
            },
            WSControlType::Pong(bin) => {
                OwnedMessage::Pong(bin)
            },
        };
        sender.send_message(&mut reader, &message).expect(&format!("send control error, msg: {:?}", message));
        let buf = Arc::new(reader.into_inner());

        self.send_bin(buf);
    }

    //通知当前tls连接关闭
    pub fn close(&self, _force: bool) {
        if let Ok(_) = self.state.compare_exchange(State::Run as usize, State::WouldClose as usize, Ordering::SeqCst, Ordering::Acquire) {
            self.send_control(WSControlType::Close(0, "server closed connect".to_string()));
        }
    }

    //实际关闭连接
    pub fn rclose(&self, force: bool) {
        if let Ok(_) = self.state.compare_exchange(State::WouldClose as usize, State::Closed as usize, Ordering::SeqCst, Ordering::Acquire) {
            let socket = self.socket;
            let send = Box::new(move |handler: &mut tls::TlsHandler| {
                tls::handle_close(handler, socket, force);
            });
            self.sender.send(send).unwrap();
        }
    }
}

//接收
#[cfg(not(test))]
pub fn recv(stream: Arc<RwLock<tls::TlsStream>>, size: usize, func: RecvFn) -> Option<(RecvFn, Result<Arc<Vec<u8>>>)> {
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
                    Err(e) => println!("!!!> Wss Handshake Error, e: {:?}", e),
                    Ok(mut ws) => {
                        let stream = stream2.clone();
                        {
                            let socket2= stream.read().unwrap().socket.clone();
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
            read_header(stream, vec![], http_func);
        },

        Websocket::Bin(offset, len) => {
            println!("!!!!!!wss, websocket frame, token: {:?}, offset: {}, len: {}, size: {}", &stream.read().unwrap().token, offset, len, size);
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
                println!("!!!!!!wss, wait websocket frame, token: {:?}", &stream.read().unwrap().token);
                //从网络中等待websocket包
                read_ws_header(stream, ws_func);
            }
        },
    }
    None
}

//读websocket握手数据中的http头
#[cfg(not(test))]
pub fn read_header(stream: Arc<RwLock<tls::TlsStream>>, buf: Vec<u8>, func: Box<FnBox(Result<Upgrade<Cursor<Vec<u8>>>>)>) {
    let stream2 = stream.clone();
    let handle_func;
    {
        handle_func = Box::new(move |v: Result<Arc<Vec<u8>>>|{
            let mut pack = vec![];
            pack.extend_from_slice(buf.as_slice());
            pack.extend_from_slice(v.unwrap().as_slice());
            let pack2 = pack.clone();
            let c = Cursor::new(pack);
            let mut reader = BufReader::new(c);
            let request = parse_request(&mut reader);
            let (stream, buf, pos, cap) = reader.into_parts();
            let buffer = Some(Buffer {
                buf: buf,
                cap: cap,
                pos: pos,
            });
            match request {
                Ok(r) => {
                    match validate(&r.subject.0, r.version, &r.headers) {
                        Ok(_) => {
                            func(Ok(WsUpgrade {
                                headers: Headers::new(),
                                stream: stream,
                                request: r,
                                buffer: buffer,
                            }))
                        }
                        Err(_e) => func(Err(Error::new(ErrorKind::Other, "validate error"))),
                    };
                },
                Err(_e) => read_header(stream2, pack2, func),
            };
        })
    }
    let result = stream.write().unwrap().wakeup_readable(0, handle_func);
    if let Some((cb, r)) = result {
        cb(r); //同步处理唤醒后读取的数据
    }
}

#[cfg(not(test))]
pub fn read_ws_header(stream: Arc<RwLock<tls::TlsStream>>, func: Box<FnBox(Result<OwnedMessage>)>)
{
    // println!("read_ws_header------------------------------------------------------------------------");
    let stream2 = stream.clone();
    recv_message_dataframes(stream, true, Box::new(move |r| {
        match r {
            Ok(dataframes) => {
                let msg = Message::from_dataframes(dataframes).unwrap();
                let o_msg = OwnedMessage::from(msg);
                func(Ok(o_msg));
            }
            Err(_e) => {
                read_ws_header(stream2, func)
            }
        }
    }));

    // stream.write().unwrap().recv_handle(0, Box::new(
    // 	move |r: Result<Arc<Vec<u8>>>| {
    // 	pack.extend_from_slice(buf.as_slice());
    //     pack.extend_from_slice(r.unwrap().as_slice());
    // 	let pack2 = pack.clone();
    // 	let c = Cursor::new(pack);
    // 	let mut reader = BufReader::new(c);
    // 	// let mut receiver = Receiver::new(true);
    // 	// match receiver.recv_message_dataframes(&mut reader) {
    // 	// 	Ok(dataframes) => {
    // 	// 		let msg = Message::from_dataframes(dataframes).unwrap();
    // 	// 		let o_msg = OwnedMessage::from(msg);
    // 	// 		func(Ok(o_msg));
    // 	// 	}
    // 	// 	Err(_) => {
    // 	// 		read_ws_header(stream2, pack2, func)
    // 	// 	}
    // 	// }
    // 	}
    // ));
}

// Reads a single data frame from the remote endpoint.
#[cfg(not(test))]
fn recv_dataframe(stream: Arc<RwLock<tls::TlsStream>>, should_be_masked: bool, cb: Box<FnBox(WebSocketResult<DataFrame>)>){
    let stream1 = stream.clone();
    read_header1(stream, Box::new(move |header: WebSocketResult<dfh::DataFrameHeader>|{
        let header = match header{
            Ok(h) => h,
            Err(e) => {
                cb(Err(e));
                return;
            },
        };

        let result = stream1.write().unwrap().wakeup_readable(header.len as usize, Box::new(
            move |r: Result<Arc<Vec<u8>>>| {
                cb(DataFrame::read_dataframe_body(header, Arc::try_unwrap(r.unwrap()).unwrap(), should_be_masked));
            }
        ));
        if let Some((cb, r)) = result {
            cb(r); //同步处理唤醒后读取的数据
        }
    }));
}

// Returns the data frames that constitute one message.
#[cfg(not(test))]
fn recv_message_dataframes(stream: Arc<RwLock<tls::TlsStream>>, mask: bool, cb: Box<FnBox(WebSocketResult<Vec<DataFrame>>)>){
    let mut buffer = Vec::new();
    recv_dataframe(stream.clone(), mask,  Box::new(
        move |r: WebSocketResult<DataFrame>| {
            let first = match r {
                Ok(f) => f,
                Err(e) => {cb(Err(e)); return;},
            };
            if first.opcode == Opcode::Continuation {
                return cb(Err(WebSocketError::ProtocolError("Unexpected continuation data frame opcode",),));
            }

            let finished = first.finished;
            buffer.push(first);

            if !finished{
                next_frame(stream, mask, buffer, cb);
            }else{
                cb(Ok(buffer));
            }
        }
    ));
}

// Returns the data frames that constitute one message.
#[cfg(not(test))]
fn next_frame(stream: Arc<RwLock<tls::TlsStream>>, mask: bool, mut buffer: Vec<DataFrame>, cb: Box<FnBox(WebSocketResult<Vec<DataFrame>>)>){
    recv_dataframe(stream.clone(), mask,  Box::new(
        move |r: WebSocketResult<DataFrame>| {
            let frame = match r {
                Ok(f) => f,
                Err(e) => {cb(Err(e)); return;},
            };
            let finished = frame.finished;
            match frame.opcode as u8 {
                // Continuation opcode
                0 => buffer.push(frame),
                // Control frame
                8...15 => {
                    return cb(Ok(vec![frame]));
                }
                // Others
                _ => return cb(Err(WebSocketError::ProtocolError("Unexpected data frame opcode"))),
            }

            if !finished{
                next_frame(stream, mask, buffer, cb);
            }else{
                cb(Ok(buffer));
            }
        }
    ));
}

// 解析前两个字节, 返回描述长度的字节数
#[cfg(not(test))]
fn read_header1(stream: Arc<RwLock<tls::TlsStream>>, cb: Box<FnBox(WebSocketResult<dfh::DataFrameHeader>)>){
    let stream1 = stream.clone();
    let result = stream.write().unwrap().wakeup_readable(2, Box::new(
        move |r: Result<Arc<Vec<u8>>>| {
            let mut r = Arc::try_unwrap(r.unwrap()).unwrap();
            let byte1 = r[1];

            let mask = if byte1 & 0x80 == 0x80{
                4
            }else{
                0
            };

            match byte1 & 0x7F {
                0...125 => {
                    if mask == 4 {
                        let result = stream1.write().unwrap().wakeup_readable(mask, Box::new(
                            move |len: Result<Arc<Vec<u8>>>| {
                                r.extend_from_slice(len.unwrap().as_slice());
                                cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                            }
                        ));
                        if let Some((cb, r)) = result {
                            cb(r); //同步处理唤醒后读取的数据
                        }
                    }else{
                        cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                    }
                },
                126 => {
                    let result = stream1.write().unwrap().wakeup_readable(2 + mask, Box::new(
                        move |len: Result<Arc<Vec<u8>>>| {
                            r.extend_from_slice(len.unwrap().as_slice());
                            cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                        }
                    ));
                    if let Some((cb, r)) = result {
                        cb(r); //同步处理唤醒后读取的数据
                    }
                }
                127 => {
                    let result = stream1.write().unwrap().wakeup_readable(8 + mask, Box::new(
                        move |len: Result<Arc<Vec<u8>>>| {
                            r.extend_from_slice(len.unwrap().as_slice());
                            cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                        }
                    ));
                    if let Some((cb, r)) = result {
                        cb(r); //同步处理唤醒后读取的数据
                    }
                }
                _ => unreachable!(),
            };
        }
    ));
    if let Some((cb, r)) = result {
        cb(r); //同步处理唤醒后读取的数据
    }
}

#[cfg(not(test))]
pub fn get_send_buf(ws: &mut Upgrade<Cursor<Vec<u8>>>) -> Result<Vec<u8>> {
    let mut headers = Headers::new();
    headers.set_raw("Sec-WebSocket-Protocol", vec![Vec::from("mqttv3.1")]);
    let status = ws.prepare_headers(Some(&headers));
    let mut send_buf = vec![];
    write!(&mut send_buf, "{} {}\r\n", ws.request.version, status).is_ok();
    write!(&mut send_buf, "{}\r\n", ws.headers).is_ok();
    Ok(send_buf)
}