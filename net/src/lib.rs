//! non-blocking Net APIs with rust
//!
#![feature(pointer_methods)]
#![feature(fn_traits)]
#![feature(proc_macro)]
#![feature(nll)]

extern crate mio;
extern crate slab;
extern crate mio_extras;
extern crate rand;
extern crate byteorder;
extern crate hyper;
extern crate sha1;
extern crate base64;
extern crate httparse;
extern crate websocket;
extern crate rustls;
extern crate fnv;
extern crate vecio;

extern crate bitflags;

#[macro_use]
extern crate lazy_static;

extern crate atom;
extern crate apm;
extern crate gray;
extern crate timer as lib_timer;


pub mod tls;
pub mod wss;
pub mod net;
pub mod api;
pub mod data;
pub mod timer;
pub mod ws;

pub use api::NetManager;
pub use data::{Config, RawSocket, RawStream, Protocol, CloseFn};
