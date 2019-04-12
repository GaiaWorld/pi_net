#![feature(fnbox)]

extern crate reqwest;

#[macro_use]
extern crate lazy_static;

extern crate worker;
extern crate atom;
extern crate apm;

use std::fs::File;
use std::sync::Arc;
use std::path::Path;
use std::boxed::FnBox;
use std::path::PathBuf;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::time::{Instant, Duration};
use std::io::{Read, Error, ErrorKind, Result};

use reqwest::multipart::Form;
use reqwest::header::{Raw, Headers};
use reqwest::{ClientBuilder, Client, Certificate, Identity, Proxy, RedirectPolicy, Method, Body, RequestBuilder, Response};

use worker::task::TaskType;
use worker::impls::cast_net_task;
use apm::counter::{GLOBAL_PREF_COLLECT, PrefCounter, PrefTimer};
use atom::Atom;

/*
* http客户端异步任务优先级
*/
const HTTPC_ASYNC_TASK_PRIORITY: usize = 100;

lazy_static! {
    //http客户端创建数量
    static ref HTTPC_CREATE_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("httpc_create_count"), 0).unwrap();
    //http客户端移除数量
    static ref HTTPC_REMOVE_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("httpc_remove_count"), 0).unwrap();
    //http客户端get请求成功数量
    static ref HTTPC_GET_OK_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("httpc_get_ok_count"), 0).unwrap();
    //http客户端get请求失败数量
    static ref HTTPC_GET_ERROR_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("httpc_get_error_count"), 0).unwrap();
    //http客户端get请求成功总时长
    static ref HTTPC_GET_OK_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("httpc_get_ok_time"), 0).unwrap();
    //http客户端get请求失败总时长
    static ref HTTPC_GET_ERROR_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("httpc_get_error_time"), 0).unwrap();
    //http客户端post请求成功数量
    static ref HTTPC_POST_OK_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("httpc_post_ok_count"), 0).unwrap();
    //http客户端post请求失败数量
    static ref HTTPC_POST_ERROR_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("httpc_post_error_count"), 0).unwrap();
    //http客户端post请求成功总时长
    static ref HTTPC_POST_OK_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("httpc_post_ok_time"), 0).unwrap();
    //http客户端post请求失败总时长
    static ref HTTPC_POST_ERROR_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("httpc_post_error_time"), 0).unwrap();
}

/*
* http客户端选项
*/
pub enum HttpClientOptions {
    Default,                                                                  //默认选项
    Normal(bool, bool, bool, isize, u64),                                     //一般选项
    VaildHost(PathBuf, PathBuf, String, bool, bool, isize, u64),              //安全选项，所有https连接将双向验证主机证书
    Proxy(Atom, bool, bool, bool, isize, u64),                                //代理选项
    ValidHostProxy(PathBuf, PathBuf, String, Atom, bool, bool, isize, u64),   //安全代理选项，所有https连接将双向验证主机证书
}

/*
* 通用Method
*/
pub enum HttpMethod {
    Get,
    Post,
}

/*
* 通用Body
*/
pub trait GenHttpClientBody: Into<Body> + Send + Sync + 'static {}

impl GenHttpClientBody for &'static str {}
impl GenHttpClientBody for String {}
impl GenHttpClientBody for Vec<u8> {}
impl GenHttpClientBody for File {}

/*
* http的Body
*/
pub enum HttpClientBody<T: GenHttpClientBody> {
    Body(T),                        //块
    Json(HashMap<String, String>),  //json
    Form(Form),                     //表单
}

impl<T: GenHttpClientBody> HttpClientBody<T> {
    //创建body
    pub fn body(body: T) -> Self {
        HttpClientBody::Body(body)
    }

    //创建json
    pub fn json(key: Atom, value: T) -> Self where T: ToString {
        let mut map = HashMap::new();
        map.insert((*key).clone(), value.to_string());
        HttpClientBody::Json(map)
    }

    //创建表单
    pub fn form(key: Atom, value: T) -> Self where T: ToString {
        HttpClientBody::Form(Form::new().text((*key).clone(), value.to_string()))
    }

    //获取指定关键字的json值
    pub fn get_json_val(&self, key: Atom) -> Option<&String> {
        match self {
            HttpClientBody::Json(map) => {
                map.get(&*key)
            },
            _ => None,
        }
    }

    //增加json键值对，返回键值对数量
    pub fn add_json_kv(&mut self, key: Atom, value: String) -> usize {
        match self {
            HttpClientBody::Json(map) => {
                map.insert((*key).clone(), value);
                map.len()
            },
            _ => 0,
        }
    }

    //移除指定关键字的json键值对，返回被移除的值
    pub fn remove_json_kv(&mut self, key: Atom) -> Option<String> {
        match self {
            HttpClientBody::Json(map) => {
                map.remove(&*key)
            },
            _ => None,
        }
    }

