use std::sync::Arc;
use std::str::FromStr;
use std::net::SocketAddr;
use std::string::ToString;
use std::fmt::{self, Debug};

use http;
use http::method::Method;
use http::version::Version as HttpVersion;

use hyper::rt::Stream;

use url::{self, Host};

use typemap::{Key, TypeMap};

use headers::{self, HeaderMap};

use plugin::Extensible;

use worker::task::TaskType;
use atom::Atom;

use https::Protocol;

use modifier::Set;
use {Plugin, Uri, Body, HttpRequest, HttpError};

/*
* http处理器请求对象使用的url
*/
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct Url {
    generic_url: url::Url,
}

impl ToString for Url {
    fn to_string(&self) -> String {
        self.generic_url.to_string()
    }
}

impl Into<url::Url> for Url {
    fn into(self) -> url::Url {
        self.generic_url
    }
}

impl AsRef<url::Url> for Url {
    fn as_ref(&self) -> &url::Url {
        &self.generic_url
    }
}

impl AsMut<url::Url> for Url {
    fn as_mut(&mut self) -> &mut url::Url {
        &mut self.generic_url
    }
}

impl FromStr for Url {
    type Err = String;
    #[inline]
    fn from_str(input: &str) -> Result<Url, Self::Err> {
        Url::parse(input)
    }
}

impl Url {
    //从字符串创建url
    pub fn parse(input: &str) -> Result<Url, String> {
        match url::Url::parse(input) {
            Ok(raw_url) => Url::from_generic_url(raw_url),
            Err(e) => Err(format!("{}", e)),
        }
    }

    //从原生url创建url
    pub fn from_generic_url(raw_url: url::Url) -> Result<Url, String> {
        if raw_url.cannot_be_a_base() {
            Err(format!("Not a special scheme: `{}`", raw_url.scheme()))
        } else if raw_url.port_or_known_default().is_none() {
            Err(format!("Invalid special scheme: `{}`", raw_url.scheme()))
        } else {
            Ok(Url {
                generic_url: raw_url,
            })
        }
    }

    //将url转换为原生url
    #[deprecated(
        since = "0.4.1",
        note = "use `into` from the `Into` trait instead"
    )]
    pub fn into_generic_url(self) -> url::Url {
        self.generic_url
    }

    //获取低层协议
    pub fn scheme(&self) -> &str {
        self.generic_url.scheme()
    }

    //获取主机或域
    pub fn host(&self) -> Host<&str> {
        self.generic_url.host().unwrap()
    }

    //获取连接端口
    pub fn port(&self) -> u16 {
        self.generic_url.port_or_known_default().unwrap()
    }

    //获取路径
    pub fn path(&self) -> Vec<&str> {
        self.generic_url.path_segments().unwrap().collect()
    }

    //获取路径描述
    pub fn path_str(&self) -> &str {
        self.generic_url.path()
    }

    //获取用户名
    pub fn username(&self) -> Option<&str> {
        // Map empty usernames to None.
        match self.generic_url.username() {
            "" => None,
            username => Some(username),
        }
    }

    //获取密码
    pub fn password(&self) -> Option<&str> {
        // Map empty passwords to None.
        match self.generic_url.password() {
            None => None,
            Some(x) if x.is_empty() => None,
            Some(password) => Some(password),
        }
    }

    //获取查询字符串，也就是?前的字符串
    pub fn query(&self) -> Option<&str> {
        self.generic_url.query()
    }

    //获取参数列表，也就是?后的字符串
    pub fn fragment(&self) -> Option<&str> {
        self.generic_url.fragment()
    }

    //获取url字符串
    pub fn format(&self) -> String {
        format!("{:?}", self.generic_url)
    }
}

/*
* http请求body的key
*/
struct RequestBodyKey;

impl Key for RequestBodyKey {
    type Value = Vec<u8>;
}

/*
* http处理器的请求对象
*/
pub struct Request {
    pub url: Url,
    pub local_addr: Option<SocketAddr>,
    pub headers: HeaderMap,
    pub query: String,
    pub body: Option<Body>,
    pub method: Method,
    pub extensions: TypeMap,
    pub version: HttpVersion,
    pub executor: fn(TaskType, usize, Option<isize>, Box<FnOnce(Option<isize>)>, Atom) -> Option<isize>,
    pub uid: usize,
}

