use std::thread;
use atom::Atom;


use https::Https;
use handler::Handler;

/**
* 启动http协议的指定服务器
* @param handler 处理器，例如一个挂载器
* @param ip 绑定的服务器地址
* @param port 绑定的服务器端口
* @param keep_alive_timeout 长连接超时时长，单位毫秒，如果大于0，则表示支持http长连接
* @param handle_timeout 处理超时时长，单位毫秒
*/
pub fn start_http<H: Handler>(handler: H, ip: Atom, port: u16, keep_alive_timeout: usize, handle_timeout: u32) {
    thread::spawn(move || {
        Https::new(handler, keep_alive_timeout as u64, handle_timeout)
            .http((&ip).to_string() + ":" + &port.to_string());
    });
}

/**
* 启动https协议的指定服务器
* @param handler 处理器，例如一个挂载器
* @param ip 绑定的服务器地址
* @param port 绑定的服务器端口
* @param keep_alive_timeout 长连接超时时长，单位毫秒，如果大于0，则表示支持http长连接
* @param handle_timeout 处理超时时长，单位毫秒
* @param cert_file 证书文件路径
* @param key_file 密钥文件路径
*/
pub fn start_https<H: Handler>(handler: H, ip: Atom, port: u16, keep_alive_timeout: usize, handle_timeout: u32, cert_file: Atom, key_file: Atom) {
    thread::spawn(move || {
        Https::new(handler, keep_alive_timeout as u64, handle_timeout)
            .https((&ip).to_string() + ":" + &port.to_string(), &cert_file, &key_file);
    });
}