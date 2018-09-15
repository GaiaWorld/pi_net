use std::fmt;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::collections::hash_map::Entry::*;

use typemap::Key;
use url::form_urlencoded;

use parser::{BodyError, Raw};
use request::Request;
use Plugin;

/*
* url编码的URL query string
*/
pub struct UrlEncodedQuery;

/*
* url编码的URL body
*/
pub struct UrlEncodedBody;

/*
* url解码错误
*/
#[derive(Debug)]
pub enum UrlDecodingError{
    BodyError(BodyError),   //分析请求body错误
    EmptyQuery              //空请求
}

impl fmt::Display for UrlDecodingError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.description().fmt(f)
    }
}

impl StdError for UrlDecodingError {
    fn description(&self) -> &str {
        match *self {
            UrlDecodingError::BodyError(ref err) => err.description(),
            UrlDecodingError::EmptyQuery => "Expected query, found empty string"
        }
    }

    fn cause(&self) -> Option<&StdError> {
        match *self {
            UrlDecodingError::BodyError(ref err) => Some(err),
            _ => None
        }
    }
}

/// Hashmap mapping strings to vectors of strings.
pub type QueryMap = HashMap<String, Vec<String>>;
/// Result type for decoding query parameters.
pub type QueryResult = Result<QueryMap, UrlDecodingError>;

impl Key for UrlEncodedBody {
    type Value = QueryMap;
}
impl Key for UrlEncodedQuery {
    type Value = QueryMap;
}

impl<'a, 'b> plugin::Plugin<Request> for UrlEncodedQuery {
    type Error = UrlDecodingError;

    fn eval(req: &mut Request) -> QueryResult {
        match req.url.query() {
            Some(ref query) => create_param_hashmap(&query),
            None => Err(UrlDecodingError::EmptyQuery)
        }
    }
}

impl<'a, 'b> plugin::Plugin<Request> for UrlEncodedBody {
    type Error = UrlDecodingError;

    fn eval(req: &mut Request) -> QueryResult {
        req.get::<Raw>()
            .map(|x| x.unwrap_or("".to_string()))
            .map_err(|e| UrlDecodingError::BodyError(e))
            .and_then(|x| create_param_hashmap(&x))
    }
}

/*
* 根据query string构建参数表
*/
fn create_param_hashmap(data: &str) -> QueryResult {
    match data {
        "" => Err(UrlDecodingError::EmptyQuery),
        _ => Ok(combine_duplicates(form_urlencoded::parse(data.as_bytes()).into_owned()))
    }
}

/*
* 将kv pairs向量转换为参数表
*/
fn combine_duplicates<I: Iterator<Item=(String, String)>>(collection: I) -> QueryMap {
    let mut deduplicated: QueryMap = HashMap::new();
    for (k, v) in collection {
        match deduplicated.entry(k) {
            Occupied(entry) => {
                //key存在，则修改值
                entry.into_mut().push(v);
            },
            Vacant(entry) => {
                //key不存在，则插入
                entry.insert(vec![v]);
            },
        };
    }
    deduplicated
}