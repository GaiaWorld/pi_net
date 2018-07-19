
use std::sync::{Arc, RwLock};
use std::io::{Error, ErrorKind, Result, Write, Cursor};
use std::boxed::FnBox;
use data::Stream;

use hyper::buffer::BufReader;
use hyper::http::h1::parse_request;
use hyper::header::{Headers};
use hyper::uri::RequestUri;

use websocket::ws::{Receiver as ReceiverTrait};
use websocket::ws::Message as MessageTrait;
use websocket::receiver::{Receiver};
use websocket::OwnedMessage;
use websocket::Message;
use websocket::server::upgrade::{WsUpgrade, validate};
use websocket::server::upgrade::sync::{Buffer, Upgrade};

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

pub fn read_ws_header(stream: Arc<RwLock<Stream>>, buf: Vec<u8>, func: Box<FnBox(Result<OwnedMessage>)>)
{
	let stream2 = stream.clone();
	//读取2字节数据
	let mut pack = vec![];
	stream.write().unwrap().recv_handle(0, Box::new(
		move |r: Result<Arc<Vec<u8>>>| {
		pack.extend_from_slice(buf.as_slice());
        pack.extend_from_slice(r.unwrap().as_slice());
		let pack2 = pack.clone();
		let c = Cursor::new(pack);
		let mut reader = BufReader::new(c);
		let mut receiver = Receiver::new(true);
		match receiver.recv_message_dataframes(&mut reader) {
			Ok(dataframes) => {
				let msg = Message::from_dataframes(dataframes).unwrap();
				let o_msg = OwnedMessage::from(msg);
				func(Ok(o_msg));
			}
			Err(_) => {
				read_ws_header(stream2, pack2, func)
			}
		}
		}
	));
}

pub fn get_send_buf(ws: &mut Upgrade<Cursor<Vec<u8>>>) -> Result<Vec<u8>> {
    let mut headers = Headers::new();
    headers.set_raw("Sec-WebSocket-Protocol", vec![Vec::from("mqtt")]);
    let status = ws.prepare_headers(Some(&headers));
    let mut send_buf = vec![];
    write!(&mut send_buf, "{} {}\r\n", ws.request.version, status).is_ok();
	write!(&mut send_buf, "{}\r\n", ws.headers).is_ok();
    Ok(send_buf)
}