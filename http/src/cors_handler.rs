use std::sync::Arc;
use std::str::FromStr;
use std::time::{Duration, SystemTime};
use std::io::{Error, Result, ErrorKind};

use url::{Host, Origin};
use https::{Uri, StatusCode, Method,
            header::{HeaderName, HeaderValue,
                     HOST,
                     ORIGIN,
                     ALLOW,
                     CONTENT_LENGTH,
                     ACCESS_CONTROL_REQUEST_METHOD,
                     ACCESS_CONTROL_REQUEST_HEADERS,
                     ACCESS_CONTROL_ALLOW_ORIGIN,
                     ACCESS_CONTROL_ALLOW_METHODS,
                     ACCESS_CONTROL_ALLOW_HEADERS,
                     ACCESS_CONTROL_MAX_AGE}};
use futures::future::{FutureExt, BoxFuture};
use parking_lot::RwLock;
use log::error;

use pi_atom::Atom;
use pi_hash::XHashMap;
use tcp::Socket;

use crate::{gateway::GatewayContext,
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::HttpResponse,
            utils::{DEFAULT_HTTP_SCHEME, DEFAULT_HTTPS_SCHEME, DEFAULT_HTTP_PORT, DEFAULT_HTTPS_PORT, HttpRecvResult}};
use std::cmp::max;

///
/// 允许任意源的跨域访问的请求头的值
///
const ACCESS_CONTROL_ALLOW_ANY_ORIGIN_HEADER_VALUE: &str = "*";

///
/// Http的CORS请求处理器
///
pub struct CORSHandler {
    default_methods:            String,                                                                     //默认允许的Http请求Method
    allow_any_origin_max_age:   Option<usize>,                                                              //是否允许任意跨域访问
    allow_origins:              RwLock<XHashMap<Origin, (Vec<String>, Option<String>, Option<usize>)>>,     //允许的跨域请求源表
    allowed:                    RwLock<XHashMap<String, Option<(usize, SystemTime)>>>,                      //已允许的跨域请求
}

unsafe impl Send for CORSHandler {}
unsafe impl Sync for CORSHandler {}

impl<S: Socket> Middleware<S, GatewayContext> for CORSHandler {
    fn request<'a>(&'a self,
                   context: &'a mut GatewayContext,
                   req: HttpRequest<S>)
                   -> BoxFuture<'a, MiddlewareResult<S>> {
        let future = async move {
            if req.method() == &Method::OPTIONS {
                //处理Options方法的CORS请求
                let resp = HttpResponse::new(2);
                handle_options_request(self, req, resp)
            } else {
                //处理其它Method的简单CORS请求
                handle_simple_request(self, req)
            }
        };
        future.boxed()
    }

    fn response<'a>(&'a self,
                    context: &'a mut GatewayContext,
                    req: HttpRequest<S>,
                    resp: HttpResponse)
                    -> BoxFuture<'a, MiddlewareResult<S>> {
        let mut response = resp;
        let future = async move {
            if let Some(allow_origin) = req.headers().get(ORIGIN) {
                //当前Http请求需要返回CORS简单验证后的响应头
                if self.allow_any_origin_max_age.is_some() {
                    //允许任意源的跨域访问
                    response.header(ACCESS_CONTROL_ALLOW_ORIGIN.as_str(), ACCESS_CONTROL_ALLOW_ANY_ORIGIN_HEADER_VALUE);
                } else {
                    response.header(ACCESS_CONTROL_ALLOW_ORIGIN.as_str(), String::from_utf8_lossy(allow_origin.as_bytes()).as_ref());
                }
            }

            //继续响应处理
            MiddlewareResult::ContinueResponse((req, response))
        };
        future.boxed()
    }
}

impl CORSHandler {
    //构建Http的CORS请求处理器
    pub fn new(default_methods: String,
               allow_any_origin_max_age: Option<usize>) -> Self {
        CORSHandler {
            default_methods,
            allow_any_origin_max_age,
            allow_origins: RwLock::new(XHashMap::default()),
            allowed: RwLock::new(XHashMap::default()),
        }
    }

