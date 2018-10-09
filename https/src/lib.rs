#![feature(nll)]
#![feature(fnbox)]
#![feature(extern_prelude)]
#![feature(type_ascription)]

pub use std::error::Error;

extern crate num;
extern crate serde;
extern crate serde_json;
extern crate npnc;
extern crate futures;

extern crate hyper;
pub use hyper::header as headers;
pub use hyper::{Uri, Body};
pub use hyper::header::HeaderValue;
pub use hyper::Request as HttpRequest;
pub use hyper::Response as HttpResponse;
pub use hyper::error::Result as HttpResult;
pub use hyper::Error as HttpError;

extern crate http;
pub use http::method;
pub use http::Method;

extern crate url as url_ext;
pub mod url {
    pub use url_ext::*;
}

extern crate mime;
extern crate mime_guess;

pub mod modifier {
    extern crate modifier as modfier;
    pub use self::modfier::*;
}

extern crate plugin;
pub use plugin::Pluggable as Plugin;

extern crate typemap as tmap;
pub mod typemap {
    pub use tmap::{Key, TypeMap};
}

extern crate sequence_trie;

extern crate time;
extern crate filetime;
extern crate tempdir;
extern crate void;
extern crate twoway;

extern crate pi_lib;
extern crate pi_base;

pub mod https_impl;
pub mod modifiers;
pub mod parser;
pub mod params;
pub mod request;
pub mod response;
pub mod handler;
pub mod mount;
pub mod https;
pub mod file_path;
pub mod file;
pub mod files;
pub mod upload;
pub mod url_encode;
pub mod util;
pub mod https_load_files;