//! non-blocking Net APIs with rust
//!
#![feature(fnbox)]
#![feature(pointer_methods)]
#![feature(fn_traits)]
#![feature(proc_macro)]

extern crate mio;
extern crate slab;
extern crate mio_extras;
extern crate atom;
extern crate gray;
extern crate rand;
extern crate byteorder;
extern crate hyper;
extern crate sha1;
extern crate base64;
extern crate httparse;
extern crate websocket;

extern crate bitflags;


pub mod net;
pub mod api;
pub mod data;
pub mod timer;
pub mod ws;

pub use api::NetManager;
pub use data::{Config, Socket, Stream, Protocol, CloseFn};
