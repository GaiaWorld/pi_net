//! rpc 实现
//!

extern crate net;
extern crate mqtt_tmp;
extern crate mqtt3;
extern crate fnv;
extern crate atom;
extern crate handler;
extern crate compress;

pub mod traits;
pub mod server;
pub mod client;

use std::time::SystemTime;
fn now_millis() -> isize {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Err(e) => -1,
        Ok(n) => n.as_millis() as isize,
    }
}