    //增加允许跨域访问的主机、方法、请求头和允许的有效时长，主机如果是ipv6，需要用[]，允许的有效时长单位为秒
    pub fn allow_origin(&self,
                        scheme: String,
                        host: String,
                        port: u16,
                        methods: &[String],
                        headers: &[String],
                        max_age: Option<usize>) -> Result<()> {
        match Host::parse(&host) {
            Err(e) => {
                Err(Error::new(ErrorKind::AddrNotAvailable,
                               format!("Add allow origin failed, host: {:?}, reason: {:?}",
                                       host,
                                       e)))
            },
            Ok(allow_host) => {
                let allow_origin = Origin::Tuple(scheme, allow_host, port);

                let mut allow_methods = Vec::with_capacity(methods.len());
                for method in methods {
                    match Method::from_str(method.as_str()) {
                        Err(e) => {
                            return Err(Error::new(ErrorKind::AddrNotAvailable,
                                                  format!("Add allow origin failed, method: {:?}, reason: {:?}",
                                                          method,
                                                          e)));
                        },
                        Ok(allow_method) => {
                            allow_methods.push(allow_method.to_string());
                        },
                    }
                }

                let mut allow_headers = Vec::with_capacity(headers.len());
                if headers.len() > 0 {
                    //允许指定的跨域请求头
                    for header in headers {
                        if header.trim().to_lowercase() == "" {
                            //如果允许的跨域请求头为空，则忽略
                            continue;
                        }

                        match HeaderName::from_str(header.as_str()) {
                            Err(e) => {
                                return Err(Error::new(ErrorKind::AddrNotAvailable,
                                                      format!("Add allow origin failed, header: {:?}, reason: {:?}",
                                                              header,
                                                              e)));
                            },
                            Ok(allow_header) => {
                                allow_headers.push(allow_header.to_string());
                            },
                        }
                    }
                }

                if allow_headers.len() == 0 {
                    self
                        .allow_origins
                        .write()
                        .insert(allow_origin,
                                (allow_methods, None, max_age));
                } else {
                    self
                        .allow_origins
                        .write()
                        .insert(allow_origin,
                                (allow_methods, Some(allow_headers.join(",")), max_age));
                }

                Ok(())
            },
        }
    }
}

// 获取请求源的源对象
fn get_origin(origin: &HeaderValue) -> Result<Origin> {
    match String::from_utf8_lossy(origin.as_bytes()).parse::<Uri>() {
        Err(e) => {
            //无效的请求源
            Err(Error::new(ErrorKind::AddrNotAvailable,
                           format!("Invalid CORS request origin, reason: {:?}",
                                   e)))
        },
        Ok(url) => {
            let mut scheme = DEFAULT_HTTP_SCHEME.to_string();
            let mut host = "";
            let mut port: u16 = DEFAULT_HTTP_PORT;

            if let Some(s) = url.scheme() {
                scheme = s.as_str().to_string();
                if let Some(h) = url.host() {
                    host = h;
                    if let Some(p) = url.port_u16() {
                        //客户端显示指定了端口
                        port = p;
                    } else {
                        //客户端没有显示指定端口
                        if s == DEFAULT_HTTPS_SCHEME {
                            //使用安全的http默认端口
                            port = DEFAULT_HTTPS_PORT;
                        }
                    }
                }
            }

            match Host::parse(host) {
                Err(e) => {
                    //无效的主机
                    Err(Error::new(ErrorKind::AddrNotAvailable,
                                   format!("Invalid CORS request host, reason: {:?}",
                                           e)))
                },
                Ok(h) => {
                    Ok(Origin::Tuple(scheme, h, port))
                },
            }
        },
    }
}

