//! non-blocking Net APIs with rust
//!
#![feature(fnbox)]
#![feature(pointer_methods)]
#![feature(fn_traits)]

extern crate mio;
extern crate slab;
extern crate mio_extras;
extern crate pi_lib;

mod net;
mod api;
mod data;
pub mod timer;

pub use api::NetManager;
pub use data::{Config, Socket, Stream, Protocol, CloseFn};
