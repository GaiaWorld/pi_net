#![deny(missing_docs)]
#![cfg_attr(feature = "nightly", feature(slice_align_to))]
#![cfg_attr(feature = "nightly", feature(test))]
#![cfg_attr(feature = "cargo-clippy", feature(tool_lints))]

//! A fast, low-overhead WebSocket client.
//!
//! This library is optimised for receiving a high volume of messages over a long period. A key feature is that it makes
//! no memory allocations once the connection is set up and the initial messages have been sent and received; it reuses
//! a single pair of buffers, which are sized for the longest message seen so far.
//!
//! Only asynchronous access is provided at present. `native_tls` provides the TLS functionality for `wss://...` servers.
//!
//! This crate is fully conformant with the fuzzingserver module in the
//! [Autobahn test suite](https://github.com/crossbario/autobahn-testsuite).
use std::error;
use std::result;

/// Represents errors that can be exposed by this crate.
pub type Error = Box<dyn error::Error + 'static>;

/// Represents results returned by the non-async functions in this crate.
pub type Result<T> = result::Result<T, Error>;

pub mod client;
pub mod frame;
pub mod mask;
pub mod message;
pub mod opcode;
pub mod ssl;
pub mod upgrade;

pub use self::client::{AsyncNetworkStream, Client, ClientBuilder};
pub use self::message::{Message, MessageCodec};
pub use self::opcode::Opcode;
pub use self::upgrade::UpgradeCodec;