// 处理Options方法的CORS请求
fn handle_options_request<S: Socket>(handler: &CORSHandler,
                                     req: HttpRequest<S>,
                                     mut resp: HttpResponse) -> MiddlewareResult<S> {
    if let Some(origin) = req.headers().get(ORIGIN) {
        if let Some(methods) = req.headers().get(ACCESS_CONTROL_REQUEST_METHOD) {
            match get_origin(origin) {
                Err(e) => {
                    //无效的请求源
                    return MiddlewareResult::Throw(e);
                },
                Ok(key) => {
                    //解析请求的源成功
                    if let Some(max_age) = handler.allow_any_origin_max_age {
                        //允许任意源的跨域访问
                        resp
                            .header(ACCESS_CONTROL_ALLOW_ORIGIN.as_str(),
                                    ACCESS_CONTROL_ALLOW_ANY_ORIGIN_HEADER_VALUE);
                        resp
                            .header(ACCESS_CONTROL_ALLOW_METHODS.as_str(),
                                    handler.default_methods.as_str());
                        if let Some(value) = req
                            .headers()
                            .get(ACCESS_CONTROL_REQUEST_HEADERS) {
                            //当前CORS请求中有Access-Control-Request-Headers请求头，则设置对应的响应头
                            resp
                                .header(ACCESS_CONTROL_ALLOW_HEADERS.as_str(),
                                        String::from_utf8_lossy(value.as_bytes()).as_ref());
                        }
                        resp
                            .header(ACCESS_CONTROL_MAX_AGE.as_str(),
                                    max_age.to_string().as_str());
                    } else {
                        //允许指定源的跨域访问
                        let allow_origin = String::from_utf8_lossy(origin.as_bytes());
                        if let Some((allow_methods, allow_headers, allow_max_age)) = handler.allow_origins.read().get(&key) {
                            //验证服务器是否支持客户端的Method
                            for method in String::from_utf8_lossy(methods.as_bytes()).split(',').collect::<Vec<&str>>() {
                                if allow_methods.contains(&method.trim().to_string()) {
                                    //支持，则继续验证
                                    continue;
                                }

                                //不支持，则不允许当前源跨域访问，并设置响应体长度后立即返回
                                error!("Http handle CORS failed, token: {:?}, remote: {:?}, local: {:?}, host: {:?}, origin: {:?}, reason: not support methods",
                                    req.get_handle().get_token(),
                                    req.get_handle().get_remote(),
                                    req.get_handle().get_local(),
                                    req.headers().get(HOST),
                                    key);
                                resp.header(CONTENT_LENGTH.as_str(), "0");
                                return MiddlewareResult::Break(resp);
                            }

                            //允许客户端指定的源进行指定的跨域访问，则设置对应的响应头
                            resp
                                .header(ACCESS_CONTROL_ALLOW_ORIGIN.as_str(),
                                        allow_origin.as_ref());
                            resp
                                .header(ACCESS_CONTROL_ALLOW_METHODS.as_str(),
                                        &allow_methods.join(","));
                            if let Some(value) = req
                                .headers()
                                .get(ACCESS_CONTROL_REQUEST_HEADERS) {
                                //当前CORS请求中有Access-Control-Request-Headers请求头，则设置对应的响应头
                                if let Some(allow_headers) = allow_headers {
                                    //允许指定的跨域请求头
                                    resp
                                        .header(ACCESS_CONTROL_ALLOW_HEADERS.as_str(),
                                                allow_headers);
                                } else {
                                    //允许任意的跨域请求头
                                    resp
                                        .header(ACCESS_CONTROL_ALLOW_HEADERS.as_str(),
                                                String::from_utf8_lossy(value.as_bytes()).as_ref());
                                }
                            }
                            if let Some(max_age) = allow_max_age {
                                //设置了允许的有效时长，则设置对应的响应头，并将当前请求源设置到已允许跨域访问表中
                                handler
                                    .allowed
                                    .write()
                                    .insert(allow_origin.to_string(),
                                            Some((*max_age, SystemTime::now())));
                                resp
                                    .header(ACCESS_CONTROL_MAX_AGE.as_str(),
                                            max_age.to_string().as_str());
                            } else {
                                //未设置允许的有效时长，则不设置对应的响应头，并将当前请求源设置到已允许跨域访问表中
                                handler
                                    .allowed
                                    .write()
                                    .insert(allow_origin.to_string(),
                                            None);
                            }
                        }

                        //不允许客户端请求的源进行跨域访问，则不设置任何允许的跨域访问的请求头，客户端会自动判断为不允许当前源跨域访问
                        error!("Http handle CORS failed, token: {:?}, remote: {:?}, local: {:?}, host: {:?}, origin: {:?}, reason: not allow client cross domain",
                            req.get_handle().get_token(),
                            req.get_handle().get_remote(),
                            req.get_handle().get_local(),
                            req.headers().get(HOST),
                            key);
                    }

                    //完成CORS请求处理，则设置响应体长度，并立即返回CORS响应
                    resp.header(CONTENT_LENGTH.as_str(), "0");
                    return MiddlewareResult::Break(resp);
                },
            }
        }
    }

    //不是CORS的Options请求，则设置服务器默认支持的Method和响应体长度，并立即返回Options响应
    resp.header(ALLOW.as_str(), handler.default_methods.as_str());
    resp.header(CONTENT_LENGTH.as_str(), "0");
    MiddlewareResult::Break(resp)
}

