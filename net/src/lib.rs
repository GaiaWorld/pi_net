//! non-blocking Net APIs with rust
//!
#![feature(fnbox)]
#![feature(pointer_methods)]

extern crate mio;
extern crate slab;

mod net;
mod api;
mod data;

pub use api::NetManager;
pub use data::{Config, Socket, Stream, Protocol};