    //清空所有json键值对
    pub fn clear_json_kvs(&mut self) {
        match self {
            HttpClientBody::Json(map) => {
                map.clear()
            },
            _ => (),
        }
    }

    //增加表单键值对
    pub fn add_form_kv(self, key: Atom, value: String) -> Self {
        match self {
            HttpClientBody::Form(form) => {
                HttpClientBody::Form(form.text((*key).clone(), value))
            },
            _ => self,
        }
    }

    //增加表单文件
    pub fn add_form_file<P: AsRef<Path>>(self, key: Atom, file: P) -> Result<Self> {
        match self {
            HttpClientBody::Form(form) => {
                form.file((*key).clone(), file).or_else(|e| {
                    Err(Error::new(ErrorKind::Other, e.description().to_string()))
                }).and_then(|f| {
                    Ok(HttpClientBody::Form(f))
                })
            },
            _ => Ok(self),
        }
    }
}

/*
* 共享http客户端
*/
pub trait SharedHttpc {
    //构建http客户端
    fn create(options: HttpClientOptions) -> Result<Arc<Self>>;
    //增加指定关键字的http头条目，返回头条目数量，一个关键字可以有多个条目
    fn add_header(client: &mut SharedHttpClient, key: Atom, value: Atom) -> usize;
    //移除指定关键字的http头条目，返回头条目数量
    fn remove_header(client: &mut SharedHttpClient, key: Atom) -> usize;
    //清空http头条目
    fn clear_headers(client: &mut SharedHttpClient);
    //异步发送get请求
    fn get<T: GenHttpClientBody>(client: &SharedHttpClient, url: Atom, body: HttpClientBody<T>, callback: Box<FnBox(Arc<Self>, Result<HttpClientResponse>)>);
    //异步发送post请求
    fn post<T: GenHttpClientBody>(client: &SharedHttpClient, url: Atom, body: HttpClientBody<T>, callback: Box<FnBox(Arc<Self>, Result<HttpClientResponse>)>);
    //获取当前http头条目数量
    fn headers_size(&self) -> usize;
    //获取所有http头条目关键字
    fn headers_keys(&self) -> Option<Vec<Atom>>;
    //获取指定关键字的http头条目，一个关键字可以有多个条目
    fn get_header(&self, key: Atom) -> Option<Vec<Atom>>;
}

/*
* 共享http客户端
*/
pub type SharedHttpClient = Arc<HttpClient>;

/*
* http客户端
*/
#[derive(Clone)]
pub struct HttpClient {
    inner: Client,      //内部客户端，因为Client依赖的mio有一个在windows下无法正常关闭socket的bug，至今未解决，所以尽量复用同一个Client，详见https://github.com/seanmonstar/reqwest/issues?utf8=%E2%9C%93&q=close 和 https://github.com/carllerche/mio/issues/776
    headers: Headers,   //请求头
}

impl Drop for HttpClient {
    fn drop(&mut self) {
        HTTPC_REMOVE_COUNT.sum(1);
    }
}