// 处理简单CORS请求
fn handle_simple_request<S: Socket, >(handler: &CORSHandler,
                                      req: HttpRequest<S>) -> MiddlewareResult<S> {
    if let Some(value) = req.headers().get(ORIGIN) {
        if handler.allow_any_origin_max_age.is_some() {
            //允许任意源的跨域访问，则继续后继中间件的处理
            return MiddlewareResult::ContinueRequest(req);
        } else {
            //允许指定源的跨域访问
            let origin = String::from_utf8_lossy(value.as_bytes());
            let allowed = handler
                .allowed
                .read()
                .get(origin.as_ref())
                .cloned(); //防止出现读写锁重入，所以不允许在match中多次使用同一把锁
            match allowed {
                None => {
                    //当前已允许的源中，没有当前请求源，则判断是否允许当前请求源跨域访问
                    match get_origin(value) {
                        Err(e) => {
                            //无效的请求源
                            return MiddlewareResult::Throw(e);
                        },
                        Ok(key) => {
                            if let Some((_, _, _)) = handler.allow_origins.read().get(&key) {
                                //简单验证通过，则允许客户端指定的源进行指定的跨域访问，并将当前请求源设置到已允许跨域访问表中
                                handler.allowed.write().insert(origin.to_string(), None);
                            } else {
                                //简单验证失败，则不允许客户端指定的源进行指定的跨域访问，则立即返回响应
                                error!("Http handle CORS failed, token: {:?}, remote: {:?}, local: {:?}, host: {:?}, origin: {:?}, reason: simple check error",
                                    req.get_handle().get_token(),
                                    req.get_handle().get_remote(),
                                    req.get_handle().get_local(),
                                    req.headers().get(HOST),
                                    key);
                                let mut resp = HttpResponse::new(2);
                                resp.header(CONTENT_LENGTH.as_str(), "0");
                                return MiddlewareResult::Break(resp);
                            }
                        },
                    }
                },
                Some(Some((timeout, time))) => {
                    //当前已允许的源中，有当前请求源，有过期时间
                    match time.elapsed() {
                        Err(e) => {
                            //验证允许的过期时间错误，则立即抛出错误
                            return MiddlewareResult::Throw(Error::new(ErrorKind::Other,
                                                                      format!("Simple CORS request failed, reason: {:?}",
                                                                              e)));
                        },
                        Ok(elapsed) => {
                            if elapsed.as_secs() > timeout as u64 {
                                //已过期，则立即返回响应
                                error!("Http handle CORS failed, token: {:?}, remote: {:?}, local: {:?}, host: {:?}, origin: {:?}, reason: auth timeout",
                                    req.get_handle().get_token(),
                                    req.get_handle().get_remote(),
                                    req.get_handle().get_local(),
                                    req.headers().get(HOST),
                                    req.headers().get(ORIGIN));
                                let mut resp = HttpResponse::new(2);
                                resp.header(CONTENT_LENGTH.as_str(), "0");
                                return MiddlewareResult::Break(resp);
                            }
                        },
                    }
                },
                Some(None) => (), //当前已允许的源中，有当前请求源，没有过期时间，则允许指定源的跨域访问
            }
        }
    }

    //通过简单CORS验证，则继续后继请求处理
    MiddlewareResult::ContinueRequest(req)
}