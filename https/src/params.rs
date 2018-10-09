use std::fmt;
use std::str::FromStr;
use std::path::PathBuf;
use std::ops::{Deref, DerefMut};
use std::io::{self, Error as IOError, Result as IOResult, Read};
use std::collections::BTreeMap;
use std::error::Error as StdError;

use num::NumCast;
use typemap::Key;
use serde_json::value::Value as JsonValue;
use twoway::find_bytes;

use Plugin;
use headers;
use mime::{self, Mime};
use request::Request;
use parser::{BodyError, Json};
use url_encode::{UrlDecodingError, UrlEncodedQuery, UrlEncodedBody, QueryMap};
use util::split_bytes;

/*
* http换行符，\r\n
*/
const HTTP_LINE_BREAK: &[u8] = &[13, 10];

/*
* form换行符，\r\n\r\n
*/
const FORM_LINE_BREAK: &[u8] = &[13, 10, 13, 10];

/*
* form结尾符，--\r\n
*/
const FORM_TAIL: &[u8] = &[45, 45, 13, 10];

/*
* boundary前缀
*/
const BOUNDARY_PREFIX: &str = "--";

/*
* 表单数据头一级分隔符，;
*/
const FORM_DATA_HEAD_FIRST_SPLIT_CHAR: &[u8] = &[59];

/*
* 表单数据头二级分隔符
*/
const FORM_DATA_HEAD_SECOND_SPLIT_CHAR: &str = "=";

/*
* 表单数据头
*/
const FORM_DATA_HEAD: &str = "content-disposition: form-data";

/*
* 默认文件名关键字
*/
const FORM_DATA_FILE_KEY: &str = "$file_name";

/*
* 假值字符串
*/
static FALSE_STRINGS: &'static [&'static str] = &["0", "f", "F", "false", "FALSE", "off", "OFF"];

/*
* 真值字符串
*/
static TRUE_STRINGS: &'static [&'static str] = &["1", "t", "T", "true", "TRUE", "on", "ON"];

/*
* 参数值转换接口
*/
pub trait FromValue: Sized {
    //值转换方法，失败返回None
    fn from_value(value: &Value) -> Option<Self>;
}

/*
* 为布尔值实现值转换接口
*/
impl FromValue for bool {
    fn from_value(value: &Value) -> Option<bool> {
        match *value {
            Value::Boolean(value) => Some(value),
            Value::I64(value) if value == 0 => Some(false),
            Value::I64(value) if value == 1 => Some(true),
            Value::U64(value) if value == 0 => Some(false),
            Value::U64(value) if value == 1 => Some(true),
            Value::String(ref value) if FALSE_STRINGS.contains(&&value[..]) => Some(false),
            Value::String(ref value) if TRUE_STRINGS.contains(&&value[..]) => Some(true),
            _ => None,
        }
    }
}

/*
* 为rust数值类型实现值转换接口的宏，用于将http数值字符串，转换为对应的数值
*/
macro_rules! num_from_value {
    ($ty:ty) => {
        impl FromValue for $ty {
            fn from_value(value: &Value) -> Option<$ty> {
                match *value {
                    Value::I64(value) => NumCast::from(value),
                    Value::U64(value) => NumCast::from(value),
                    Value::F64(value) => NumCast::from(value),
                    Value::String(ref value) => FromStr::from_str(value).ok(),
                    _ => None,
                }
            }
        }
    }
}

/*
* 为常用数值类型实现值转换接口
*/
num_from_value!(u8);
num_from_value!(u16);
num_from_value!(u32);
num_from_value!(u64);
num_from_value!(usize);
num_from_value!(i8);
num_from_value!(i16);
num_from_value!(i32);
num_from_value!(i64);
num_from_value!(isize);
num_from_value!(f32);
num_from_value!(f64);

/*
* 为字符串实现值转换接口
*/
impl FromValue for String {
    fn from_value(value: &Value) -> Option<String> {
        match *value {
            Value::Null => Some(String::new()),
            Value::Boolean(value) => Some(value.to_string()),
            Value::I64(value) => Some(value.to_string()),
            Value::U64(value) => Some(value.to_string()),
            Value::F64(value) => Some(value.to_string()),
            Value::String(ref value) => Some(value.clone()),
            _ => None,
        }
    }
}

/*
* 为可空值实现值转换接口
*/
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Option<Option<T>> {
        match *value {
            Value::Null => Some(None),
            _ => T::from_value(value).map(Some),
        }
    }
}

