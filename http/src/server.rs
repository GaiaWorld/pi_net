use std::mem;
use std::sync::Arc;
use std::cell::RefCell;
use std::net::Shutdown;
use std::str::from_utf8;
use std::marker::PhantomData;
use std::collections::HashMap;
use std::result::Result as GenResult;
use std::io::{ErrorKind, Result, Error};

use https::{Version, Response, HeaderMap, header::HOST};
use httparse::{EMPTY_HEADER, Request};
use futures::future::{FutureExt, BoxFuture};
use bytes::{Buf, BufMut, BytesMut};
use log::warn;

use tcp::{server::AsyncWaitsHandle,
          driver::{Socket, AsyncIOWait,
                   AsyncService, AsyncServiceFactory, SocketStatus,
                   SocketHandle, AsyncReadTask, AsyncWriteTask},
          util::{IoBytes, SocketContext}};

use crate::{acceptor::{MAX_CONNECT_HTTP_HEADER_LIMIT, HttpAcceptor},
            connect::HttpConnect,
            virtual_host::VirtualHostPool,
            service::ServiceFactory,
            request::HttpRequest,
            packet::UpStreamHeader};

/*
* Http连接监听器
*/
pub struct HttpListener<S: Socket, W: AsyncIOWait, P: VirtualHostPool<S, W>> {
    acceptor:   HttpAcceptor<S, W>, //连接接受器
    hosts:      P,                  //虚拟主机池
    keep_alive: usize,              //Http保持连接时长
}

impl<S: Socket, W: AsyncIOWait, P: VirtualHostPool<S, W>> AsyncService<S, W> for HttpListener<S, W, P> {
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn handle_connected(&self, handle: SocketHandle<S>, waits: W, status: SocketStatus) -> Self::Future {
        //处理Http连接，并处理首次请求
        let acceptor = self.acceptor.clone();
        let factory = self.hosts.clone();
        let keep_alive = self.keep_alive;

        let future = async move {
            if let SocketStatus::Connected(Err(e)) = status {
                //Tcp连接失败
                handle.close(Err(Error::new(ErrorKind::Other, format!("http server connect failed, reason: {:?}", e))));
                return;
            }

            HttpAcceptor::<S, W>::accept(handle, waits, acceptor, factory, keep_alive).await;
        };
        future.boxed()
    }

    fn handle_readed(&self, handle: SocketHandle<S>, waits: W, status: SocketStatus) -> Self::Future {
        //处理Http后续请求
        let future = async move {
            if let SocketStatus::Readed(Err(e)) = status {
                //Tcp读数据失败
                handle.close(Err(Error::new(ErrorKind::Other, format!("http server read failed, reason: {:?}", e))));
                return;
            }

            //获取Http请求绑定的Http连接
            let mut context;
            if let Some(cx) = handle.get_context().get::<HttpConnect<S, W, <<P as VirtualHostPool<S, W>>::Host as ServiceFactory<S, W>>::Service>>() {
                //需要将handle中获取的上下文句柄移动到外部，避免if let语句导致handle引用不会即时释放，从而导致在在后续使用handle的代码中出现编译时错误
                context = cx;
            } else {
                //请求没有连接上下文，则立即关闭当前Tcp连接
                handle.close(Err(Error::new(ErrorKind::ConnectionRefused, "http server read failed, reason: invalid http connect context")));
                return;
            }

            //解析上行请求
            if let Some(connect) = context.as_mut() {
                let mut http_request_result = None;
                let buf = Box::into_raw(Box::new(Vec::<u8>::new())) as usize;
                loop {
                    match AsyncReadTask::async_read(handle.clone(), waits.clone(), 0).await {
                        Err(e) => {
                            handle.close(Err(Error::new(ErrorKind::Other, format!("http server read header failed, reason: {:?}", e))));
                            return;
                        },
                        Ok(bin) => {
                            unsafe { (&mut *(buf as *mut Vec<u8>)).put(bin); }
                            let mut headers = HeaderMap::new();
                            let mut header = [EMPTY_HEADER; MAX_CONNECT_HTTP_HEADER_LIMIT];
                            let mut req = Request::new(&mut header);
                            if let Some(body_offset) = UpStreamHeader::read_header(handle.clone(),
                                                                                    waits.clone(),
                                                                                    unsafe { (&*(buf as *mut Vec<u8>)).as_slice() },
                                                                                    &mut req,
                                                                                    &mut headers) {
                                //解析成功
                                let buf = unsafe { *Box::from_raw(buf as *mut Vec<u8>) };
                                if let Some(value) = headers.get(HOST) {
                                    if let Ok(host_name) = value.to_str() {
                                        if let &Some(method) = &req.method {
                                            if let &Some(path) = &req.path {
                                                //构建本次Http连接请求
                                                let url = if handle.is_security() {
                                                    "https://".to_string() + host_name + path
                                                } else {
                                                    "https://".to_string() + host_name + path
                                                };

                                                if let Some(request) = HttpRequest::new(handle.clone(), waits.clone(), method, &url, Version::HTTP_11, headers, &buf[body_offset..]) {
                                                    http_request_result = Some(request);
                                                    break;
                                                } else {
                                                    //请求的Url无效，则立即关闭当前Tcp连接
                                                    handle.close(Err(Error::new(ErrorKind::ConnectionRefused, format!("http server read failed, url: {:?}, reason: invalid url", url))));
                                                    return;
                                                }
                                            }
                                        }
                                    } else {
                                        //请求的主机头无效，则立即关闭当前连接
                                        handle.close(Err(Error::new(ErrorKind::Other, "http server read failed, reason: invalid host header")));
                                        return;
                                    }
                                } else {
                                    //请求没有主机头，则立即关闭当前连接
                                    handle.close(Err(Error::new(ErrorKind::Other, "http server read failed, reason: host header not exist")));
                                    return;
                                }
                            }
                        }
                    }
                }

                if let Some(request) = http_request_result {
                    //运行Http服务
                    connect.run_service(request).await;
                }
            } else {
                //请求没有绑定Http连接，则立即关闭当前Tcp连接
                handle.close(Err(Error::new(ErrorKind::ConnectionRefused, "http server read failed, reason: invalid http connect")));
            }
        };
        future.boxed()
    }

