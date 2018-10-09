use std::fmt;
use std::error::Error;
use std::sync::{Arc, RwLock, Mutex};

use plugin::Plugin;
use typemap::Key;
use twoway::find_bytes;

use request::Request;
use response::Response;
use handler::{BeforeMiddleware, AfterMiddleware, HttpsResult};

/*
* 状态共享错误
*/
#[derive(Clone, Debug)]
pub enum PersistentError {
    NotFound
}

impl Error for PersistentError {
    fn description(&self) -> &str {
        match *self {
            PersistentError::NotFound => "Value not found in extensions."
        }
    }
}

impl fmt::Display for PersistentError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.description().fmt(f)
    }
}

/*
* 状态共享接口
*/
pub trait PersistentInto<T> {
    fn persistent_into(self) -> T;
}

impl<T> PersistentInto<T> for T {
    fn persistent_into(self) -> T { self }
}

impl<T> PersistentInto<Arc<T>> for T {
    fn persistent_into(self) -> Arc<T> {
        Arc::new(self)
    }
}

impl<T> PersistentInto<Arc<Mutex<T>>> for T {
    fn persistent_into(self) -> Arc<Mutex<T>> {
        Arc::new(Mutex::new(self))
    }
}

impl<T> PersistentInto<Arc<RwLock<T>>> for T {
    fn persistent_into(self) -> Arc<RwLock<T>> {
        Arc::new(RwLock::new(self))
    }
}

/*
* 只读共享状态
*/
pub struct Read<P: Key> {
    data: Arc<P::Value>
}

impl<P: Key> Clone for Read<P> where P::Value: Send + Sync {
    fn clone(&self) -> Read<P> {
        Read { data: self.data.clone() }
    }
}

impl<P: Key> Key for Read<P> where P::Value: 'static {
    type Value = Arc<P::Value>;
}

impl<'a, 'b, P: Key> Plugin<Request> for Read<P> where P::Value: Send + Sync {
    type Error = PersistentError;
    fn eval(req: &mut Request) -> Result<Arc<P::Value>, PersistentError> {
        req.extensions.get::<Read<P>>().cloned().ok_or(PersistentError::NotFound)
    }
}

impl<P: Key> BeforeMiddleware for Read<P> where P::Value: Send + Sync {
    fn before(&self, mut req: Request) -> (Request, HttpsResult<()>) {
        req.extensions.insert::<Read<P>>(self.data.clone());
        (req, Ok(()))
    }
}

impl<P: Key> AfterMiddleware for Read<P> where P::Value: Send + Sync {
    fn after(&self, req: Request, mut res: Response) -> (Request, Response, HttpsResult<()>) {
        res.extensions.insert::<Read<P>>(self.data.clone());
        (req, res, Ok(()))
    }
}

impl<P: Key> Read<P> where P::Value: Send + Sync {
    //构建用于预处理和后处理的只读共享状态
    pub fn both<T>(start: T) -> (Read<P>, Read<P>) where T: PersistentInto<Arc<P::Value>> {
        let x = Read { data: start.persistent_into() };
        (x.clone(), x)
    }

    //构建用于预处理或后处理的只读共享状态
    pub fn one<T>(start: T) -> Read<P> where T: PersistentInto<Arc<P::Value>> {
        Read { data: start.persistent_into() }
    }
}

/*
* 多读少写共享状态
*/
pub struct State<P: Key> {
    data: Arc<RwLock<P::Value>>
}

impl<P: Key> Clone for State<P> where P::Value: Send + Sync {
    fn clone(&self) -> State<P> {
        State { data: self.data.clone() }
    }
}

impl<P: Key> Key for State<P> where P::Value: 'static {
    type Value = Arc<RwLock<P::Value>>;
}

impl<'a, 'b, P: Key> Plugin<Request> for State<P> where P::Value: Send + Sync {
    type Error = PersistentError;
    fn eval(req: &mut Request) -> Result<Arc<RwLock<P::Value>>, PersistentError> {
        req.extensions.get::<State<P>>().cloned().ok_or(PersistentError::NotFound)
    }
}

