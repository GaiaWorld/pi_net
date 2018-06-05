//! non-blocking Net APIs with rust
//!
#![feature(fnbox)]
#![feature(pointer_methods)]

extern crate mio;
extern crate slab;
extern crate mio_extras;

mod net;
mod api;
mod data;
mod timer;

pub use api::NetManager;
pub use data::{Config, Socket, Stream, Protocol};
