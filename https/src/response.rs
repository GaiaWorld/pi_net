use std::fs::File;
use std::sync::Arc;
use std::io::{self, Write};
use std::fmt::{self, Debug};

use futures::*;
use npnc::bounded::spsc::{Producer, Consumer};

use http::StatusCode;
use headers::{self, HeaderMap};

use hyper::body::Body as ResBody;
use hyper::Error;

use modifier::{Modifier, Set};
use plugin::Extensible;
use typemap::TypeMap;

use {Body, HttpResponse, Plugin};

/*
* 响应时写body
*/
pub trait WriteBody: Send {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()>;
}

impl WriteBody for String {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()> {
        self.as_bytes().write_body(res)
    }
}

impl<'a> WriteBody for &'a str {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()> {
        self.as_bytes().write_body(res)
    }
}

impl WriteBody for Vec<u8> {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()> {
        res.write_all(self)
    }
}

impl<'a> WriteBody for &'a [u8] {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()> {
        res.write_all(self)
    }
}

impl WriteBody for File {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()> {
        io::copy(self, res).map(|_| ())
    }
}

impl WriteBody for Box<io::Read + Send> {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()> {
        io::copy(self, res).map(|_| ())
    }
}

/*
* body读取器
*/
pub struct BodyReader<R: Send>(pub R);

impl<R: io::Read + Send> WriteBody for BodyReader<R> {
    fn write_body(&mut self, res: &mut Write) -> io::Result<()> {
        io::copy(&mut self.0, res).map(|_| ())
    }
}

/*
* http服务的响应对象
*/
pub struct Response {
    pub status: Option<StatusCode>,
    pub headers: HeaderMap,
    pub extensions: TypeMap,
    pub body: Option<Box<WriteBody>>,
    pub sender: Option<Arc<Producer<Result<HttpResponse<ResBody>, Error>>>>, 
    pub receiver: Option<Arc<Consumer<task::Task>>>,
}

impl Default for Response {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            f,
            "HTTP/1.1 {}\n{:?}",
            self.status.unwrap_or(StatusCode::NOT_FOUND),
            self.headers
        )
    }
}

impl Extensible for Response {
    fn extensions(&self) -> &TypeMap {
        &self.extensions
    }

    fn extensions_mut(&mut self) -> &mut TypeMap {
        &mut self.extensions
    }
}

impl Plugin for Response {}
impl Set for Response {}

impl Response {
    //构建一个http响应
    pub fn new() -> Response {
        Response {
            status: None,
            headers: HeaderMap::new(),
            extensions: TypeMap::new(),
            body: None,
            sender: None,
            receiver: None,
        }
    }

    //使用修改器构建http响应
    pub fn with<M: Modifier<Response>>(m: M) -> Response {
        Response::new().set(m)
    }

    //写响应的body
    pub fn write_back(self, http_res: &mut HttpResponse<Body>) {
        *http_res.headers_mut() = self.headers;
        *http_res.status_mut() = self.status.unwrap_or(StatusCode::NOT_FOUND);

        let out = match self.body {
            Some(body) => write_with_body(http_res, body),
            None => {
                http_res.headers_mut().insert(
                    headers::CONTENT_LENGTH,
                    headers::HeaderValue::from_static("0"),
                );
                Ok(())
            }
        };

        if let Err(e) = out {
            panic!("Error writing response: {}", e);
        }
    }
}

//写body
fn write_with_body(res: &mut HttpResponse<Body>, mut body: Box<WriteBody>) -> io::Result<()> {
    let content_type = res.headers().get(headers::CONTENT_TYPE).map_or_else(
        || headers::HeaderValue::from_static("text/plain"),
        |cx| cx.clone(),
    );
    res.headers_mut()
        .insert(headers::CONTENT_TYPE, content_type);

    let mut body_contents: Vec<u8> = vec![];
    try!(body.write_body(&mut body_contents));
    *res.body_mut() = Body::from(body_contents);
    Ok(())
}