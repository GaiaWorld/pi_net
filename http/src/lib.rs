#![feature(mem_take)]
#![feature(never_type)]
#![feature(async_await)]
#![feature(entry_insert)]
#![feature(const_generics)]
#![feature(map_first_last)]
#![feature(associated_type_defaults)]

extern crate mio;
extern crate url;
extern crate mime;
extern crate twoway;
extern crate https;
extern crate httparse;
extern crate httpdate;
extern crate serde_json;
extern crate futures;
extern crate parking_lot;
extern crate crossbeam_channel;
extern crate base64;
extern crate flate2;
extern crate bytes;
extern crate path_absolutize;
extern crate log;

extern crate tcp;
extern crate handler;
extern crate hash;
extern crate gray;
extern crate file;
extern crate atom;
extern crate adler32;

pub mod server;
pub mod acceptor;
pub mod connect;
pub mod virtual_host;
pub mod gateway;
pub mod service;
pub mod route;
pub mod middleware;
pub mod cors_handler;
pub mod default_parser;
pub mod multi_parts;
pub mod file_load;
pub mod files_load;
pub mod batch_load;
pub mod upload;
pub mod port;
pub mod static_cache;
pub mod request;
pub mod response;
pub mod packet;
pub mod util;