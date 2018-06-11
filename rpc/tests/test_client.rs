extern crate mqtt;
extern crate mqtt3;
extern crate net;
extern crate pi_lib;
extern crate rpc;

mod client;

use std::thread::sleep;
use std::time::Duration;

use client::start_client;




#[test]
fn run() {

    let _ = start_client();

    loop {
        sleep(Duration::from_secs(1))
    }
}
