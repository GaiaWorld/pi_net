use std::marker::PhantomData;
use std::io::{ErrorKind, Error};

use https::{Version, HeaderMap, header::HOST};
use httparse::{EMPTY_HEADER, Request};
use futures::future::{FutureExt, LocalBoxFuture};
use bytes::Buf;
use log::warn;

use tcp::{Socket, AsyncService, SocketStatus, SocketHandle, SocketEvent,
          utils::SocketContext};

use crate::{acceptor::{MAX_CONNECT_HTTP_HEADER_LIMIT, HttpAcceptor},
            connect::HttpConnect,
            virtual_host::VirtualHostPool,
            service::ServiceFactory,
            request::HttpRequest,
            packet::UpStreamHeader};

///
/// Http连接监听器
///
pub struct HttpListener<S: Socket, P: VirtualHostPool<S>> {
    acceptor:           HttpAcceptor<S>,    //连接接受器
    hosts:              P,                  //虚拟主机池
    keep_alive:         usize,              //Http保持连接时长
}

impl<S: Socket, P: VirtualHostPool<S>> AsyncService<S> for HttpListener<S, P> {
    fn handle_connected(&self,
                        handle: SocketHandle<S>,
                        status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        //处理Http连接
        let future = async move {
            if let SocketStatus::Connected(Err(e)) = status {
                //Tcp连接失败
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Http server connect failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    e))));
                return;
            }
        };
        future.boxed_local()
    }

    fn handle_readed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        //处理Http后续请求
        if let SocketStatus::Readed(Err(e)) = status {
            //Tcp读数据失败
            return async move {
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    e))));
            }.boxed_local();
        }

        if unsafe { (&*handle.get_context().get()).is_empty() } {
            //当前是连接的首个Http请求
            let acceptor = self.acceptor.clone();
            let factory = self.hosts.clone();
            let keep_alive = self.keep_alive;

            async move {
                HttpAcceptor::<S>::accept(handle,
                                          acceptor,
                                          factory,
                                          keep_alive).await;
            }.boxed_local()
        } else {
            //Http连接已建立
            async move {
                //获取Http请求绑定的Http连接
                let mut context;
                if let Some(cx) = unsafe { (&*handle.get_context().get()).get::<HttpConnect<S, <<P as VirtualHostPool<S>>::Host as ServiceFactory<S>>::Service>>() } {
                    //需要将handle中获取的上下文句柄移动到外部，避免if let语句导致handle引用不会即时释放，从而导致在在后续使用handle的代码中出现编译时错误
                    context = cx;
                } else {
                    //请求没有连接上下文，则立即关闭当前Tcp连接
                    handle.close(Err(Error::new(ErrorKind::ConnectionRefused,
                                                format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid http connect context",
                                                        handle.get_token(),
                                                        handle.get_remote(),
                                                        handle.get_local()))));
                    return;
                }

                //解析上行请求
                if let Some(connect) = context.as_mut() {
                    let mut http_request_result = None;
                    let mut buf: &[u8] = &[]; //初始化本地缓冲区
                    let mut last_bin_len = 0; //初始化本地缓冲区上次长度
                    let mut parse_count = 0; //初始化分析次数
                    loop {
                        parse_count += 1; //更新分析次数
                        if parse_count > 16 {
                            //过多的分析次数，则立即返回错误原因
                            handle.close(Err(Error::new(ErrorKind::Other,
                                                        format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, buf_len: {:?}, buf: {:?}, reason: out of parse",
                                                                handle.get_token(),
                                                                handle.get_remote(),
                                                                handle.get_local(),
                                                                buf.len(),
                                                                buf))));
                            return;
                        }

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
                                                        format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid read buffer",
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
                                //解析成功
                                if let Some(value) = headers.get(HOST) {
                                    if let Ok(host_name) = value.to_str() {
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
                                                    http_request_result = Some(request);
                                                    break;
                                                } else {
                                                    //请求的Url无效，则立即关闭当前Tcp连接
                                                    handle.close(Err(Error::new(ErrorKind::ConnectionRefused,
                                                                                format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, url: {:?}, reason: invalid url",
                                                                                        handle.get_token(),
                                                                                        handle.get_remote(),
                                                                                        handle.get_local(),
                                                                                        url))));
                                                    return;
                                                }
                                            }
                                        }
                                    } else {
                                        //请求的主机头无效，则立即关闭当前连接
                                        handle.close(Err(Error::new(ErrorKind::Other,
                                                                    format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid host header",
                                                                            handle.get_token(),
                                                                            handle.get_remote(),
                                                                            handle.get_local()))));
                                        return;
                                    }
                                } else {
                                    //请求没有主机头，则立即关闭当前连接
                                    handle.close(Err(Error::new(ErrorKind::Other,
                                                                format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, reason: host header not exist",
                                                                        handle.get_token(),
                                                                        handle.get_remote(),
                                                                        handle.get_local()))));
                                    return;
                                }
                            },
                        }
                    }

                    if let Some(request) = http_request_result {
                        //运行Http服务
                        connect.run_service(request).await;
                    }
                } else {
                    //请求没有绑定Http连接，则立即关闭当前Tcp连接
                    handle.close(Err(Error::new(ErrorKind::ConnectionRefused,
                                                format!("Http server read failed, token: {:?}, remote: {:?}, local: {:?}, reason: invalid http connect",
                                                        handle.get_token(),
                                                        handle.get_remote(),
                                                        handle.get_local()))));
                }
            }.boxed_local()
        }
    }

    fn handle_writed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let keep_alive = self.keep_alive;
        let future = async move {
            if let SocketStatus::Writed(Err(e)) = status {
                //Tcp写数据失败，则立即关闭当前Http连接
                handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Http server write failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    e))));
                return;
            }

            //更新连接超时时长
            let mut event = SocketEvent::empty();
            event.set::<usize>(keep_alive);
            handle.set_timeout(keep_alive, event);
        };
        future.boxed_local()
    }

    fn handle_closed(&self,
                     handle: SocketHandle<S>,
                     status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let future = async move {
            if let SocketStatus::Closed(result) = status {
                if let Err(e) = result {
                    if e.kind() != ErrorKind::UnexpectedEof {
                        //Http连接非正常关闭
                        warn!("Http Connect Close by Error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                            handle.get_token(),
                            handle.get_remote(),
                            handle.get_local(),
                            e);
                    }
                }

                //连接已关闭，则立即释放Tcp连接的上下文
                if let Err(e) = unsafe { (&mut *handle.get_context().get()).remove::<HttpConnect<S, <<P as VirtualHostPool<S>>::Host as ServiceFactory<S>>::Service>>() } {
                    warn!("Free Context Failed by Http Connect Close, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                        handle.get_token(),
                        handle.get_remote(),
                        handle.get_local(),
                        e);
                }
            }
        };
        future.boxed_local()
    }

    fn handle_timeouted(&self,
                        handle: SocketHandle<S>,
                        status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let future = async move {
            if let SocketStatus::Timeout(event) = status {
                //Http连接超时，则立即关闭当前Http连接
                handle.close(Ok(()));
                warn!("Http Connect Timeout, token: {:?}, remote: {:?}, local: {:?}, keep_alive: {:?}",
                    handle.get_token(),
                    handle.get_remote(),
                    handle.get_local(),
                    event.get::<usize>());
            }
        };
        future.boxed_local()
    }
}

impl<S: Socket, P: VirtualHostPool<S>> HttpListener<S, P> {
    /// 构建指定连接服务工厂的Http连接监听器
    pub fn with_factory(hosts: P,
                        keep_alive: usize) -> Self {
        HttpListener {
            acceptor: HttpAcceptor::default(),
            hosts,
            keep_alive,
        }
    }
}

///
/// Http连接监听器工厂
///
pub struct HttpListenerFactory<S: Socket, P: VirtualHostPool<S>> {
    hosts:      P,                      //虚拟主机池
    keep_alive: usize,                  //Http保持连接时长
    marker:     PhantomData<S>,
}

impl<S: Socket, P: VirtualHostPool<S, >> HttpListenerFactory<S, P> {
    /// 构建指定虚拟主机池和Http保持连接时长的Http连接监听器工厂
    pub fn with_hosts(hosts: P,
                      keep_alive: usize) -> Self {
        HttpListenerFactory {
            hosts,
            keep_alive,
            marker: PhantomData,
        }
    }

    /// 构建Http连接监听器服务
    pub fn new_service(&self) -> Box<dyn AsyncService<S>> {
        Box::new(HttpListener::with_factory(self.hosts.clone(), self.keep_alive))
    }
}