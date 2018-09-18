use std::thread;
use pi_lib::atom::Atom;


use https::Https;
use handler::Handler;

/*
* 启动http协议的指定服务器
*/
pub fn start_http<H: Handler>(handler: H, ip: Atom, port: u16, keep_alive_timeout: u64, handle_timeout: u32) {
    thread::spawn(move || {
        Https::new(handler, keep_alive_timeout, handle_timeout).http((&ip).to_string() + ":" + &port.to_string());
    });
}