/*
* 为数组实现值转换接口
*/
impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(value: &Value) -> Option<Vec<T>> {
        match *value {
            Value::Array(ref array) => {
                let mut vec = Vec::with_capacity(array.len());
                for value in array {
                    match T::from_value(value) {
                        Some(value) => vec.push(value),
                        None => return None,
                    }
                }
                Some(vec)
            },
            _ => None,
        }
    }
}

/*
* 为表实现值转换接口
*/
impl<T: FromValue> FromValue for BTreeMap<String, T> {
    fn from_value(value: &Value) -> Option<BTreeMap<String, T>> {
        match *value {
            Value::Map(ref map) => map.to_strict_map(),
            _ => None,
        }
    }
}

/*
* 参数错误
*/
#[derive(Debug)]
pub enum ParamsError {
    BodyError(BodyError),               //解析body错误
    UrlDecodingError(UrlDecodingError), //解析url错误
    IoError(IOError),                   //读来自multipart/form-data的临时文件错误
    InvalidPath,                        //无效的参数路径
    CannotAppend,                       //在非数组参数值上append
    CannotInsert,                       //在非表参数值上插入
    NotJsonObject,                      //在非根json对象上构建表
    InvalidForm,                        //无效的表单
    InvalidFile,                        //无效的上传文件
}

impl fmt::Display for ParamsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.description().fmt(f)
    }
}

impl StdError for ParamsError {
    fn description(&self) -> &str {
        match *self {
            ParamsError::BodyError(ref err) => err.description(),
            ParamsError::UrlDecodingError(ref err) => err.description(),
            ParamsError::IoError(ref err) => err.description(),
            ParamsError::InvalidPath => "Invalid parameter path",
            ParamsError::CannotAppend => "Cannot append to a non-array value",
            ParamsError::CannotInsert => "Cannot insert into a non-map value",
            ParamsError::NotJsonObject => "Tried to make a `Map` from a non-object root JSON value",
            ParamsError::InvalidForm => "Invalid form part",
            ParamsError::InvalidFile => "Save upload file failed",
        }
    }

    fn cause(&self) -> Option<&StdError> {
        match *self {
            ParamsError::BodyError(ref err) => Some(err),
            ParamsError::UrlDecodingError(ref err) => Some(err),
            ParamsError::IoError(ref err) => Some(err),
            _ => None,
        }
    }
}

impl From<BodyError> for ParamsError {
    fn from(err: BodyError) -> ParamsError {
        ParamsError::BodyError(err)
    }
}

impl From<IOError> for ParamsError {
    fn from(err: IOError) -> ParamsError {
        ParamsError::IoError(err)
    }
}

/*
* 文件参数
*/
#[derive(Clone, Debug)]
pub struct File {
    pub path: PathBuf,              //文件临时路径
    pub filename: Option<String>,   //文件名，注意没有约束文件名为本地文件名
    pub size: u64,                  //文件大小
    pub content_type: Mime,         //上传mime，注意这个值可以由客户端任意更改
}

/*
* 为文件参数实现值转换接口
*/
impl FromValue for File {
    fn from_value(value: &Value) -> Option<File> {
        match *value {
            Value::File(ref file) => Some(file.clone()),
            _ => None,
        }
    }
}

impl File {
    //以只读模式异步打开文件
    // pub fn open(&self) -> io::Result<fs::File> {
    //     fs::File::open(&self.path)
    // }
}

impl PartialEq for File {
    fn eq(&self, other: &File) -> bool {
        self.path == other.path //只检查文件路径是否相等
    }
}

/*
* 表参数
*/
#[derive(Clone, PartialEq, Default)]
pub struct Map(pub BTreeMap<String, Value>);

impl Deref for Map {
    type Target = BTreeMap<String, Value>;

    fn deref(&self) -> &BTreeMap<String, Value> {
        &self.0
    }
}

impl DerefMut for Map {
    fn deref_mut(&mut self) -> &mut BTreeMap<String, Value> {
        &mut self.0
    }
}

impl fmt::Debug for Map {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}


impl Map {
    //构建一个空表
    pub fn new() -> Map {
        Map(BTreeMap::new())
    }

    //将指定的参数值插入表的指定键路径，键路径：name, name[], name[][x]
    pub fn assign(&mut self, path: &str, value: Value) -> Result<(), ParamsError> {
        let (base, remainder) = try!(parse_base(path));
        if remainder.is_empty() {
            self.0.insert(String::from(base), value);
            return Ok(());
        }

        let (key, _) = try!(parse_index(remainder));
        let collection = self.0.entry(String::from(base)).or_insert_with(|| {
            if key.is_empty() {
                Value::Array(vec![])
            } else {
                Value::Map(Map::new())
            }
        });
        try!(collection.assign(remainder, value));
        Ok(())
    }

