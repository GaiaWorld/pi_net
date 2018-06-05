extern crate mqtt;
extern crate mqtt3;
extern crate net;
extern crate string_cache;

mod mqtt_client;
mod mqtt_server;

use std::io::{Error, ErrorKind, Result};
use std::thread::sleep;
use std::time::Duration;

use mqtt_client::start_client;
use mqtt_server::start_server;

use mqtt::{ClientNode, Server, ServerNode};
use net::{Config, NetManager, Protocol, Socket, Stream};
use string_cache::atom::DefaultAtom;

use mqtt3::{LastWill, QoS};
use std::env;

#[test]
fn run() {
    let s = start_server();
    loop {
        sleep(Duration::from_secs(1))
    }
}
