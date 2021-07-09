#[macro_use]
extern crate lazy_static;

use std::fs;
use std::sync::Arc;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use std::net::{IpAddr, SocketAddr};
use std::io::{Error, Result, ErrorKind};

use tokio;
use percent_encoding::{CONTROLS, percent_encode};
use reqwest::{ClientBuilder, Client, Proxy, Certificate, Identity, RequestBuilder, Request, Body, Response,
              header::{HeaderMap, HeaderName, HeaderValue},
              redirect::Policy,
              multipart::{Part, Form}};
use bytes::{Buf, BufMut, Bytes};

use hash::XHashMap;

use r#async::rt::{AsyncRuntime, AsyncValue};

/*
* 异步Http客户端运行时
*/
lazy_static! {
    static ref ASYNC_HTTPC_RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread().thread_name("ASYNC-HTTPC").worker_threads(2).max_blocking_threads(16).enable_all().build().unwrap();
}

/*
* 异步Http客户端构建器
*/
pub struct AsyncHttpcBuilder(HeaderMap, ClientBuilder);

impl AsyncHttpcBuilder {
    //构建异步Http客户端构建器，默认使用rustls
    pub fn new() -> Self {
        AsyncHttpcBuilder(HeaderMap::default(), ClientBuilder::use_native_tls(ClientBuilder::default()))
    }

    //使用rustls构建异步Http客户端构建器
    pub fn with_rustls() -> Self {
        AsyncHttpcBuilder(HeaderMap::default(), ClientBuilder::use_rustls_tls(ClientBuilder::default()))
    }

    //绑定客户端的地址
    pub fn bind_address<'a, V: Into<&'a str>>(self, addr: V) -> Self {
        if let Ok(addr) = IpAddr::from_str(addr.into()) {
            AsyncHttpcBuilder(self.0, self.1.local_address(Some(addr)))
        } else {
            panic!("Bind address failed, reason: invalid address");
        }
    }

    //设置Http客户端默认的User Agent头
    pub fn set_default_user_agent<'a, V: Into<&'a str>>(self, value: V) -> Self {
        AsyncHttpcBuilder(self.0, self.1.user_agent(value.into()))
    }

    //创建Http客户端默认头，同名头会被覆蓋
    pub fn new_default_header<'a, V: Into<&'a str>>(mut self, key: V, value: V) -> Self {
        self.0.insert(HeaderName::from_str(key.into()).unwrap(),
                      HeaderValue::from_str(value.into()).unwrap());
        AsyncHttpcBuilder(self.0, self.1)
    }

    //增加Http客户端默认头，同名头不会被覆蓋
    pub fn add_default_header<'a, V: Into<&'a str>>(mut self, key: V, value: V) -> Self {
        self.0.append(HeaderName::from_str(key.into()).unwrap(),
                      HeaderValue::from_str(value.into()).unwrap());
        AsyncHttpcBuilder(self.0, self.1)
    }

    //是否允许cookie，默认不允许
    pub fn enable_cookie(self, b: bool) -> Self {
        AsyncHttpcBuilder(self.0, self.1.cookie_store(b))
    }

    //是否允许根据响应头进行gzip解压缩，默认允许
    pub fn enable_gzip(self, b: bool) -> Self {
        AsyncHttpcBuilder(self.0, self.1.gzip(b))
    }

    //是否允许自动设置页面推荐，默认允许
    pub fn enable_auto_referer(self, b: bool) -> Self {
        AsyncHttpcBuilder(self.0, self.1.referer(b))
    }

    //是否允许严格模式，默认不允许
    pub fn enable_strict(self, b: bool) -> Self {
        AsyncHttpcBuilder(self.0,
                          ClientBuilder::danger_accept_invalid_certs(self.1, b)
                              .danger_accept_invalid_certs(b))
    }

    //设置允许的最大重定向次数，默认为0次
    pub fn set_redirect_limit(self, count: usize) -> Self {
        AsyncHttpcBuilder(self.0, self.1.redirect(Policy::limited(count)))
    }

