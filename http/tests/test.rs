extern crate route_recognizer;

use std::thread;
use std::pin::Pin;
use std::sync::Arc;
use std::cell::RefCell;
use std::time::Instant;
use std::time::Duration;
use std::future::Future;
use std::net::SocketAddr;
use std::marker::PhantomData;
use std::io::{Error, ErrorKind};
use std::error::Error as StdError;
use std::task::{Context, Poll, Waker};

use https::HeaderMap;
use regex::{RegexSetBuilder, RegexSet, RegexBuilder, Regex};
use route_recognizer::Router;
use futures::future::{FutureExt, BoxFuture};
use flate2::{Compression, FlushCompress, Compress, Status, write::GzEncoder};
use twoway::{find_bytes, rfind_bytes};
use parking_lot::RwLock;
use env_logger;

use r#async::rt::{multi_thread::{MultiTaskPool, MultiTaskRuntime}};
use hash::XHashMap;
use atom::Atom;
use gray::GrayVersion;
use handler::{Args, Handler, SGenType};
use tcp::driver::{Socket, SocketConfig, AsyncIOWait, AsyncServiceFactory};
use tcp::buffer_pool::WriteBufferPool;
use tcp::util::{SocketEvent, TlsConfig};
use tcp::server::{AsyncWaitsHandle, AsyncPortsFactory, SocketListener};
use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;

use http::{server::HttpListenerFactory,
           virtual_host::{VirtualHostTab, VirtualHost, VirtualHostPool},
           gateway::GatewayContext,
           route::HttpRoute,
           middleware::{MiddlewareResult, Middleware, MiddlewareChain},
           cors_handler::CORSHandler,
           default_parser::DefaultParser,
           multi_parts::MutilParts,
           range_load::RangeLoad,
           file_load::FileLoad,
           files_load::FilesLoad,
           batch_load::BatchLoad,
           upload::UploadFile,
           port::HttpPort,
           static_cache::StaticCache,
           request::HttpRequest,
           response::{ResponseHandler, HttpResponse},
           util::HttpRecvResult};

