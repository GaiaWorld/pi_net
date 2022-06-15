#![allow(warnings)]
#![feature(range_is_empty)]
#![feature(no_more_cas)]

#[macro_use]
extern crate lazy_static;

pub mod server;
pub mod driver;
pub mod connect;
pub mod buffer_pool;
pub mod util;
pub mod tls_connect;
mod acceptor;
mod connect_pool;