    fn handle_writed(&self, handle: SocketHandle<S>, waits: W, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Writed(Err(e)) = status {
                //Tcp写数据失败，则立即关闭当前Http连接
                handle.close(Err(Error::new(ErrorKind::Other, format!("http server write failed, reason: {:?}", e))));
                return;
            }
        };
        future.boxed()
    }

    fn handle_closed(&self, handle: SocketHandle<S>, waits: W, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Closed(result) = status {
                if let Err(e) = result {
                    if e.kind() != ErrorKind::UnexpectedEof {
                        //Http连接非正常关闭
                        warn!("!!!> Http Connect Close by Error, local: {:?}, remote: {:?}, reason: {:?}", handle.get_local(), handle.get_remote(), e);
                    }
                }

                //连接已关闭，则立即释放Tcp连接的上下文
                if let Err(e) = handle.get_context_mut().remove::<HttpConnect<S, W, <<P as VirtualHostPool<S, W>>::Host as ServiceFactory<S, W>>::Service>>() {
                    warn!("!!!> Free Context Failed by Http Connect Close, uid: {:?}, local: {:?}, remote: {:?}, reason: {:?}", handle.get_uid(), handle.get_local(), handle.get_remote(), e);
                }
            }
        };
        future.boxed()
    }

    fn handle_timeouted(&self, handle: SocketHandle<S>, waits: W, status: SocketStatus) -> Self::Future {
        let future = async move {
            if let SocketStatus::Timeout(event) = status {
                //Http连接超时，则立即关闭当前Http连接
                handle.close(Ok(()));
                warn!("!!!> Http Connect Timeout, keep_alive: {:?}, local: {:?}, remote: {:?}", event.get::<usize>(), handle.get_local(), handle.get_remote());
            }
        };
        future.boxed()
    }
}

impl<S: Socket, W: AsyncIOWait, P: VirtualHostPool<S, W>> HttpListener<S, W, P> {
    //构建指定连接服务工厂的Http连接监听器
    pub fn with_factory(hosts: P, keep_alive: usize) -> Self {
        HttpListener {
            acceptor: HttpAcceptor::default(),
            hosts,
            keep_alive,
        }
    }
}

/*
* Http连接监听器工厂
*/
pub struct HttpListenerFactory<S: Socket, P: VirtualHostPool<S, AsyncWaitsHandle>> {
    hosts:      P,      //虚拟主机池
    keep_alive: usize,  //Http保持连接时长
    marker:     PhantomData<S>,
}

impl<S: Socket, P: VirtualHostPool<S, AsyncWaitsHandle>> AsyncServiceFactory for HttpListenerFactory<S, P> {
    type Connect = S;
    type Waits = AsyncWaitsHandle;
    type Out = ();
    type Future = BoxFuture<'static, Self::Out>;

    fn new_service(&self) -> Box<dyn AsyncService<Self::Connect, Self::Waits, Out = Self::Out, Future = Self::Future>> {
        Box::new(HttpListener::with_factory(self.hosts.clone(), self.keep_alive))
    }
}

impl<S: Socket, P: VirtualHostPool<S, AsyncWaitsHandle>> HttpListenerFactory<S, P> {
    //构建指定虚拟主机池和Http保持连接时长的Http连接监听器工厂
    pub fn with_hosts(hosts: P, keep_alive: usize) -> Self {
        HttpListenerFactory {
            hosts,
            keep_alive,
            marker: PhantomData,
        }
    }
}