impl SharedHttpc for HttpClient {
    fn create(options: HttpClientOptions) -> Result<Arc<Self>> {
        match options {
            HttpClientOptions::Default => {
                ClientBuilder::new()
                            .danger_disable_hostname_verification()
                            .build()
            },
            HttpClientOptions::Normal(https, gzip, referer, count, timeout) => {
                let mut client = ClientBuilder::new();
                let client = if !https {
                    client.danger_disable_hostname_verification()
                } else {
                    client.enable_hostname_verification()
                };

                client
                    .gzip(gzip)
                    .referer(referer)
                    .redirect(if count < 0 {
                        RedirectPolicy::none()
                    } else {
                        RedirectPolicy::limited(count as usize)
                    })
                    .timeout(Duration::from_millis(timeout))
                    .build()
            },
            HttpClientOptions::VaildHost(cert_file, identity_file, pk, gzip, referer, count, timeout) => {
                let mut cert_buf = Vec::new();
                File::open(cert_file)?.read_to_end(&mut cert_buf)?;
                let cert = Certificate::from_der(&cert_buf).or_else(|e| {
                    Err(Error::new(ErrorKind::Other, e))
                })?;
                let mut identity_buf = Vec::new();
                File::open(identity_file)?.read_to_end(&mut identity_buf)?;
                let identity = Identity::from_pkcs12_der(&identity_buf, &pk).or_else(|e| {
                    Err(Error::new(ErrorKind::Other, e))
                })?;
                ClientBuilder::new()
                            .add_root_certificate(cert)
                            .identity(identity)
                            .gzip(gzip)
                            .referer(referer)
                            .redirect(if count < 0 {
                                RedirectPolicy::none()
                            } else {
                                RedirectPolicy::limited(count as usize)
                            })
                            .timeout(Duration::from_millis(timeout))
                            .build()
            },
            HttpClientOptions::Proxy(proxy_url, https, gzip, referer, count, timeout) => {
                let proxy = Proxy::http(&*proxy_url).or_else(|e| {
                    Err(Error::new(ErrorKind::Other, e))
                })?;

                let mut client = ClientBuilder::new();
                let client = if !https {
                    client.danger_disable_hostname_verification()
                } else {
                    client.enable_hostname_verification()
                };

                client
                    .proxy(proxy)
                    .gzip(gzip)
                    .referer(referer)
                    .redirect(if count < 0 {
                        RedirectPolicy::none()
                    } else {
                        RedirectPolicy::limited(count as usize)
                    })
                    .timeout(Duration::from_millis(timeout))
                    .build()
            },
            HttpClientOptions::ValidHostProxy(cert_file, identity_file, pk, proxy_url, gzip, referer, count, timeout) => {
                let mut cert_buf = Vec::new();
                File::open(cert_file)?.read_to_end(&mut cert_buf)?;
                let cert = Certificate::from_der(&cert_buf).or_else(|e| {
                    Err(Error::new(ErrorKind::Other, e))
                })?;
                let mut identity_buf = Vec::new();
                File::open(identity_file)?.read_to_end(&mut identity_buf)?;
                let identity = Identity::from_pkcs12_der(&identity_buf, &pk).or_else(|e| {
                    Err(Error::new(ErrorKind::Other, e))
                })?;
                let proxy = Proxy::http(&*proxy_url).or_else(|e| {
                    Err(Error::new(ErrorKind::Other, e))
                })?;
                ClientBuilder::new()
                            .add_root_certificate(cert)
                            .identity(identity)
                            .proxy(proxy)
                            .gzip(gzip)
                            .referer(referer)
                            .redirect(if count < 0 {
                                RedirectPolicy::none()
                            } else {
                                RedirectPolicy::limited(count as usize)
                            })
                            .timeout(Duration::from_millis(timeout))
                            .build()
            },
        }.or_else(|e| {
            Err(Error::new(ErrorKind::Other, e))
        }).and_then(|inner| {
            HTTPC_CREATE_COUNT.sum(1);

            Ok(Arc::new(HttpClient {
                inner: inner,
                headers: Headers::new(),
            }))
        })
    }

    fn add_header(client: &mut Arc<HttpClient>, key: Atom, value: Atom) -> usize {
        Arc::make_mut(client).headers.append_raw((*key).clone(), (*value).as_str());
        client.headers.len()
    }

    fn remove_header(client: &mut Arc<HttpClient>, key: Atom) -> usize {
        Arc::make_mut(client).headers.remove_raw((*key).as_str());
        client.headers.len()
    }

    fn clear_headers(client: &mut Arc<HttpClient>) {
        Arc::make_mut(client).headers.clear();
    }

    fn get<T: GenHttpClientBody>(client: &SharedHttpClient, url: Atom, body: HttpClientBody<T>, callback: Box<FnBox(Arc<Self>, Result<HttpClientResponse>)>) {
        let start = HTTPC_GET_OK_TIME.start();

        let copy = client.clone();
        let func = move |_lock| {
            let get = &mut copy.inner.get((*url).as_str());
            request(copy, get, HttpMethod::Get, body, callback, start);
        };
        cast_net_task(TaskType::Async(false), HTTPC_ASYNC_TASK_PRIORITY, None, Box::new(func), Atom::from("httpc normal get request task"));
    }

    fn post<T: GenHttpClientBody>(client: &SharedHttpClient, url: Atom, body: HttpClientBody<T>, callback: Box<FnBox(Arc<Self>, Result<HttpClientResponse>)>) {
        let start = HTTPC_POST_OK_TIME.start();

        let copy = client.clone();
        let func = move |_lock| {
            let post = &mut copy.inner.post((*url).as_str());
            request(copy, post, HttpMethod::Post, body, callback, start);
        };
        cast_net_task(TaskType::Async(false), HTTPC_ASYNC_TASK_PRIORITY, None, Box::new(func), Atom::from("httpc normal post request task"));
    }

    fn headers_size(&self) -> usize {
        self.headers.len()
    }

    fn headers_keys(&self) -> Option<Vec<Atom>> {
        let len = self.headers_size();
        if len == 0 {
            return None;
        }

        let mut vec = Vec::with_capacity(len);
        for header in self.headers.iter() {
            vec.push(Atom::from(header.name()))
        }
        Some(vec)
    }

