
use std::sync::{Arc, RwLock};
use std::io::{Error, ErrorKind, Result, Write, Cursor};
use std::boxed::FnBox;
use data::Stream;

use hyper::buffer::BufReader;
use hyper::http::h1::parse_request;
use hyper::header::{Headers};

use websocket::ws::Message as MessageTrait;
use websocket::OwnedMessage;
use websocket::Message;
use websocket::server::upgrade::{WsUpgrade, validate};
use websocket::server::upgrade::sync::{Buffer, Upgrade};
use websocket::ws::util::header as dfh;

use websocket::dataframe::{DataFrame, Opcode};
use websocket::result::{WebSocketResult, WebSocketError};

pub fn read_header(stream: Arc<RwLock<Stream>>, buf: Vec<u8>, func: Box<FnBox(Result<Upgrade<Cursor<Vec<u8>>>>)>) {
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
                    match validate(&r.subject.0, &r.version, &r.headers) {
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
    stream.write().unwrap().recv_handle(0, handle_func);
}

pub fn read_ws_header(stream: Arc<RwLock<Stream>>, func: Box<FnBox(Result<OwnedMessage>)>)
{
    println!("read_ws_header------------------------------------------------------------------------");
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

pub fn get_send_buf(ws: &mut Upgrade<Cursor<Vec<u8>>>) -> Result<Vec<u8>> {
    let mut headers = Headers::new();
    headers.set_raw("Sec-WebSocket-Protocol", vec![Vec::from("mqttv3.1")]);
    let status = ws.prepare_headers(Some(&headers));
    let mut send_buf = vec![];
    write!(&mut send_buf, "{} {}\r\n", ws.request.version, status).is_ok();
	write!(&mut send_buf, "{}\r\n", ws.headers).is_ok();
    Ok(send_buf)
}


/// Reads a single data frame from the remote endpoint.
fn recv_dataframe(stream: Arc<RwLock<Stream>>, should_be_masked: bool, cb: Box<FnBox(WebSocketResult<DataFrame>)>){
    let stream1 = stream.clone();
    read_header1(stream, Box::new(move |header: WebSocketResult<dfh::DataFrameHeader>|{
        let header = match header{
            Ok(h) => h,
            Err(e) => {cb(Err(e)); return;},
        };
        stream1.write().unwrap().recv_handle(header.len as usize, Box::new(
            move |r: Result<Arc<Vec<u8>>>| {
                cb(DataFrame::read_dataframe_body(header, Arc::try_unwrap(r.unwrap()).unwrap(), should_be_masked));
            }
        ));
    }));
}

/// Returns the data frames that constitute one message.
fn recv_message_dataframes(stream: Arc<RwLock<Stream>>, mask: bool, cb: Box<FnBox(WebSocketResult<Vec<DataFrame>>)>){
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

/// Returns the data frames that constitute one message.
fn next_frame(stream: Arc<RwLock<Stream>>, mask: bool, mut buffer: Vec<DataFrame>, cb: Box<FnBox(WebSocketResult<Vec<DataFrame>>)>){
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

/// 解析前两个字节, 返回描述长度的字节数
fn read_header1(stream: Arc<RwLock<Stream>>, cb: Box<FnBox(WebSocketResult<dfh::DataFrameHeader>)>){
    let stream1 = stream.clone();
    stream.write().unwrap().recv_handle(2, Box::new(
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
                        stream1.write().unwrap().recv_handle(mask, Box::new(
                            move |len: Result<Arc<Vec<u8>>>| {
                                r.extend_from_slice(len.unwrap().as_slice());
                                cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                            }
                        ));
                    }else{
                        cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                    }
                },
                126 => {
                    stream1.write().unwrap().recv_handle(2 + mask, Box::new(
                        move |len: Result<Arc<Vec<u8>>>| {
                            r.extend_from_slice(len.unwrap().as_slice());
                            cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                        }
                    ));
                }
                127 => {
                    stream1.write().unwrap().recv_handle(8 + mask, Box::new(
                        move |len: Result<Arc<Vec<u8>>>| {
                            r.extend_from_slice(len.unwrap().as_slice());
                            cb(dfh::read_header(&mut BufReader::new(r.as_slice())));
                        }
                    ));
                }
                _ => unreachable!(),
            };
        }
    ));
}
