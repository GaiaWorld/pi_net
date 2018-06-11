extern crate mqtt;
extern crate mqtt3;
extern crate net;
extern crate pi_lib;
extern crate rpc;

mod client;
mod server;

use std::thread::sleep;
use std::time::Duration;

use server::start_server;

#[test]
fn run() {
    let _s = start_server();
    loop {
        sleep(Duration::from_secs(1))
    }
}
