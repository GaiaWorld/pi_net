use std::thread;
use atom::Atom;


use https::Https;
use handler::Handler;

/*
* 启动http协议的指定服务器
*/
pub fn start_http<H: Handler>(handler: H, ip: Atom, port: u16, keep_alive_timeout: usize, handle_timeout: u32) {
    thread::spawn(move || {
        Https::new(handler, keep_alive_timeout as u64, handle_timeout)
            .http((&ip).to_string() + ":" + &port.to_string());
    });
}

/*
* 启动https协议的指定服务器
*/
pub fn start_https<H: Handler>(handler: H, ip: Atom, port: u16, keep_alive_timeout: usize, handle_timeout: u32, cert_file: Atom, key_file: Atom) {
    thread::spawn(move || {
        Https::new(handler, keep_alive_timeout as u64, handle_timeout)
            .https((&ip).to_string() + ":" + &port.to_string(), &cert_file, &key_file);
    });
}