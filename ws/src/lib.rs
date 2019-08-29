#![feature(never_type)]
#![feature(async_await)]

extern crate http;
extern crate httparse;
extern crate futures;
extern crate base64;
extern crate bytes;
extern crate fnv;

extern crate atom;
extern crate pi_crypto;

extern crate tcp;

pub mod server;
pub mod acceptor;
pub mod frame;
pub mod msg;
pub mod connect;

/*
* Websocket子协议
*/
pub trait ChildPorotocl: Send + 'static {
    //获取子协议名称
    fn protocol_name(&self) -> &str;
}