    //设置指定的http代理
    pub fn set_http_proxy<'a, V: Into<&'a str>>(self, url: V, auth: Option<(V, V)>) -> Self {
        let url = url.into();
        let proxy = match Proxy::http(url) {
            Err(e) => {
                //创建代理失败，则立即抛出异常
                panic!("Set http proxy failed, url: {}, reason: {:?}", url, e);
            },
            Ok(proxy) => {
                //创建代理成功
                if let Some((user, pwd)) = auth {
                    //设置代理的账号和密码
                    proxy.basic_auth(user.into(), pwd.into())
                } else {
                    proxy
                }
            },
        };

        AsyncHttpcBuilder(self.0, self.1.proxy(proxy))
    }

    //设置连接过程的超时时长，超时后关退出连接过程，单位毫秒
    pub fn set_connect_timeout(self, timeout: u64) -> Self {
        AsyncHttpcBuilder(self.0, self.1.connect_timeout(Duration::from_millis(timeout)))
    }

    //设置请求过程的超时时长，超时后退出请求过程，单位毫秒
    pub fn set_request_timeout(self, timeout: u64) -> Self {
        AsyncHttpcBuilder(self.0, self.1.timeout(Duration::from_millis(timeout)))
    }

    //设置连接空闲的超时时长，超时后关闭连接，None表示永不超时，单位毫秒，默认90秒
    pub fn set_connection_timeout(self, timeout: Option<u64>) -> Self {
        if let Some(timeout) = timeout {
            AsyncHttpcBuilder(self.0, self.1.pool_idle_timeout(Some(Duration::from_millis(timeout))))
        } else {
            AsyncHttpcBuilder(self.0, self.1.pool_idle_timeout(None))
        }
    }

    //设置主机最大连接数
    pub fn set_host_connection_limit(self, count: usize) -> Self {
        AsyncHttpcBuilder(self.0, self.1.pool_max_idle_per_host(count))
    }

    //增加客户端根证书，用于客户端验证
    pub fn add_client_cert<P: AsRef<Path>>(self, path: P) -> Self {
        let cert = match fs::read(&path) {
            Err(e) => {
                //读取根证书文件失败，则立即抛出异常
                panic!("Set client cert failed, path: {:?}, reason: {:?}", path.as_ref(), e);
            },
            Ok(bin) => {
                match Certificate::from_pem(&bin[..]) {
                    Err(e) => {
                        //解析根证书文件失败，则立即抛出异常
                        panic!("Set client root cert failed, path: {:?}, reason: {:?}", path.as_ref(), e);
                    },
                    Ok(cert) => cert,
                }
            },
        };

        AsyncHttpcBuilder(self.0, self.1.add_root_certificate(cert))
    }

    //增加客户端身份，用于客户端验证
    //openssl pkcs12 -export -out identity.pfx -inkey key.pem -in cert.pem -certfile root.pem
    pub fn add_client_identity<P: AsRef<Path>>(self, path: P, pwd: &str) -> Self {
        let key = match fs::read(&path) {
            Err(e) => {
                //读取身份文件失败，则立即抛出异常
                panic!("Set client key failed, path: {:?}, reason: {:?}", path.as_ref(), e);
            },
            Ok(bin) => {
                match Identity::from_pkcs12_der(&bin[..], pwd) {
                    Err(e) => {
                        //解析身份文件失败，则立即抛出异常
                        panic!("Set client identity failed, path: {:?}, reason: {:?}", path.as_ref(), e);
                    },
                    Ok(key) => key,
                }
            },
        };

        AsyncHttpcBuilder(self.0, self.1.identity(key))
    }

    //构建异步http客户端
    pub fn build(self) -> Result<AsyncHttpc> {
        let headers = self.0;
        let mut builder = self.1;
        builder = builder.default_headers(headers);
        match builder.build() {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Build async http client failed, reason: {:?}", e)))
            },
            Ok(client) => {
                Ok(AsyncHttpc(Arc::new(client)))
            },
        }
    }
}

/*
* 异步Http客户端
*/
#[derive(Clone)]
pub struct AsyncHttpc(Arc<Client>);

