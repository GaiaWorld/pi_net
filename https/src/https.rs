use std::sync::Arc;
use std::boxed::FnBox;
use std::time::Duration;
use std::sync::atomic::AtomicUsize;
use std::net::{SocketAddr, ToSocketAddrs};

use futures::*;
use npnc::bounded::spsc::{Producer, Consumer};

use hyper;
use hyper::Server;
use hyper::body::Body;
use hyper::Error;
use hyper::service::{NewService, Service};

use http::StatusCode;

use worker::task::TaskType;
use worker::impls::cast_net_task;
use future::future_pool::FutTaskPool;
use atom::Atom;

use request::Request;
use response::Response;
use handler::Handler;

use {HttpRequest, HttpResponse};

/*
* https异步任务优先级
*/
const HTTPS_ASYNC_TASK_PRIORITY: usize = 100;

/*
* http协议
*/
#[derive(Clone, Debug)]
pub enum Protocol {
    Http(u8),
    Https(u8),
}

impl Protocol {
    //获取协议名称
    pub fn name(&self) -> Atom {
        match self {
            Protocol::Http(_) => Atom::from("http"),
            Protocol::Https(_) => Atom::from("https"),
        }
    }
}

/*
* http服务器
*/
#[derive(Debug)]
pub struct Https<H> {
    protocol: Protocol,                 //http协议
    local_address: Option<SocketAddr>,  //本地地址
    keep_alive: Option<Duration>,       //是否允许长连接
    handler: Arc<H>,                    //处理器
    handle_timeout: u32,                //处理超时时长
    pool: FutTaskPool,                  //http任务池
}

impl<H: Handler> NewService for Https<H> {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = Error;
    type Service = HttpsHandler<H>;
    type InitError = Error;
    type Future = future::FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self) -> Self::Future {
        future::ok(HttpsHandler {
            protocol: self.protocol.clone(),
            addr: self.local_address,
            handler: self.handler.clone(),
            handle_timeout: self.handle_timeout,
            pool: self.pool.clone(),
        })
    }
}

impl<H: Handler> Https<H> {
    //构建一个http服务器
    pub fn new(handler: H, keep_alive_timeout: u64, handle_timeout: u32) -> Self {
        Https {
            protocol: Protocol::Http(11),
            local_address: None,
            keep_alive: Some(Duration::from_millis(keep_alive_timeout)),
            handler: Arc::new(handler),
            handle_timeout: handle_timeout,
            pool: FutTaskPool::new(cast_net_task),
        }
    }

    //配置http服务器
    pub fn http<A: ToSocketAddrs>(mut self, addr: A) {
        let addr: SocketAddr = addr.to_socket_addrs().unwrap().next().unwrap();
        self.local_address = Some(addr);
        let server = Server::bind(&addr)
                            .tcp_keepalive(self.keep_alive)
                            .serve(self)
                            .map_err(|e| eprintln!("https error: {}", e));
        hyper::rt::run(server);
    }
}

/*
* http服务器处理器
*/
pub struct HttpsHandler<H> {
    protocol: Protocol,
    addr: Option<SocketAddr>,
    handler: Arc<H>,
    handle_timeout: u32,
    pool: FutTaskPool,
}

impl<H: Handler> Service for HttpsHandler<H> {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = Error;
    type Future = Box<Future<Item = HttpResponse<Self::ResBody>, Error = Self::Error> + Send>;

    fn call(&mut self, request: HttpRequest<Self::ReqBody>) -> Self::Future {
        let addr = self.addr;
        let proto = self.protocol.clone();
        let handler = self.handler.clone();

        let callback = Box::new(move |executor: fn(TaskType, usize, Option<isize>, Box<FnBox(Option<isize>)>, Atom) -> Option<isize>,
                                                            sender: Arc<Producer<Result<HttpResponse<Self::ResBody>, Self::Error>>>,
                                                            receiver: Arc<Consumer<task::Task>>, uid: usize| {
            let func = Box::new(move |_lock| {
                match Request::from_http(executor, request, addr, &proto, uid) {
                    Err(e) => println!("Https Service Task Parse Request Failed, e: {}", e),
                    Ok(req) => {
                        {
                            //测试
                            // use headers;
                            // let mime_ = mime::TEXT_HTML;
                            // let vals: Vec<&str> = req.headers.get(headers::ACCEPT).unwrap().to_str().ok().unwrap().split(",").collect();
                            // let accept = vals[0].parse::<mime::Mime>();
                            // println!("!!!!!!headers: {:?}, {:?}, {:?}", mime_, accept, vals);
                            // println!("!!!!!!headers: {:?}", req.headers);
                            // println!("!!!!!!local: {:?}", req.local_addr);
                            // println!("!!!!!!method: {:?}", req.method);
                            // println!("!!!!!!path: {:?}", req.url.path());
                            // println!("!!!!!!query: {}", req.query);
                        }
                        let mut res = Response::new();
                        res.sender = Some(sender);
                        res.receiver = Some(receiver);
                        match handler.handle(req, res) {
                            None => (), //异步处理请求
                            Some((r, q, reply)) => {
                                //同步处理请求
                                match reply {
                                    Err(e) => {
                                        println!("!!!> Https Service Task Handle Failed, e: {:?}", e);
                                        match q.receiver.as_ref().unwrap().consume() {
                                            Err(_) => println!("!!!> Https Service Task Wakeup Failed, task id: {}", r.uid),
                                            Ok(waker) => {
                                                let sender_ = q.sender.as_ref().unwrap().clone();
                                                let mut http_res = HttpResponse::<Body>::new(Body::empty());
                                                *http_res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                                                q.write_back(&mut http_res);
                                                sender_.produce(Ok(http_res)).is_ok();
                                                waker.notify();
                                            }
                                        }
                                    },
                                    Ok(_) => {
                                        match q.receiver.as_ref().unwrap().consume() {
                                            Err(_) => println!("!!!> Https Service Task Wakeup Failed, task id: {}", r.uid),
                                            Ok(waker) => {
                                                let sender_ = q.sender.as_ref().unwrap().clone();
                                                let mut http_res = HttpResponse::<Body>::new(Body::empty());
                                                q.write_back(&mut http_res);
                                                sender_.produce(Ok(http_res)).is_ok();
                                                waker.notify();
                                            }
                                        }
                                    },
                                }
                            },
                        }
                    },
                }
            });
            executor(TaskType::Async(false), HTTPS_ASYNC_TASK_PRIORITY, None, func, Atom::from("https service before task"));
        });
        Box::new(self.pool.spawn(callback, self.handle_timeout))
    }
}