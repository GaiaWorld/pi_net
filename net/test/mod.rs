
extern crate net;

mod client;
mod server;

use std::thread::sleep;
use std::time::Duration;

use client::start_client;
use server::start_server;

#[test]
fn run() {
    let _s = start_server();

    let _c = start_client();
    sleep(Duration::from_secs(30));
}

