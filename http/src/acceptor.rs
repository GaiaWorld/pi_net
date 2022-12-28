use std::sync::Arc;
use std::marker::PhantomData;
use std::io::{Error, ErrorKind};

use httparse::{EMPTY_HEADER, Request};
use https::{Result as HttpResult,
            Response,
            Version,
            status::StatusCode,
            header::{HOST, CONTENT_LENGTH, HeaderMap}};
use bytes::{Buf, BufMut, BytesMut};

use tcp::{Socket, SocketHandle};

use crate::{virtual_host::VirtualHostPool,
            service::{ServiceFactory, HttpService},
            request::HttpRequest,
            connect::HttpConnect,
            packet::UpStreamHeader};

///
/// Http连接时允许的最大Http头数量
///
pub const MAX_CONNECT_HTTP_HEADER_LIMIT: usize = 32;

///
/// Http连接接受器
///
pub struct HttpAcceptor<S: Socket> {
    marker: PhantomData<(S)>,
}

unsafe impl<S: Socket> Send for HttpAcceptor<S> {}
unsafe impl<S: Socket> Sync for HttpAcceptor<S> {}

impl<S: Socket> Clone for HttpAcceptor<S> {
    fn clone(&self) -> Self {
        HttpAcceptor {
            marker: PhantomData,
        }
    }
}

impl<S: Socket> Default for HttpAcceptor<S> {
    /// 默认构建连接接受器
    fn default() -> Self {
        HttpAcceptor {
            marker: PhantomData,
        }
    }
}

///
/// Http连接接受器异步方法
///
impl<S: Socket> HttpAcceptor<S> {
    /// 异步接受连接请求
    pub async fn accept<P>(handle: SocketHandle<S>,
                           acceptor: HttpAcceptor<S>,
                           hosts: P,
                           keep_alive: usize)
        where P: VirtualHostPool<S> {
        //解析上行请求
        let mut http_request_result = None;
        let mut buf: &[u8] = &[]; //初始化本地缓冲区
        let mut last_bin_len = 0; //初始化本地缓冲区上次长度
        loop {
            if let Some(bin) = unsafe { (&mut *handle.get_read_buffer().get()) } {
                let remaining = bin.remaining();
                if remaining == 0 {
                    //当前缓冲区还没有请求的数据，则异步准备读取后，继续尝试接收请求数据
                    if let Ok(value) = handle.read_ready(0) {
                        if value.await == 0 {
                            //当前连接已关闭，则立即退出
                            return;
                        }
                    }

                    continue;
                } else if remaining == last_bin_len {
                    //当前缓冲区的数据还没有更新，则异步准备读取后，继续尝试接收请求数据
                    if let Ok(value) = handle.read_ready(remaining + 1) {
                        if value.await == 0 {
                            //当前连接已关闭，则立即退出
                            return;
                        }
                    }

                    continue;
                } else {
                    //当前缓冲区有请求的数据或当前缓冲区的数据已更新，则更新本地缓冲区上次长度
                    last_bin_len = remaining;
                }
            } else {
                //Tcp读缓冲区不存在
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Http connect failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid read buffer",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local()))));
                return;
            }

            let mut headers = HeaderMap::new();
            let mut header = [EMPTY_HEADER; MAX_CONNECT_HTTP_HEADER_LIMIT];
            let mut req = Request::new(&mut header);

            buf = unsafe { (&*handle.get_read_buffer().get()).as_ref().unwrap().as_ref() }; //填充本地缓冲区
            match UpStreamHeader::read_header(handle.clone(),
                                              buf,
                                              &mut req,
                                              &mut headers).await {
                Err(_) => {
                    //解决请求头失败，则立即退出本次请求
                    return;
                },
                Ok(None) => {
                    //解析请求头不完整，则读取后继续解析
                    continue;
                },
                Ok(Some(_body_offset)) => {
                    //解析成功，则根据请求的主机，获取相应的服务
                    if let Some(value) = headers.get(HOST) {
                        if let Ok(host_name) = value.to_str() {
                            if let Some(host) = hosts.get(host_name) {
                                let mut connect = HttpConnect::new(handle.clone(),
                                                                   host.new_service(),
                                                                   keep_alive);
                                if let &Some(method) = &req.method {
                                    if let &Some(path) = &req.path {
                                        //构建本次Http连接请求
                                        let url = if handle.is_security() {
                                            "https://".to_string() + host_name + path
                                        } else {
                                            "http://".to_string() + host_name + path
                                        };

                                        if let Some(request) = HttpRequest::new(handle.clone(),
                                                                                method,
                                                                                &url,
                                                                                Version::HTTP_11,
                                                                                headers,
                                                                                &[]) {
                                            http_request_result = Some((connect, request));
                                            break;
                                        } else {
                                            //连接请求中的Url无效，则立即关闭当前连接
                                            handle.close(Err(Error::new(ErrorKind::Other,
                                                                        format!("Http connect failed, token: {:?}, remote: {:?}, local: {:?}, url: {:?}, reason: invalid url",
                                                                                handle.get_token(),
                                                                                handle.get_remote(),
                                                                                handle.get_local(),
                                                                                url))));
                                            return;
                                        }
                                    }
                                }
                            } else {
                                //连接请求中的主机不存在，则立即关闭当前连接
                                handle.close(Err(Error::new(ErrorKind::Other,
                                                            format!("Http connect failed, token: {:?}, remote: {:?}, local: {:?}, host: {:?}, reason: host not exist",
                                                                    handle.get_token(),
                                                                    handle.get_remote(),
                                                                    handle.get_local(),
                                                                    host_name))));
                                return;
                            }
                        } else {
                            //连接请求的主机头无效，则立即关闭当前连接
                            handle.close(Err(Error::new(ErrorKind::Other,
                                                        format!("Http connect failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid host header",
                                                                handle.get_token(),
                                                                handle.get_remote(),
                                                                handle.get_local()))));
                            return;
                        }
                    } else {
                        //连接请求中没有主机头，则立即关闭当前连接
                        handle.close(Err(Error::new(ErrorKind::Other,
                                                    format!("Http connect failed, token: {:?}, remote: {:?}, local: {:?}, reason: host header not exist",
                                                            handle.get_token(),
                                                            handle.get_remote(),
                                                            handle.get_local()))));
                        return;
                    }
                },
            }
        }

        if let Some((mut connect, request)) = http_request_result {
            connect.run_service(request).await; //运行Http服务
            unsafe { (&mut *handle.get_context().get()).set(connect); } //绑定Tcp连接上下文
        }
    }
}