    //递归遍历查找指定键的值，键路径：&[name, x]
    pub fn find(&self, keys: &[&str]) -> Option<&Value> {
        if keys.is_empty() {
            return None;
        }

        let mut value = self.0.get(keys[0]);
        for key in &keys[1..] {
            value = match value {
                Some(&Value::Map(ref map)) => map.0.get(*key),
                _ => return None,
            }
        }
        value
    }

    //将表转换为BtreeMap
    pub fn to_strict_map<T: FromValue>(&self) -> Option<BTreeMap<String, T>> {
        let mut map = BTreeMap::new();

        for (key, value) in &self.0 {
            if let Some(converted_value) = T::from_value(value) {
                map.insert(key.clone(), converted_value);
            } else {
                return None;
            }
        }

        Some(map)
    }
}

//分析键路径中的括号
fn parse_base(path: &str) -> Result<(&str, &str), ParamsError> {
    let length = path.len();
    let open = path.find('[').unwrap_or(length);
    let (base, remainder) = path.split_at(open);
    if base.is_empty() {
        Err(ParamsError::InvalidPath)
    } else {
        Ok((base, remainder))
    }
}

//分析键路径中的索引
fn parse_index(path: &str) -> Result<(&str, &str), ParamsError> {
    if !path.starts_with('[') {
        return Err(ParamsError::InvalidPath);
    }
    let close = try!(path.find(']').ok_or(ParamsError::InvalidPath));
    let key = &path[1..close];
    let remainder = &path[1 + close..];

    Ok((key, remainder))
}

/*
* 参数值
*/
#[derive(Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Bin(Vec<u8>),
    File(File),
    Array(Vec<Value>),
    Map(Map),
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Value::Null => f.write_str("null"),
            Value::Boolean(value) => value.fmt(f),
            Value::I64(value) => value.fmt(f),
            Value::U64(value) => value.fmt(f),
            Value::F64(value) => value.fmt(f),
            Value::String(ref value) => value.fmt(f),
            Value::Bin(ref value) => value.fmt(f),
            Value::File(ref value) => value.fmt(f),
            Value::Array(ref value) => value.fmt(f),
            Value::Map(ref value) => value.fmt(f),
        }
    }
}

/*
* 为参数值实现值转换接口
*/
impl FromValue for Value {
    fn from_value(value: &Value) -> Option<Self> {
        Some(value.clone())
    }
}

impl Value {
    //将指定的参数值插入参数值的指定键路径
    fn assign(&mut self, path: &str, value: Value) -> Result<(), ParamsError> {
        if path.is_empty() {
            return Err(ParamsError::InvalidPath);
        }

        let (key, remainder) = try!(parse_index(path));
        if key.is_empty() {
            match *self {
                Value::Array(ref mut array) => {
                    if remainder.is_empty() {
                        array.push(value);
                        return Ok(());
                    }

                    let (next_key, _) = try!(parse_index(remainder));
                    if next_key.is_empty() {
                        return Err(ParamsError::InvalidPath); //键路径中连续两个数组索引是非法的，如："[][]"
                    }
                    if let Some(map) = array.last_mut() {
                        if !try!(map.contains_key(next_key)) {
                            return map.assign(remainder, value);
                        }
                    }

                    let mut map = Value::Map(Map::new());
                    try!(map.assign(remainder, value));
                    array.push(map);
                    Ok(())
                },
                _ => Err(ParamsError::CannotAppend),
            }
        } else {
            match *self {
                Value::Map(ref mut map) => {
                    if remainder.is_empty() {
                        map.0.insert(String::from(key), value);
                        return Ok(());
                    }

                    let (next_key, _) = try!(parse_index(remainder));
                    let collection = map.0.entry(String::from(key)).or_insert_with(|| {
                        if next_key.is_empty() {
                            Value::Array(vec![])
                        } else {
                            Value::Map(Map::new())
                        }
                    });

                    collection.assign(remainder, value)
                },
                _ => Err(ParamsError::CannotInsert),
            }
        }
    }

    //检查指定的键是否存在
    fn contains_key(&mut self, key: &str) -> Result<bool, ParamsError> {
        match *self {
            Value::Map(ref map) => Ok(map.contains_key(key)),
            _ => Err(ParamsError::CannotInsert),
        }
    }
}

/*
* 转换为参数列表
*/
trait ToParams {
    //将当前对象转换为表参数
    fn to_map(&self) -> Result<Map, ParamsError>;
    //将当对象转换为参数值
    fn to_value(&self) -> Result<Value, ParamsError>;
}