impl AsyncHttpc {
    //构建Http的异步请求
    pub fn build_request(&self,
                         url: &str,
                         method: AsyncHttpRequestMethod) -> AsyncHttpRequest {
        let builder = match method {
            AsyncHttpRequestMethod::Get => {
                self.0.get(url)
            },
            AsyncHttpRequestMethod::Post => {
                self.0.post(url)
            },
        };

        AsyncHttpRequest {
            url: url.to_string(),
            method: method,
            builder,
        }
    }
}

/*
* 异步Http请求方法
*/
#[derive(Debug, Clone)]
pub enum AsyncHttpRequestMethod {
    Get,    //get请求
    Post,   //post请求
}

/*
* 异步Http请求的表单
*/
pub struct AsyncHttpForm(Form);

impl AsyncHttpForm {
    //获取当前表单的边界
    pub fn get_boundary(&self) -> &str {
        self.0.boundary()
    }

    //增加表单属性
    pub fn add_field(mut self, key: String, value: String) -> AsyncHttpForm {
        self.0 = self.0.text(key, value);
        self
    }

    //增加文件内容
    pub fn add_file(mut self,
                    key: String,
                    filename: String,
                    mime: String,
                    content: Vec<u8>) -> Result<AsyncHttpForm> {
        let len = content.len();
        match Part::bytes(content)
            .file_name(filename.clone())
            .mime_str(mime.as_str()) {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Add file to form failed, key: {}, file: {}, mime: {}, len: {}, reason: {}", key, filename, mime, len, e)))
            },
            Ok(part) => {
                self.0 = self.0.part(key, part);
                Ok(self)
            },
        }
    }

    //将表单转换为异步Http请求体
    pub fn into_body(self) -> AsyncHttpRequestBody {
        let form = self.0.percent_encode_attr_chars(); //对表单进行编码
        AsyncHttpRequestBody::Form(form)
    }
}

/*
* 异步Http请求体
*/
pub enum AsyncHttpRequestBody {
    Body(Body), //字符串或二进制
    Form(Form), //表单
}

impl AsyncHttpRequestBody {
    //构建一个字符串请求体
    pub fn with_string(body: String, is_encode: bool) -> Self {
        if is_encode {
            AsyncHttpRequestBody::Body(Body::from(percent_encode(body.into_bytes().as_slice(), CONTROLS).to_string()))
        } else {
            AsyncHttpRequestBody::Body(Body::from(body))
        }
    }

    //构建一个二进制请求体
    pub fn with_binary(body: Vec<u8>) -> Self {
        AsyncHttpRequestBody::Body(Body::from(body))
    }

    //构建一个表单
    pub fn form() -> AsyncHttpForm {
        AsyncHttpForm(Form::new())
    }
}

/*
* 异步Http请求
*/
pub struct AsyncHttpRequest {
    url:        String,                 //请求的url
    method:     AsyncHttpRequestMethod, //请求的方法
    builder:    RequestBuilder,         //请求构建器
}

/*
* 异步Http请求同步方法
*/
impl AsyncHttpRequest {
    //是否允许CORS，默认允许
    pub fn enable_cors(mut self, b: bool) -> AsyncHttpRequest {
        if !b {
            self.builder = self.builder.fetch_mode_no_cors();
        }

        self
    }

    //设置请求授权
    pub fn set_auth<'a, V: Into<&'a str>>(mut self, user: V, pwd: Option<V>) -> AsyncHttpRequest {
        if let Some(pwd) = pwd {
            self.builder = self.builder.basic_auth(user.into(), Some(pwd.into()));
        } else {
            self.builder = self.builder.basic_auth(user.into(), Some(""));
        }

        self
    }

    //设置请求参数对
    pub fn set_pairs(mut self, pairs: &[(&str, &str)]) -> AsyncHttpRequest {
        self.builder = self.builder.query(pairs);
        self
    }

    //增加请求头，同名头不会覆蓋
    pub fn add_header<'a, V: Into<&'a str>>(mut self, key: V, value: V) -> AsyncHttpRequest {
        self.builder = self.builder.header(key.into(), value.into());
        self
    }

