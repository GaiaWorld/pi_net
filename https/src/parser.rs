use std::fmt;
use std::cmp;
use std::str;
use std::marker;
use std::any::Any;
use std::io::{self, Read, Error as IOError};
use std::error::Error as StdError;

use serde::Deserialize;
use serde_json::{self, from_str, from_value};
use typemap::Key;

use util;
use request::Request;
use mime;
use headers;
use Plugin;

/*
* 默认body限制大小，100MB
*/
const DEFAULT_BODY_LIMIT: usize = 100 * 1024 * 1024;

/*
* body错误选项
*/
#[derive(Debug)]
pub enum BodyErrorCause {
    Utf8Error(str::Utf8Error),
    IoError(io::Error),
    JsonError(serde_json::Error),
}

/*
* body错误
*/
#[derive(Debug)]
pub struct BodyError {
    pub detail: String,
    pub cause: BodyErrorCause
}

impl fmt::Display for BodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.description().fmt(formatter)
    }
}

impl StdError for BodyError {
    fn description(&self) -> &str {
        &self.detail[..]
    }

    fn cause(&self) -> Option<&StdError> {
        match self.cause {
            BodyErrorCause::Utf8Error(ref err) => Some(err),
            BodyErrorCause::IoError(ref err) => Some(err),
            BodyErrorCause::JsonError(ref err) => Some(err),
        }
    }
}

/*
* 受限读取器
*/
#[derive(Debug)]
pub struct LimitReader<R> {
    limit: usize,
    inner: R
}

impl<R: io::Read> LimitReader<R> {
    pub fn new(r: R, limit: usize) -> LimitReader<R> {
        LimitReader { limit: limit, inner: r }
    }

    pub fn into_inner(self) -> R { self.inner }
    pub fn limit(&self) -> usize { self.limit }
}

impl<R: io::Read> io::Read for LimitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.limit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Body is too big"))
        }

        let len = cmp::min(self.limit, buf.len());
        let res = self.inner.read(&mut buf[..len]);
        match res {
            Ok(len) => self.limit -= len,
            _ => {}
        }
        res
    }
}

/*
* http请求最大body大小
*/
pub struct MaxBodyLength;
impl Key for MaxBodyLength {
    type Value = usize;
}

/*
* http请求的raw body，即编码为utf8的字符串
*/
pub struct Raw;

impl Key for Raw {
    type Value = Option<String>;
}

impl<'a, 'b> plugin::Plugin<Request> for Raw {
    type Error = BodyError;

    fn eval(req: &mut Request) -> Result<Option<String>, BodyError> {
        let need_read = match req.headers.get(headers::CONTENT_TYPE) {
            None => true,
            Some(val) if val.is_empty() => true,
            Some(val) => {
                //http headers中有content type，且有值
                let vals: Vec<&str> = val.to_str().ok().unwrap().split(",").collect();
                match vals[0].parse::<mime::Mime>() {
                    Ok(ref m) => {
                        if (mime::MULTIPART_FORM_DATA.type_() == m.type_()) && (mime::MULTIPART_FORM_DATA.subtype() == m.subtype()) {
                            //如果是分段表单数据，如上传文件，则不是raw body
                            false
                        } else {
                            true
                        }
                    },
                    _ => true,
                }
            }
        };

        if need_read {
            //根据最大长度读取utf8字符串
            let max_length = req
                .get::<util::Read<MaxBodyLength>>()
                .ok()
                .map(|x| *x)
                .unwrap_or(DEFAULT_BODY_LIMIT);
            let body = try!(read_body_as_utf8(req, max_length));
            Ok(Some(body))
        } else {
            Ok(None)
        }
    }
}

//获取编码为utf8的无结构String
fn read_body_as_utf8(req: &mut Request, limit: usize) -> Result<String, BodyError> {
    let mut bytes = Vec::new();
    match req.get_body_contents() {
        Err(err) => Err(BodyError {
            detail: "read request body failed".to_string(),
            cause: BodyErrorCause::IoError(IOError::new(io::ErrorKind::Other, err.to_string()))
        }),
        Ok(body) => {
            //获取请求body成功
            if body.is_empty() {
                //body为空
                Ok(req.query.clone())
            } else {
                match LimitReader::new(body.as_slice(), limit).read_to_end(&mut bytes) {
                    Ok(_) => {
                        match String::from_utf8(bytes) {
                            Ok(b) => Ok(b),
                            Err(err) => Err(BodyError {
                                detail: "Invalid UTF-8 sequence".to_string(),
                                cause: BodyErrorCause::Utf8Error(err.utf8_error())
                            }),
                        }
                    },
                    Err(err) => Err(BodyError {
                        detail: "Can't read request body".to_string(),
                        cause: BodyErrorCause::IoError(err)
                    }),
                }
            }       
        },
    }
}

/*
* http请求的json body，即编码为utf8的json
*/
#[derive(Clone)]
pub struct Json;

impl Key for Json {
    type Value = Option<serde_json::Value>;
}

impl<'a, 'b> plugin::Plugin<Request> for Json {
    type Error = BodyError;

    fn eval(req: &mut Request) -> Result<Option<serde_json::Value>, BodyError> {
        req.get::<Raw>()
            .and_then(|maybe_body| {
                reverse_option(maybe_body.map(|body| from_str(&body)))
                    .map_err(|err| {
                        BodyError {
                            detail: "Can't parse body to JSON".to_string(),
                            cause: BodyErrorCause::JsonError(err)
                        }
                    })
            })
    }
}

/*
* http请求的kv pairs body，即编码为utf8的键值对
*/
pub struct Struct<T> where T: for<'a> Deserialize<'a> {
    marker: marker::PhantomData<T>
}

impl<T> Key for Struct<T> where T: for<'a> Deserialize<'a> + Any {
    type Value = Option<T>;
}

impl<'a, 'b, T> plugin::Plugin<Request> for Struct<T>
    where T: for<'c> Deserialize<'c> + Any {
        type Error = BodyError;

        fn eval(req: &mut Request) -> Result<Option<T>, BodyError> {
            req.get::<Json>()
                .and_then(|maybe_body| {
                    reverse_option(maybe_body.map(|body| from_value(body)))
                        .map_err(|err| BodyError {
                            detail: "Can't parse body to the struct".to_string(),
                            cause: BodyErrorCause::JsonError(err)
                        })
                })
        }
}

//将Option转换为Result
fn reverse_option<T,E>(value: Option<Result<T, E>>) -> Result<Option<T>, E> {
    match value {
        Some(Ok(val)) => Ok(Some(val)),
        Some(Err(err)) => Err(err),
        None => Ok(None),
    }
}