impl<P: Key> BeforeMiddleware for State<P> where P::Value: Send + Sync {
    fn before(&self, mut req: Request) -> (Request, HttpsResult<()>) {
        req.extensions.insert::<State<P>>(self.data.clone());
        (req, Ok(()))
    }
}

impl<P: Key> AfterMiddleware for State<P> where P::Value: Send + Sync {
    fn after(&self, req: Request, mut res: Response) -> (Request, Response, HttpsResult<()>) {
        res.extensions.insert::<State<P>>(self.data.clone());
        (req, res, Ok(()))
    }
}

impl<P: Key> State<P> where P::Value: Send + Sync {
    //构建用于预处理和后处理的多读少写共享状态
    pub fn both<T>(start: T) -> (State<P>, State<P>) where T: PersistentInto<Arc<RwLock<P::Value>>> {
        let x = State { data: start.persistent_into() };
        (x.clone(), x)
    }

    //构建用于预处理或后处理的多读少写共享状态
    pub fn one<T>(start: T) -> State<P> where T: PersistentInto<Arc<RwLock<P::Value>>> {
        State { data: start.persistent_into() }
    }
}

/*
* 多写少读共享状态
*/
pub struct Write<P: Key> {
    data: Arc<Mutex<P::Value>>
}

impl<P: Key> Clone for Write<P> where P::Value: Send {
    fn clone(&self) -> Write<P> {
        Write { data: self.data.clone() }
    }
}

impl<P: Key> Key for Write<P> where P::Value: 'static {
    type Value = Arc<Mutex<P::Value>>;
}

impl<'a, 'b, P: Key> Plugin<Request> for Write<P> where P::Value: Send {
    type Error = PersistentError;
    fn eval(req: &mut Request) -> Result<Arc<Mutex<P::Value>>, PersistentError> {
        req.extensions.get::<Write<P>>().cloned().ok_or(PersistentError::NotFound)
    }
}

impl<P: Key> BeforeMiddleware for Write<P> where P::Value: Send {
    fn before(&self, mut req: Request) -> (Request, HttpsResult<()>) {
        req.extensions.insert::<Write<P>>(self.data.clone());
        (req, Ok(()))
    }
}

impl<P: Key> AfterMiddleware for Write<P> where P::Value: Send {
    fn after(&self, req: Request, mut res: Response) -> (Request, Response, HttpsResult<()>) {
        res.extensions.insert::<Write<P>>(self.data.clone());
        (req, res, Ok(()))
    }
}

impl<P: Key> Write<P> where P::Value: Send {
    //构建用于预处理和后处理的多写少读共享状态
    pub fn both<T>(start: T) -> (Write<P>, Write<P>) where T: PersistentInto<Arc<Mutex<P::Value>>> {
        let x = Write { data: start.persistent_into() };
        (x.clone(), x)
    }

    //构建用于预处理或后处理的多写少读共享状态
    pub fn one<T>(start: T) -> Write<P> where T: PersistentInto<Arc<Mutex<P::Value>>> {
        Write { data: start.persistent_into() }
    }
}

/*
* 批分指定子片断的二进制数据
*/
pub fn split_bytes<'a>(bytes: &'a [u8], sub: &'a [u8]) -> Vec<&'a [u8]> {
    let mut pos = 0;
    let bytes_len = bytes.len();
    let sub_len = sub.len();
    let mut vec = Vec::new();
    if (bytes_len == 0) || (sub_len == 0) || (bytes_len < sub_len) {
        return vec;
    }

    let mut slice = bytes;
    loop {
        match find_bytes(slice, sub) {
            Some(0) => {
                //移除子片断，并继续查询
                slice = &slice[sub_len..];
            },
            Some(top) => {
                //加入片断，并继续查询
                pos = top + sub_len;
                vec.push(&slice[0..top]);
                slice = &slice[pos..];
            },
            None => {
                //退出查询
                if pos < bytes_len {
                    //加入剩余片断
                    vec.push(&slice[0..slice.len()]);
                }
                break;
            }
        }
    }
    vec
}

/*
* 释放指定值
*/
pub fn free<T>(_: T) where T: 'static {}