    //设置请求体
    pub fn set_body(mut self, body: AsyncHttpRequestBody) -> AsyncHttpRequest {
        match body {
            AsyncHttpRequestBody::Body(body) => {
                self.builder = self.builder.body(body);
            },
            AsyncHttpRequestBody::Form(form) => {
                self.builder = self.builder.multipart(form);
            },
        }

        self
    }

    //设置请求的超时时长，覆蓋客户端的超时时长，单位毫秒
    pub fn set_timeout(mut self, timeout: u64) -> AsyncHttpRequest {
        self.builder = self.builder.timeout(Duration::from_millis(timeout));
        self
    }
}

/*
* 异步Http请求异步方法
*/
impl AsyncHttpRequest {
    //发送请求
    pub async fn send(self) -> Result<AsyncHttpResponse> {
        let url = self.url;
        let method = self.method;
        let request = self.builder;
        match ASYNC_HTTPC_RUNTIME.spawn(async move {
            request.send().await
        }).await {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Async http request failed, method: {:?}, url: {:?}, reason: {:?}", method, url, e)))
            },
            Ok(result) => {
                match result {
                    Err(e) => {
                        //发送请求失败
                        Err(Error::new(ErrorKind::Other, format!("Async http request failed, method: {:?}, url: {:?}, reason: {:?}", method, url, e)))
                    },
                    Ok(respone) => {
                        //发送请求成功
                        Ok(AsyncHttpResponse(respone))
                    },
                }
            }
        }
    }
}

/*
* 异步Http响应
*/
pub struct AsyncHttpResponse(Response);

/*
* 异步Http响应同步方法
*/
impl AsyncHttpResponse {
    //获取响应的对端地址
    pub fn get_peer_addr(&self) -> Option<SocketAddr> {
        self.0.remote_addr()
    }

    //获取响应的url
    pub fn get_url(&self) -> String {
        self.0.url().to_string()
    }

    //获取响应状态
    pub fn get_status(&self) -> u16 {
        self.0.status().as_u16()
    }

    //获取响应的http版本号
    pub fn get_version(&self) -> String {
        format!("{:?}", self.0.version())
    }

    //检查是否有指定关键字的响应头
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.headers().contains_key(key)
    }

    //获取指定关键字的响应头
    pub fn get_headers(&self, key: &str) -> Option<Vec<String>> {
        let r = self.0.headers().get_all(key).iter().map(|value| {
            if let Ok(val) = String::from_utf8(value.as_bytes().to_vec()) {
                val
            } else {
                String::new()
            }
        }).collect::<Vec<String>>();

        if r.is_empty() {
            None
        } else {
            Some(r)
        }
    }

    //获取所有的响应头
    pub fn to_headers(&self) -> XHashMap<String, String> {
        let mut headers = XHashMap::default();
        for (key, value) in self.0.headers() {
            if let Ok(value) = value.to_str() {
                headers.insert(key.to_string(), value.to_string());
            }
        }

        headers
    }

    //获取响应体的长度
    pub fn get_body_len(&self) -> Option<u64> {
        self.0.content_length()
    }
}

/*
* 异步Http响应异步方法
*/
impl AsyncHttpResponse {
    //获取响应体块
    pub async fn get_body(&mut self) -> Result<Option<Box<[u8]>>> {
        match self.0.chunk().await {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Get response body failed, reason: {:?}", e)))
            },
            Ok(Some(chunk)) => {
                Ok(Some(chunk.to_vec().into_boxed_slice()))
            },
            Ok(None) => Ok(None),
        }
    }

    //获取所有响应体块
    pub async fn body(&mut self) -> Result<Box<[u8]>> {
        let mut body: Vec<u8> = Vec::new();

        loop {
            match self.get_body().await {
                Err(e) => {
                    return Err(e);
                },
                Ok(Some(block)) => {
                    body.put_slice(&block[..]);
                },
                Ok(None) => break,
            }
        }

        Ok(body.into_boxed_slice())
    }
}