impl ToParams for JsonValue {
    fn to_map(&self) -> Result<Map, ParamsError> {
        match try!(self.to_value()) {
            Value::Map(map) => Ok(map),
            _ => Err(ParamsError::NotJsonObject),
        }
    }

    fn to_value(&self) -> Result<Value, ParamsError> {
        match *self {
            JsonValue::Number(ref number) if number.is_u64() => Ok(Value::U64(number.as_u64().unwrap())),
            JsonValue::Number(ref number) if number.is_i64() => Ok(Value::I64(number.as_i64().unwrap())),
            JsonValue::Number(ref number) => Ok(Value::F64(number.as_f64().unwrap())),
            JsonValue::String(ref value) => Ok(Value::String(value.clone())),
            JsonValue::Bool(value) => Ok(Value::Boolean(value)),
            JsonValue::Null => Ok(Value::Null),
            JsonValue::Array(ref value) => {
                let result = value.iter().map(|v| v.to_value()).collect();
                Ok(Value::Array(try!(result)))
            },
            JsonValue::Object(ref value) => {
                let mut result = Map::new();
                for (key, json) in value {
                    result.insert(key.clone(), try!(json.to_value()));
                }

                Ok(Value::Map(result))
            },
        }
    }
}

/*
* 参数列表，支持以下参数类型，基本上除了text/xml都支持
* JSON data (`Content-Type: application/json`)
* URL-encoded GET parameters
* URL-encoded `Content-Type: application/x-www-form-urlencoded` parameters
* Multipart form data (`Content-Type: multipart/form-data`)
*/
pub struct Params;

impl Key for Params {
    type Value = Map;
}

impl<'a, 'b> plugin::Plugin<Request> for Params {
    type Error = ParamsError;

    fn eval(req: &mut Request) -> Result<Map, ParamsError> {
        let mut map = try!(try_parse_json_into_map(req));
        let has_json_body = !map.is_empty(); //是否是json数据
        try!(try_parse_multipart(req, &mut map)); //解析分段表单
        try!(try_parse_url_encode::<UrlEncodedQuery>(req, &mut map));  //解析query string
        if !has_json_body {
            try!(try_parse_url_encode::<UrlEncodedBody>(req, &mut map)); //解析body
        }
        Ok(map)
    }
}

//尝试分析json并转换到表
fn try_parse_json_into_map(req: &mut Request) -> Result<Map, ParamsError> {
    let need_parse = match req.headers.get(headers::CONTENT_TYPE) {
            None => false,
            Some(val) if val.is_empty() => false,
            Some(val) => {
                //http headers中有content type，且有值
                let vals: Vec<&str> = val.to_str().ok().unwrap().split(",").collect();
                match vals[0].parse::<mime::Mime>() {
                    Ok(ref m) => {
                        if (mime::APPLICATION_JSON.type_() == m.type_()) && (mime::APPLICATION_JSON.subtype() == m.subtype()) {
                            //如果是json数据
                            true
                        } else {
                            false
                        }
                    },
                    _ => false,
                }
            }
        };
    if !need_parse {
        //不是json数据，则返回空表
        return Ok(Map::new());
    }

    match *try!(req.get_ref::<Json>()) {
        Some(ref json) => json.to_map(),
        None => Ok(Map::new()),
    }
}

//尝试分析分段表单
fn try_parse_multipart(req: &mut Request, map: &mut Map) -> Result<(), ParamsError>
{
    if let Some(boundary) = multipart_boundary(req) {
        let mut data = Vec::new();
        match read_form(req, &mut data) {
            Err(e) => Err(ParamsError::IoError(e)),
            Ok(0) => Ok(()), //表单长度为0，则忽略
            Ok(_len) => {
                let _size = try!(parse_multipart(boundary.as_str(), &data[..], map));
                Ok(())
            },
        }
    } else {
        //没有边界，则忽略
        Ok(())
    }
}

//获取分段表单边界
fn multipart_boundary(req: &Request) -> Option<String> {
    match req.headers.get(headers::CONTENT_TYPE) {
        None => None,
        Some(content) if content.is_empty() => None,
        Some(content) => {
            //http headers中有content type，且有值
            let contents: Vec<&str> = content.to_str().ok().unwrap().split(",").collect();
            match contents[0].parse::<mime::Mime>() {
                Ok(m) => {
                    if (mime::MULTIPART_FORM_DATA.type_() == m.type_()) && (mime::MULTIPART_FORM_DATA.subtype() == m.subtype()) {
                        //如果是分段表单数据，则获取分段边界
                        m.get_param("boundary").map(|b| BOUNDARY_PREFIX.to_string() + b.as_str() )
                    } else {
                        None
                    }
                },
                _ => None,
            }
        }
    }
}

