extern crate mqtt;
extern crate mqtt3;
extern crate net;
extern crate pi_lib;

mod mqtt_client;
mod mqtt_server;

use std::io::{Error, ErrorKind, Result};
use std::thread::sleep;
use std::time::Duration;

use mqtt_client::start_client;
use mqtt_server::start_server;

use mqtt::client::ClientNode;
use mqtt::server::ServerNode;
use mqtt::data::Server;
use net::{Config, NetManager, Protocol, Socket, Stream};

use mqtt3::{LastWill, QoS};
use std::env;


#[test]
fn run() {

    let _ = start_client();

    loop {
        sleep(Duration::from_secs(1))
    }
}
