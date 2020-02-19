use std::sync::Arc;
use std::marker::PhantomData;
use std::io::{Error, ErrorKind};

use httparse::{EMPTY_HEADER, Request};
use https::{Result as HttpResult,
            Response,
            Version,
            status::StatusCode,
            header::{HOST, CONTENT_LENGTH, HeaderMap}};

use tcp::driver::{Socket, AsyncIOWait, SocketHandle, AsyncReadTask, AsyncWriteTask};

use crate::{virtual_host::VirtualHostPool,
            service::{ServiceFactory, HttpService},
            request::HttpRequest,
            connect::HttpConnect,
            packet::UpStreamHeader};

/*
* Http连接时允许的最大Http头数量
*/
pub const MAX_CONNECT_HTTP_HEADER_LIMIT: usize = 32;

/*
* Http连接接受器
*/
pub struct HttpAcceptor<S: Socket, W: AsyncIOWait> {
    marker: PhantomData<(S, W)>,
}

unsafe impl<S: Socket, W: AsyncIOWait> Send for HttpAcceptor<S, W> {}
unsafe impl<S: Socket, W: AsyncIOWait> Sync for HttpAcceptor<S, W> {}

impl<S: Socket, W: AsyncIOWait> Clone for HttpAcceptor<S, W> {
    fn clone(&self) -> Self {
        HttpAcceptor {
            marker: PhantomData,
        }
    }
}

impl<S: Socket, W: AsyncIOWait> Default for HttpAcceptor<S, W> {
    //默认构建连接接受器
    fn default() -> Self {
        HttpAcceptor {
            marker: PhantomData,
        }
    }
}

/*
* Http连接接受器异步方法
*/
impl<S: Socket, W: AsyncIOWait> HttpAcceptor<S, W> {
    //异步接受连接请求
    pub async fn accept<P>(handle: SocketHandle<S>,
                           waits: W,
                           acceptor: HttpAcceptor<S, W>,
                           hosts: P,
                           keep_alive: usize)
        where P: VirtualHostPool<S, W> {
        let mut headers = [EMPTY_HEADER; MAX_CONNECT_HTTP_HEADER_LIMIT];
        let req = Request::new(&mut headers);
        let headers = HeaderMap::new();

        //解析上行请求
        if let Some((req, headers, buf, body_offset)) = UpStreamHeader::read_header(handle.clone(), waits.clone(), req, headers).await {
            //解析成功，则根据请求的主机，获取相应的服务
            if let Some(value) = headers.get(HOST) {
                if let Ok(host_name) = value.to_str() {
                    if let Some(host) = hosts.get(host_name) {
                        let mut connect = HttpConnect::new(handle.clone(), waits.clone(), host.new_service(), keep_alive);
                        if let &Some(method) = &req.method {
                            if let &Some(path) = &req.path {
                                //构建本次Http连接请求
                                let url = if handle.is_security() {
                                    "https://".to_string() + host_name + path
                                } else {
                                    "https://".to_string() + host_name + path
                                };

                                if let Some(request) = HttpRequest::new(handle.clone(), waits, method, &url, Version::HTTP_11, headers, &buf[body_offset..]) {
                                    connect.run_service(request).await; //运行Http服务
                                    handle.get_context_mut().set(connect); //绑定Tcp连接上下文
                                } else {
                                    //连接请求中的Url无效，则立即关闭当前连接
                                    handle.close(Err(Error::new(ErrorKind::Other, format!("http connect failed, url: {:?}, reason: invalid url", url))));
                                }
                            }
                        }
                    } else {
                        //连接请求中的主机不存在，则立即关闭当前连接
                        handle.close(Err(Error::new(ErrorKind::Other, format!("http connect failed, host: {:?}, reason: host not exist", host_name))));
                    }
                } else {
                    //连接请求的主机头无效，则立即关闭当前连接
                    handle.close(Err(Error::new(ErrorKind::Other, "http connect failed, reason: invalid host header")));
                }
            } else {
                //连接请求中没有主机头，则立即关闭当前连接
                handle.close(Err(Error::new(ErrorKind::Other, "http connect failed, reason: host header not exist")));
            }
        }
    }
}