//读取表单和长度
fn read_form(req: &mut Request, buf: &mut Vec<u8>) -> IOResult<usize> {
    match req.get_body_contents() {
        Err(err) => Err(IOError::new(io::ErrorKind::Other, err.to_string())),
        Ok(body) => {
            //获取请求body成功
            buf.resize(body.len(), 0);
            body.as_slice().read(&mut buf[..])
        },
    }
}

//分析表单数据
fn parse_multipart(boundary: &str, data: &[u8], map: &mut Map) -> Result<(), ParamsError> {
    for part in split_bytes(data, boundary.as_bytes()) {
        if part == FORM_TAIL {
            //忽略结尾符
            continue;
        }
        
        if let Some(pos) = find_bytes(part, FORM_LINE_BREAK) {
            let (prefix, content) = part.split_at(pos + 4);
            let fields = split_bytes(prefix, HTTP_LINE_BREAK);
            let len = fields.len();
            if len == 2 {
                try!(parse_form_text(fields[0], content, map));
            } else if len == 3 {
                let (slice, _) = content.split_at(content.len() - 2);
                try!(parse_form_binary(fields[0], fields[1], slice, map));
            } else {
                return Err(ParamsError::InvalidForm);
            }
        } else {
            return Err(ParamsError::InvalidForm);
        }
    }
    Ok(())
}

//分析表单文本数据
fn parse_form_text(prefix: &[u8], value: &[u8], map: &mut Map) -> Result<(), ParamsError> {
    let vec = split_bytes(prefix, FORM_DATA_HEAD_FIRST_SPLIT_CHAR);
    if vec.len() != 2 {
        return Err(ParamsError::InvalidForm);
    }

    if let Ok(h) = String::from_utf8(Vec::from(vec[0])) {
        if let FORM_DATA_HEAD = h.trim().to_lowercase().as_str() {
            //分析头正确
            if let Ok(k) = String::from_utf8(Vec::from(vec[1])) {
                let key = k.trim().split(FORM_DATA_HEAD_SECOND_SPLIT_CHAR).collect::<Vec<&str>>()[1].trim_matches('\"');
                if let Ok(v) = String::from_utf8(Vec::from(&value[0..(value.len() - 2)])) {
                    return map.assign(key, Value::String(v));
                }
            }
        }
    }
    Err(ParamsError::InvalidForm)
}

//分析表单二进制数据
fn parse_form_binary(prefix: &[u8], content_type: &[u8], value: &[u8], map: &mut Map) -> Result<(), ParamsError> {
    let vec = split_bytes(prefix, FORM_DATA_HEAD_FIRST_SPLIT_CHAR);
    if vec.len() != 3 {
        return Err(ParamsError::InvalidForm);
    }

    if let Ok(h) = String::from_utf8(Vec::from(vec[0])) {
        if let FORM_DATA_HEAD = h.trim().to_lowercase().as_str() {
            //分析头正确
            if let Ok(k) = String::from_utf8(Vec::from(vec[1])) {
                let key = k.trim().split(FORM_DATA_HEAD_SECOND_SPLIT_CHAR).collect::<Vec<&str>>()[1].trim_matches('\"');
                try!(map.assign(key, Value::Bin(value.to_vec())));
            }

            if let Ok(k) = String::from_utf8(Vec::from(vec[2])) {
                let value = k.trim().split(FORM_DATA_HEAD_SECOND_SPLIT_CHAR).collect::<Vec<&str>>()[1].trim_matches('\"');
                if let Ok(v) = String::from_utf8(Vec::from(value)) {
                    return map.assign(FORM_DATA_FILE_KEY, Value::String(v));
                }
            }       
        }
    }
    Err(ParamsError::InvalidForm)
}

//尝试解析编码的url
fn try_parse_url_encode<'a, 'b, P>(req: &mut Request, map: &mut Map) -> Result<(), ParamsError>
    where P: plugin::Plugin<Request, Error = UrlDecodingError>,
          P: Key<Value = QueryMap>
{
    let hash_map = match req.get::<P>() {
        Ok(hash_map) => hash_map,
        Err(UrlDecodingError::EmptyQuery) => return Ok(()),
        Err(e) => return Err(ParamsError::UrlDecodingError(e)),
    };
    for (path, vec) in hash_map {
        for value in vec {
            try!(map.assign(&path, Value::String(value)));
        }
    }
    Ok(())
}