#[test]
fn test_regex() {
    //测试**
    if let Ok(set) = RegexSetBuilder::new(&vec![r"/?([\w \.-]/?)+"][..]).build() {
        let matches: Vec<_> = set.matches(r"/abc/abcA0_.1aB0/adfa_01a/adf-sasd/a.jpg/").into_iter().collect();
        println!("!!!!!!matches: {:?}", matches);
    }

    //测试*
    if let Ok(set) = RegexSetBuilder::new(&vec![r"([\w \.-])+"][..]).build() {
        let matches: Vec<_> = set.matches(r"abcA0_.1aB0").into_iter().collect();
        println!("!!!!!!matches: {:?}", matches);
    }

    //测试过滤
    if let Ok(regex) = RegexBuilder::new(r"^([^\*])+[\.\*]$").build() {
        let str = "/x/_y/1z/Hello0_Route-1. z...png .*";
        if regex.is_match(str) {
            println!("!!!!!!filter: {:?}", str.replace(".*", ""));
        }
    }

    //测试访问路径
    let mut vec0 = vec![
        r"^/$",
        r"^/x$",
        r"^/y$",
        r"^/x/y/z$",
        r"^/x/y/z/$",
        r"^/xasdfasdfasdf/yasdfasdfasdfas/zasdfasdfsadfsad/xasdfasdfasdfasdf/yasfdasdfasdfasdf/zasdfasdfasdf/xasdfasdfasdf/yasdfasdfasfd/zasdfasdfasdf$",
    ];
    let mut vec1 = vec![
        r"^/([\w \.-])+$",                                              //  "/*"
        r"^/([\w \.-])+/$",                                             //  "/*/"
        r"^/x/([\w \.-])+/([\w \.-])+$",                                //  "/x/*/*"
        r"^/x/([\w \.-])+/([\w \.-])+/$",                               //  "/x/*/*/"
        r"^/x/([\w \.-])+/([\w \.-])+/z$",                              //  "/x/*/*/z"
        r"^/x/([\w \.-])+/([\w \.-])+/([\w \.-])+z([\w \.-])+\.jpg$",   //  "/x/*/*/*z*.jpg"
    ];
    let mut vec2 = vec![
        r"^/x/([\w \.-])+//?([\w \.-]/?)+$",        // "/x/*/**"
        r"^/x//?([\w \.-]/?)+//?([\w \.-]/?)+$",    // "/x/**/**"
    ];

    let mut set0 = RegexSetBuilder::new(&vec0[..]).build().ok().unwrap();
    let mut set1 = RegexSetBuilder::new(&vec1[..]).build().ok().unwrap();
    let mut set2 = RegexSetBuilder::new(&vec2[..]).build().ok().unwrap();

    let start = Instant::now();
    let route = match_route("/", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/", route);
    let route = match_route("/x", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x", route);
    let route = match_route("/y", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/y", route);
    let route = match_route("/x/y/z", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/y/z", route);
    let route = match_route("/x/y/z/", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/y/z/", route);
    println!("!!!!!!time: {:?}\n", Instant::now() - start);

    let start = Instant::now();
    let route = match_route("/xasdfasdfasdf/yasdfasdfasdfas/zasdfasdfsadfsad/xasdfasdfasdfasdf/yasfdasdfasdfasdf/zasdfasdfasdf/xasdfasdfasdf/yasdfasdfasfd/zasdfasdfasdf", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/xasdfasdfasdf/yasdfasdfasdfas/zasdfasdfsadfsad/xasdfasdfasdfasdf/yasfdasdfasdfasdf/zasdfasdfasdf/xasdfasdfasdf/yasdfasdfasfd/zasdfasdfasdf", route);
    let route = match_route("/xasdfasdfasdf/yasdfasdfasdfas/zasdfasdfsadfsad/xasdfasdfasdfasdf/yasfdasdfasdfasdf/zasdfasdfasdf/xasdfasdfasdf/yasdfasdfasfd/zasdfasdfasdf", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/xasdfasdfasdf/yasdfasdfasdfas/zasdfasdfsadfsad/xasdfasdfasdfasdf/yasfdasdfasdfasdf/zasdfasdfasdf/xasdfasdfasdf/yasdfasdfasfd/zasdfasdfasdf", route);
    let route = match_route("/xasdfasdfasdf/yasdfasdfasdfas/zasdfasdfsadfsad/xasdfasdfasdfasdf/yasfdasdfasdfasdf/zasdfasdfasdf/xasdfasdfasdf/yasdfasdfasfd/zasdfasdfasdf", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/xasdfasdfasdf/yasdfasdfasdfas/zasdfasdfsadfsad/xasdfasdfasdfasdf/yasfdasdfasdfasdf/zasdfasdfasdf/xasdfasdfasdf/yasdfasdfasfd/zasdfasdfasdf", route);
    println!("!!!!!!time: {:?}\n", Instant::now() - start);

    let start = Instant::now();
    let route = match_route("/xyz", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/xyz", route);
    let route = match_route("/xyz/", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/xyz/", route);
    let route = match_route("/x/xyz/z", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/xyz/z", route);
    let route = match_route("/x/xyz/z/", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/xyz/z/", route);
    let route = match_route("/x/xyz/zyx/z", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/xyz/zyx/z", route);
    let route = match_route("/x/_y/1z/Hello0_Route-1. z...png .jpg", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/_y/1z/Hello0_Route-1. z...png .jpg", route);
    let route = match_route("/x/y/z/xyz", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/y/z/xyz", route);
    let route = match_route("/x/y/z/xyz/", &vec0, &vec1, &vec2, &mut set0, &mut set1, &mut set2);
    println!("!!!!!!path: {:?}, route: {:?}", "/x/y/z/xyz/", route);
    println!("!!!!!!time: {:?}\n", Instant::now() - start);
}

//分三个优先级进行匹配，同优先级根据后进先匹配的原则进行匹配
fn match_route<'a>(path: &str, vec0: &'a Vec<&str>, vec1: &'a Vec<&str>, vec2: &'a Vec<&str>, set0: &mut RegexSet, set1: &mut RegexSet, set2: &mut RegexSet) -> Option<&'a str> {
    let matches: Vec<usize> = set0.matches(path).into_iter().collect();
    let len = matches.len();
    if len > 0 {
        return Some(vec0[matches[len - 1]]);
    }

    let matches: Vec<usize> = set1.matches(path).into_iter().collect();
    let len = matches.len();
    if len > 0 {
        return Some(vec1[matches[len - 1]]);
    }

    let matches: Vec<usize> = set2.matches(path).into_iter().collect();
    let len = matches.len();
    if len > 0 {
        return Some(vec2[matches[len - 1]]);
    }

    None
}

#[test]
fn test_router() {
    //匹配规则，基于不回溯的路径最大化匹配原则，以/分隔路径段
    let mut router = Router::new();
    router.add("/", "Hello Router /");
    router.add("/x", "Hello Router /x");
    router.add("/y", "Hello Router /y"); //同段路径中如果有多个相同的确定路由，则不覆盖; 且形如/xxx的路由只可以匹配本级路径，无法匹配上级和下级路径，例如:可以匹配/path，但无法匹配/或/path/
    router.add("/:", "Hello Router /:"); //同段路径中如果有确定路由和通配符路由，则匹配更具体的确定路由
    router.add("/:x", "Hello Router /:x");
    router.add("/:x/", "Hello Router /:x/"); //同段路径中如果有多个相同的通配符路由，则覆盖; 且形如/xxx/的路由可以同时匹配本级和下级路径，无法匹配上级路径，例如:可以匹配/path或/path/，但无法匹配/
    let dst = router.recognize("/").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/", dst.handler, dst.params);
    let dst = router.recognize("/x").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/x", dst.handler, dst.params);
    let dst = router.recognize("/y").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/y", dst.handler, dst.params);
    let dst = router.recognize("/xyz").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/xyz", dst.handler, dst.params);
    let dst = router.recognize("/xyz/").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/xyz/", dst.handler, dst.params);
    let dst = router.recognize("/;user=test001;passwd=111111").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/;user=test001;passwd=111111", dst.handler, dst.params);
    let dst = router.recognize("/xyz;user=test001;passwd=111111").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/xyz;user=test001;passwd=111111", dst.handler, dst.params);
    let dst = router.recognize("/x;user=test001;passwd=111111").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/x;user=test001;passwd=111111", dst.handler, dst.params);

    router.add("/x/y/z", "Hello Router /x/y/z");
    let dst = router.recognize("/x/y/z").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/x/y/z", dst.handler, dst.params);

    router.add("/x/:y/:z/", "Hello Router /x/:y/:z/");
    let dst = router.recognize("/x/y/z/").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/x/y/z/", dst.handler, dst.params);

    router.add("/x/:y/:z/:", "Hello Router /x/:y/:z/:");
    let dst = router.recognize("/x/y/z/;user=test001;passwd=111111").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/x/y/z/;user=test001;passwd=111111", dst.handler, dst.params);

    router.add("/x/:y/:z/hello", "Hello Router /x/:y/:z/hello"); //通配符路径与确定路径配合使用
    let dst = router.recognize("/x/y/z/hello;user=test001;passwd=111111").ok().unwrap();
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/x/y/z/hello;user=test001;passwd=111111", dst.handler, dst.params);

    router.add("/x/:y/*", "Hello Router /x/:y/*");
    router.add("/x/*z/*", "Hello Router /x/*y/*"); //同段路径中有:通配符和*通配符，则:通配符更具体
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/y/z", router.recognize("/x/y/z").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/y/z/", router.recognize("/x/y/z/").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/y/z/hello", router.recognize("/x/y/z/").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/hello/z", router.recognize("/x/hello/z").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/hello/z/", router.recognize("/x/hello/z/").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/hello/z/hello", router.recognize("/x/hello/z/hello").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/hello/x/y/z", router.recognize("/x/hello/x/y/z").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/hello/x/y/z/", router.recognize("/x/hello/x/y/z/").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}", "/x/hello/x/y/z/hello", router.recognize("/x/hello/x/y/z/hello").ok().unwrap().handler);
    println!("!!!!!!path: {:?}, dst: {:?}, params: {:?}", "/x/hello/x/y/z/hello/", router.recognize("/x/hello/x/y/z/hello/").ok().unwrap().handler, router.recognize("/x/hello/x/y/z/hello/").ok().unwrap().params);
}

#[test]
fn test_compress() {
    let mut encode = Compress::new(Compression::fast(), true);
    let input = "HelloHelloHelloHelloHelloHelloHelloHelloHello".as_bytes();
    let mut output = Vec::with_capacity(10);
    unsafe {
        output.set_len(10);
    }

    loop {
        if let Ok(status) = encode.compress(input, output.as_mut_slice(), FlushCompress::Finish) {
            match status {
                Status::BufError => {
                    println!("!!!!!!buf error");
                    break;
                },
                Status::Ok => {
                    println!("!!!!!!buf full, len: {:?}", encode.total_out());
                    output.resize(100, 0);
                    continue;
                },
                Status::StreamEnd => {
                    println!("!!!!!!compress finish, len: {:?}, output: {:?}", encode.total_out(), output);
                    break;
                },
            }
        }
    }
}

#[test]
fn test_find_bytes() {
    let data = "------WebKitFormBoundaryda3kjf6KAEbPATkF\r\nContent-Disposition: form-data; name=\"method\"\r\n\r\n\r\n------WebKitFormBoundaryda3kjf6KAEbPATkF\r\nContent-Disposition: form-data; name=\"file_name\"\r\n\r\n\r\n------WebKitFormBoundaryda3kjf6KAEbPATkF\r\nContent-Disposition: form-data; name=\"content\"; filename=\"README.md\"\r\nContent-Type: application/octet-stream\r\n\r\n# pi_pt\n\npi_serv.exe -r ../dst -l ../dst/pi_pt ../dst/pi_pi\n\npi_serv收到参数， 读../dst中的.depend, 创建依赖表。\n根据-l 中的路径， 找到项目所需js路径， 编译js代码， 存储在mgr中\n\n按 .c .a .b .e .i 顺序，合并一个js，开一个虚拟机执行， 执行完毕后虚拟机销毁\n\n.c为配置文件\n.a .b .e 读取server.cfg中的配置， 决定是否启动对应的服务模块和如何启动服务模块\n\n配置入口是多个路径。\n原生代码\n\n定义一个server.cfg，它描述了服务模块的配置结构\n然后init.ts里面就会有\n\n\n\n\npi_serv.exe -start cc.init.js  \n\npi_serv.exe ../dst/pi_pt ../dst/pi_pi\npi_serv收到参数， 找到项目下所有js， 编译js代码， 存储在mgr中\npi_serv收到参数， 找到项目下所有*.init.js， 按名字(如果相同，按目录名称)依次合并。然后执行。\n\n\n\r\n------WebKitFormBoundaryda3kjf6KAEbPATkF--\r\n".as_bytes();
    let boundary = b"------WebKitFormBoundaryda3kjf6KAEbPATkF\r\n";
    let boundary_end_str = (String::from_utf8_lossy(b"------WebKitFormBoundaryda3kjf6KAEbPATkF") + "--\r\n");
    let boundary_end = boundary_end_str.as_bytes();
    let boundary_len = boundary.len();

    let mut bin = &data[..];
    loop {
        match find_bytes(bin, boundary) {
            Some(index) => {
                println!("!!!!!!index: {:?}", index);
                let part = String::from_utf8_lossy(&bin[0..index]);
                let vec: Vec<&str> = part.split("\r\n\r\n").collect();
                println!("!!!!!!part headers: {:?}, part body: {:?}", vec.get(0), vec.get(1));
                bin = &bin[(index + boundary_len)..];
            },
            _ => {
                match rfind_bytes(bin, boundary_end) {
                    Some(index) => {
                        println!("!!!!!!finish index: {:?}", index);
                        let part = String::from_utf8_lossy(&bin[0..index]);
                        let vec: Vec<&str> = part.split("\r\n\r\n").collect();
                        println!("!!!!!!part headers: {:?}, part body: {:?}", vec.get(0), vec.get(1));
                        break;
                    },
                    _ => break,
                }
            },
        }
    }
}

struct TestMultiPartsHandler;

unsafe impl Send for TestMultiPartsHandler {}
unsafe impl Sync for TestMultiPartsHandler {}

impl<S: Socket, W: AsyncIOWait> Middleware<S, W, GatewayContext> for TestMultiPartsHandler {
    fn request<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>)
                   -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let future = async move {
            for (key, value) in context.as_parts() {
                match value {
                    SGenType::Str(val) => {
                        println!("!!!!!!key: {}, value: {:?}", key, val);
                    },
                    SGenType::Bin(val) => {
                        println!("!!!!!!key: {}, len: {:?}", key, val.len());
                    },
                    _ => (),
                }
            }

            MiddlewareResult::ContinueRequest(req)
        };
        future.boxed()
    }

    fn response<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>, resp: HttpResponse<S, W>)
                    -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let future = async move {
            MiddlewareResult::ContinueResponse((req, resp))
        };
        future.boxed()
    }
}

#[derive(Clone)]
struct WrapMsg(Arc<RefCell<XHashMap<String, SGenType>>>);

unsafe impl Send for WrapMsg {}
unsafe impl Sync for WrapMsg {}

struct TestHttpGatewayHandler;

unsafe impl Send for TestHttpGatewayHandler {}
unsafe impl Sync for TestHttpGatewayHandler {}

impl Handler for TestHttpGatewayHandler {
    type A = SocketAddr;
    type B = Arc<HeaderMap>;
    type C = Arc<RefCell<XHashMap<String, SGenType>>>;
    type D = ResponseHandler<TcpSocket>;
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    //处理方法
    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        if let Args::FourArgs(addr, headers, msg, handler) = args {
            handle(env, topic, addr, headers, msg, handler);
        }
    }
}

struct TestHttpsGatewayHandler;

unsafe impl Send for TestHttpsGatewayHandler {}
unsafe impl Sync for TestHttpsGatewayHandler {}

impl Handler for TestHttpsGatewayHandler {
    type A = SocketAddr;
    type B = Arc<HeaderMap>;
    type C = Arc<RefCell<XHashMap<String, SGenType>>>;
    type D = ResponseHandler<TlsSocket>;
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    //处理方法
    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> Self::HandleResult {
        if let Args::FourArgs(addr, headers, msg, handler) = args {
            handle(env, topic, addr, headers, msg, handler);
        }
    }
}

fn handle<S: Socket>(env: Arc<dyn GrayVersion>,
                     topic: Atom,
                     addr: SocketAddr,
                     headers: Arc<HeaderMap>,
                     msg: Arc<RefCell<XHashMap<String, SGenType>>>,
                     handler: ResponseHandler<S>) {
    let msg = WrapMsg(msg);
    let resp_handler = Arc::new(handler);

    thread::spawn(move || {
        println!("!!!!!!http gateway handle, topic: {:?}", topic);
        println!("!!!!!!http gateway handle, peer addr: {:?}", addr);
        println!("!!!!!!http gateway handle, headers: {:?}", headers);
        println!("!!!!!!http gateway handle, msg: {:?}", msg.0.borrow());

        //处理Http响应
        resp_handler.status(200);
        resp_handler.header("Port_Test", "true");
        if let Err(e) = resp_handler.write(Vec::from("Hello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello HttpHello Http\n".as_bytes())) {
            println!("!!!!!!write body failed, reason: {:?}", e);
            return;
        }

        resp_handler.finish();
        println!("!!!!!!http gateway handle ok");
    });
}

#[test]
fn test_http_hosts() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    //启动文件异步运行时
    let mut pool = MultiTaskPool::new("Http-Files-Runtime".to_string(), 8, 1 * 1024 * 1024, 10, None);
    let rt = pool.startup(false);

    //构建请求处理器
    let handler = Arc::new(TestHttpGatewayHandler);

    //构建全局静态资源缓存，并启动缓存的整理
    let cache = Arc::new(StaticCache::new(1024 * 1024 * 1024, 99999));
    StaticCache::run_collect(cache.clone(), "test http cache".to_string(), 10000);

    //构建中间件
    let cors_handler = CORSHandler::new("OPTIONS, GET, POST".to_string(), None);
    cors_handler.allow_origin("http".to_string(), "msg.highapp.com".to_string(), 80, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    cors_handler.allow_origin("http".to_string(), "127.0.0.1".to_string(), 80, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    let cors_handler = Arc::new(cors_handler);
    let parser = Arc::new(DefaultParser::with(128, None));
    let multi_parts = Arc::new(MutilParts::with(8 * 1024 * 1024));
    let range_load = Arc::new(RangeLoad::new());
    let file_load = Arc::new(FileLoad::new(rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let files_load = Arc::new(FilesLoad::new(rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let batch_load = Arc::new(BatchLoad::new(rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let upload = Arc::new(UploadFile::new(rt.clone(), "../upload"));
    let port = Arc::new(HttpPort::with_handler(None, handler));

    //构建处理CORS的Options方法的请求的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.finish();
    let cors_middleware = Arc::new(chain);

    //构建处理文件加载的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(range_load.clone());
    chain.push_back(file_load);
    chain.finish();
    let file_load_middleware = Arc::new(chain);

    //构建处理文件批量加载的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(range_load.clone());
    chain.push_back(files_load);
    chain.finish();
    let files_load_middleware = Arc::new(chain);

    //构建改进的处理文件批量加载的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(range_load);
    chain.push_back(batch_load);
    chain.finish();
    let batch_load_middleware = Arc::new(chain);

    //构建处理文件上传的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(multi_parts.clone());
    chain.push_back(upload);
    chain.finish();
    let upload_middleware = Arc::new(chain);

    //构建处理动态资源访问的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser);
    chain.push_back(multi_parts);
    chain.push_back(port);
    chain.finish();
    let port_middleware = Arc::new(chain);

    //构建路由
    let mut route = HttpRoute::new();
    route.at("/").options(cors_middleware.clone())
        .at("/**").options(cors_middleware)
        .at("/").head(file_load_middleware.clone())
        .at("/**").head(file_load_middleware.clone())
        .at("/").get(file_load_middleware.clone())
        .at("/**").get(file_load_middleware.clone())
        .at("/").post(file_load_middleware.clone())
        .at("/**").post(file_load_middleware)
        .at("/fs").get(files_load_middleware.clone())
        .at("/fs").post(files_load_middleware)
        .at("/batch").get(batch_load_middleware.clone())
        .at("/batch").post(batch_load_middleware)
        .at("/upload").post(upload_middleware.clone())
        .at("/login").get(port_middleware.clone())
        .at("/login").post(port_middleware.clone())
        .at("/port/**").get(port_middleware.clone())
        .at("/port/**").post(port_middleware);

    //构建虚拟主机
    let host = VirtualHost::with(route);

    //设置虚拟主机
    let mut hosts = VirtualHostTab::new();
    hosts.add("msg.highapp.com", host.clone());
    hosts.add("127.0.0.1", host);

    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(80,
                 Box::new(HttpListenerFactory::<TcpSocket, _>::with_hosts(hosts, 10000)));
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Http Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Http Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_https_hosts() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    //启动文件异步运行时
    let mut pool = MultiTaskPool::new("Http-Files-Runtime".to_string(), 8, 1 * 1024 * 1024, 10, None);
    let rt = pool.startup(false);

    //构建请求处理器
    let handler = Arc::new(TestHttpsGatewayHandler);

    //构建全局静态资源缓存，并启动缓存的整理
    let cache = Arc::new(StaticCache::new(1024 * 1024 * 1024, 99999));
    StaticCache::run_collect(cache.clone(), "test https cache".to_string(), 10000);

    //构建中间件
    let cors_handler = CORSHandler::new("OPTIONS, GET, POST".to_string(), None);
    cors_handler.allow_origin("https".to_string(), "msg.highapp.com".to_string(), 443, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    cors_handler.allow_origin("https".to_string(), "127.0.0.1".to_string(), 443, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    let cors_handler = Arc::new(cors_handler);
    let parser = Arc::new(DefaultParser::with(128, None));
    let multi_parts = Arc::new(MutilParts::with(8 * 1024 * 1024));
    let range_load = Arc::new(RangeLoad::new());
    let file_load = Arc::new(FileLoad::new(rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let files_load = Arc::new(FilesLoad::new(rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let batch_load = Arc::new(BatchLoad::new(rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let upload = Arc::new(UploadFile::new(rt.clone(), "../upload"));
    let port = Arc::new(HttpPort::with_handler(None, handler));

    //构建处理CORS的Options方法的请求的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.finish();
    let cors_middleware = Arc::new(chain);

    //构建处理文件加载的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(range_load.clone());
    chain.push_back(file_load);
    chain.finish();
    let file_load_middleware = Arc::new(chain);

    //构建处理文件批量加载的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(range_load.clone());
    chain.push_back(files_load);
    chain.finish();
    let files_load_middleware = Arc::new(chain);

    //构建改进的处理文件批量加载的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(range_load);
    chain.push_back(batch_load);
    chain.finish();
    let batch_load_middleware = Arc::new(chain);

    //构建处理文件上传的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser.clone());
    chain.push_back(multi_parts.clone());
    chain.push_back(upload);
    chain.finish();
    let upload_middleware = Arc::new(chain);

    //构建处理动态资源访问的中间件链
    let mut chain = MiddlewareChain::new();
    chain.push_back(cors_handler.clone());
    chain.push_back(parser);
    chain.push_back(multi_parts);
    chain.push_back(port);
    chain.finish();
    let port_middleware = Arc::new(chain);

    //构建路由
    let mut route = HttpRoute::new();
    route.at("/").options(cors_middleware.clone())
        .at("/**").options(cors_middleware)
        .at("/").head(file_load_middleware.clone())
        .at("/**").head(file_load_middleware.clone())
        .at("/").get(file_load_middleware.clone())
        .at("/**").get(file_load_middleware.clone())
        .at("/").post(file_load_middleware.clone())
        .at("/**").post(file_load_middleware)
        .at("/fs").get(files_load_middleware.clone())
        .at("/fs").post(files_load_middleware)
        .at("/batch").get(batch_load_middleware.clone())
        .at("/batch").post(batch_load_middleware)
        .at("/upload").post(upload_middleware.clone())
        .at("/login").get(port_middleware.clone())
        .at("/login").post(port_middleware.clone())
        .at("/port/**").get(port_middleware.clone())
        .at("/port/**").post(port_middleware);

    //构建虚拟主机
    let host = VirtualHost::with(route);

    //设置虚拟主机
    let mut hosts = VirtualHostTab::new();
    hosts.add("msg.highapp.com", host.clone());
    hosts.add("127.0.0.1", host);

    let mut factory = AsyncPortsFactory::<TlsSocket>::new();
    factory.bind(443,
                 Box::new(HttpListenerFactory::<TlsSocket, _>::with_hosts(hosts, 10000)));
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./3376363_msg.highapp.com.pem",
                                           "./3376363_msg.highapp.com.key",
                                           "",
                                           "",
                                           "",
                                           512,
                                           false,
                                           "").unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(443, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Https Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Https Listener Bind Ok");
        }
    }

    thread::sleep(Duration::from_millis(10000000));
}