    fn get_header(&self, key: Atom) -> Option<Vec<Atom>> {
        self.headers.get_raw(&*key).and_then(|val: &Raw| {
            let len = val.len();
            let mut vec = Vec::with_capacity(len);
            for index in 0..len {
                vec.push(Atom::from(&val[index]))
            }
            Some(vec)
        })
    }
}

/*
* http响应
*/
pub struct HttpClientResponse {
    inner: Response,
}

impl HttpClientResponse{
    //获取响应url
    pub fn url(&self) -> Atom {
        Atom::from(self.inner.url().as_str())
    }

    //判断是否是消息
    pub fn is_info(&self) -> bool {
        self.inner.status().is_informational()
    }

    //判断是否成功
    pub fn is_ok(&self) -> bool {
        self.inner.status().is_success()
    }

    //判断是否是重定向
    pub fn is_redirect(self) -> bool {
        self.inner.status().is_redirection()
    }

    //判断是否是客户端错误
    pub fn is_client_error(self) -> bool {
        self.inner.status().is_client_error()
    }

    //判断是否是服务器端错误
    pub fn is_server_error(self) -> bool {
        self.inner.status().is_server_error()
    }

    //判断是否是未知状态
    pub fn is_undefined(self) -> bool {
        self.inner.status().is_strange_status()
    }

    //获取响应状态
    pub fn status(&self) -> u16 {
        self.inner.status().as_u16()
    }

    //获取响应状态描述
    pub fn status_info(&self) -> Option<Atom> {
        self.inner.status().canonical_reason().and_then(|reason| {
            Some(Atom::from(reason))
        })
    }

    //获取响应头条目数量
    pub fn headers_size(&self) -> usize {
        self.inner.headers().len()
    }

    //获取响应头所有条目关键字
    pub fn headers_keys(&self) -> Option<Vec<Atom>> {
        let len = self.headers_size();
        if len == 0 {
            return None;
        }

        let mut vec = Vec::with_capacity(len);
        for header in self.inner.headers().iter() {
            vec.push(Atom::from(header.name()))
        }
        Some(vec)
    }

    //获取指定关键字的响应头条目，一个关键字可以有多个条目
    pub fn get_header(&self, key: Atom) -> Option<Vec<Atom>> {
        self.inner.headers().get_raw(&*key).and_then(|val: &Raw| {
            let len = val.len();
            let mut vec = Vec::with_capacity(len);
            for index in 0..len {
                vec.push(Atom::from(&val[index]))
            }
            Some(vec)
        })
    }

    //获取文本格式的响应体
    pub fn text(&mut self) -> Result<String> {
        self.inner.text().or_else(|e| {
            Err(Error::new(ErrorKind::Other, e))
        }).and_then(|text| {
            Ok(text)
        })
    }

    //获取二进制的响应体
    pub fn bin(&mut self) -> Result<Vec<u8>> {
        let mut vec = Vec::new();
        self.inner.copy_to(&mut vec).or_else(|e| {
            Err(Error::new(ErrorKind::Other, e))
        }).and(Ok(vec))
    }
}

//发送http请求
fn request<T: GenHttpClientBody>(client: SharedHttpClient,
                                 request: &mut RequestBuilder,
                                 method: HttpMethod,
                                 body: HttpClientBody<T>,
                                 callback: Box<FnBox(SharedHttpClient, Result<HttpClientResponse>)>,
                                 time: Instant) {
    match 
        match body {
            HttpClientBody::Body(body) => {
                //发送普通请求
                request.headers(client.headers.clone())
                    .body(body)
                    .send()
            },
            HttpClientBody::Json(json) => {
                //发送json请求
                request.headers(client.headers.clone())
                    .json(&json)
                    .send()
            },
            HttpClientBody::Form(form) => {
                //发送表单请求
                request.headers(client.headers.clone())
                    .multipart(form)
                    .send()
            },
        }
    {
        Err(e) => {
            match method {
                HttpMethod::Get => {
                    HTTPC_GET_ERROR_TIME.timing(time);
                    HTTPC_GET_ERROR_COUNT.sum(1);
                },
                HttpMethod::Post => {
                    HTTPC_POST_ERROR_TIME.timing(time);
                    HTTPC_POST_ERROR_COUNT.sum(1);
                },
                _ => (),
            }

            callback(client, Err(Error::new(ErrorKind::Other, e.description().to_string())))
        },
        Ok(inner) => {
            match method {
                HttpMethod::Get => {
                    HTTPC_GET_OK_TIME.timing(time);
                    HTTPC_GET_OK_COUNT.sum(1);
                },
                HttpMethod::Post => {
                    HTTPC_POST_OK_TIME.timing(time);
                    HTTPC_POST_OK_COUNT.sum(1);
                },
                _ => (),
            }

            callback(client, Ok(HttpClientResponse {
                inner: inner,
            }));
        },
    }
}