impl Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(writeln!(f, "Request {{"));
        try!(writeln!(f, "    url: {:?}", self.url));
        try!(writeln!(f, "    method: {:?}", self.method));
        try!(writeln!(f, "    local_addr: {:?}", self.local_addr));
        try!(write!(f, "}}"));
        Ok(())
    }
}

//实现插件接口，如parser中对Request的使用
impl Plugin for Request {}
//实现修改器接口
impl Set for Request {}

//实现插件扩展接口，如parser中对Request的使用
impl Extensible for Request {
    fn extensions(&self) -> &TypeMap {
        &self.extensions
    }

    fn extensions_mut(&mut self) -> &mut TypeMap {
        &mut self.extensions
    }
}

impl Request {
    //处理来自http的请求
    pub fn from_http(executor: fn(TaskType, usize, Option<isize>, Box<FnOnce(Option<isize>)>, Atom) -> Option<isize>,
                    req: HttpRequest<Body>, 
                    local_addr: Option<SocketAddr>, 
                    protocol: &Protocol, 
                    uid: usize) -> Result<Request, String> {
        //获取基础信息和请求体
        let (
            http::request::Parts {
                method,
                uri,
                version,
                headers,
                ..
            },
            body,
        ) = req.into_parts();
        //获取请求的查询
        let query = match uri.query() {
            None => "".to_string(),
            Some(q) => q.to_string(),
        };
        Ok(Request {
            url: get_url(&uri, &version, &headers, local_addr.clone(), protocol)?,
            local_addr: local_addr,
            headers: headers,
            query: query,
            body: Some(body),
            method: method,
            extensions: TypeMap::new(),
            version: version,
            executor: executor,
            uid: uid,
        })
    }

    //获取body内容
    pub fn get_body_contents(&mut self) -> Result<&Vec<u8>, HttpError> {
        if let Some(reader) = self.body.take() {
            let body = reader.wait().fold(Ok(Vec::new()), |r, input| {
                if let Ok(mut v) = r {
                    input.map(move |next_body_chunk| {
                        v.extend_from_slice(&next_body_chunk);
                        v
                    })
                } else {
                    r
                }
            });
            match body {
                Ok(body) => self.extensions.insert::<RequestBodyKey>(body),
                Err(e) => return Err(e),
            };
        }
        Ok(self.extensions.get::<RequestBodyKey>().unwrap())
    }
}

//根据hyper的对端的路径、主机、端口获得访问的url
fn get_url(uri: &Uri, version: &HttpVersion, headers: &HeaderMap, 
    local_addr: Option<SocketAddr>, protocol: &Protocol) -> Result<Url, String> {
        let path = uri.path(); //获取请求的path
        let mut socket_ip = String::new();
        //获取对端的主机和端口
        let (host, port) = if let Some(host) = uri.host() {
            (host, uri.port())
        } else if let Some(host) = headers.get(headers::HOST).and_then(|h| h.to_str().ok()) {
            let mut parts = host.split(':');
            let hostname = parts.next().unwrap();
            let port = parts.next().and_then(|p| p.parse::<u16>().ok());
            (hostname, port)
        } else if version < &HttpVersion::HTTP_11 {
            if let Some(local_addr) = local_addr {
                match local_addr {
                    SocketAddr::V4(addr4) => socket_ip.push_str(&format!("{}", addr4.ip())),
                    SocketAddr::V6(addr6) => socket_ip.push_str(&format!("[{}]", addr6.ip())),
                }
                (socket_ip.as_ref(), Some(local_addr.port()))
            } else {
                return Err("No fallback host specified".into());
            }
        } else {
            return Err("No host specified in request".into());
        };
        //获取请求的url
        let url_string = if let Some(port) = port {
            format!("{}://{}:{}{}", protocol.name().to_string(), host, port, path)
        } else {
            format!("{}://{}{}", protocol.name().to_string(), host, path)
        };
        match Url::parse(&url_string) {
            Ok(url) => Ok(url),
            Err(e) => return Err(format!("Couldn't parse requested URL: {}", e)),
        }
}