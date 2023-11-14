extern crate new_tcp as tcp;

use std::fs;
use std::thread;
use std::sync::Arc;
use std::cell::RefCell;
use std::time::Duration;
use std::net::SocketAddr;

use env_logger;
use futures::future::{FutureExt, LocalBoxFuture};
use https::HeaderMap;
use tokio;
use bytes::BufMut;

use pi_async_rt::rt::{AsyncRuntime, AsyncRuntimeBuilder as RTAsyncRuntimeBuilder,
                      serial::AsyncRuntimeBuilder,
                      multi_thread::{MultiTaskRuntimeBuilder, MultiTaskRuntime}};
use tcp::{AsyncService, Socket, SocketHandle, SocketConfig, SocketStatus, SocketEvent,
          connect::TcpSocket,
          tls_connect::TlsSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::{TlsConfig, Ready}};
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
           utils::HttpRecvResult};
use pi_handler::{Args, Handler, SGenType};
use pi_hash::XHashMap;
use pi_gray::GrayVersion;
use pi_atom::Atom;
use tokio::time::Instant;

use pi_async_httpc::{AsyncHttpcBuilder, AsyncHttpc, AsyncHttpRequestMethod, AsyncHttpForm, AsyncHttpRequestBody};

#[derive(Clone)]
struct WrapMsg(Arc<RefCell<XHashMap<String, SGenType>>>);

unsafe impl Send for WrapMsg {}
unsafe impl Sync for WrapMsg {}

struct TestHttpGatewayHandler<R: AsyncRuntime>(R);

unsafe impl<R: AsyncRuntime> Send for TestHttpGatewayHandler<R> {}
unsafe impl<R: AsyncRuntime> Sync for TestHttpGatewayHandler<R> {}

impl<R: AsyncRuntime> Handler for TestHttpGatewayHandler<R> {
    type A = SocketAddr;
    type B = String;
    type C = Arc<HeaderMap>;
    type D = Arc<RefCell<XHashMap<String, SGenType>>>;
    type E = ResponseHandler;
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    //处理方法
    fn handle(&self, env: Arc<dyn GrayVersion>, topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        let rt = self.0.clone();
        async move {
            if let Args::FiveArgs(addr, method, headers, msg, handler) = args {
                handle(rt, env, topic, addr, method, headers, msg, handler);
            }
        }.boxed_local()
    }
}

fn handle<R: AsyncRuntime>(rt: R,
                           env: Arc<dyn GrayVersion>,
                           topic: Atom,
                           addr: SocketAddr,
                           method: String,
                           headers: Arc<HeaderMap>,
                           msg: Arc<RefCell<XHashMap<String, SGenType>>>,
                           handler: ResponseHandler) {
    let msg = WrapMsg(msg);
    let resp_handler = Arc::new(handler);

    rt.spawn(async move {
        // println!("!!!!!!http gateway handle, topic: {:?}", topic);
        // println!("!!!!!!http gateway handle, peer addr: {:?}", addr);
        // println!("!!!!!!http gateway handle, headers: {:?}", headers);
        // println!("!!!!!!http gateway handle, msg: {:?}", msg.0.borrow());

        //处理Http响应
        resp_handler.status(200);
        // resp_handler.header("Port_Test", "true");
        if let Err(e) = resp_handler.write(Vec::from("Hello Http".as_bytes())).await {
            println!("!!!!!!write body failed, reason: {:?}", e);
            return;
        }

        resp_handler.finish().await;
        // println!("!!!!!!http gateway handle ok");
    });
}

#[test]
fn test_http_request() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    //启动文件异步运行时
    let mut builder = MultiTaskRuntimeBuilder::default();
    let file_rt = builder.build();

    //启动网络异步运行时
    let rt0 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt1 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt2 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt3 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt4 = AsyncRuntimeBuilder::default_local_thread(None, None);
    let rt5 = AsyncRuntimeBuilder::default_local_thread(None, None);

    //构建全局静态资源缓存，并启动缓存的整理
    let cache = Arc::new(StaticCache::new(1024 * 1024 * 1024, 99999));
    StaticCache::run_collect(cache.clone(), "test http cache".to_string(), 10000);

    //构建请求处理器
    let handler
        = Arc::new(TestHttpGatewayHandler(file_rt.clone()));

    //构建全局静态资源缓存，并启动缓存的整理
    let cache = Arc::new(StaticCache::new(1024 * 1024 * 1024, 99999));
    StaticCache::run_collect(cache.clone(), "test http cache".to_string(), 10000);

    //构建中间件
    let cors_handler = CORSHandler::new("OPTIONS, GET, POST".to_string(), None);
    cors_handler.allow_origin("http".to_string(), "msg.highapp.com".to_string(), 80, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    cors_handler.allow_origin("http".to_string(), "127.0.0.1".to_string(), 80, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    let cors_handler = Arc::new(cors_handler);
    let parser = Arc::new(DefaultParser::with(128, None, None));
    let multi_parts = Arc::new(MutilParts::with(8 * 1024 * 1024));
    let range_load = Arc::new(RangeLoad::new());
    let mut file_load = FileLoad::new(file_rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10);
    file_load.set_min_block_size(Some(8 * 1024 * 1024));
    file_load.set_chunk_size(Some(512 * 1024));
    file_load.set_interval(Some(100));
    let file_load = Arc::new(file_load);
    let files_load = Arc::new(FilesLoad::new(file_rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let batch_load = Arc::new(BatchLoad::new(file_rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let upload = Arc::new(UploadFile::new(file_rt.clone(), "../upload"));
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
    hosts.add("test.17youx.cn", host.clone());
    hosts.add("127.0.0.1", host);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(80,
                 HttpListenerFactory::<TcpSocket, _>::with_hosts(hosts, 10000).new_service());
    let mut config = SocketConfig::new("0.0.0.0", factory.ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(vec![rt0, rt1, rt2, rt3, rt4, rt5],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               512 * 1024,
                               512 * 1024,
                               Some(10)) {
        Err(e) => {
            println!("!!!> Http Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Http Listener Bind Ok");
        }
    }

    //初始化异步Http客户端
    let httpc =
        AsyncHttpcBuilder::new()
            .bind_address("127.0.0.1")
            .set_default_user_agent("TestHttpClient")
            .new_default_header("XXX_XXX_XXX", "undefined")
            .add_default_header("XXX_XXX_XXX", "null")
            .enable_cookie(true)
            .enable_gzip(true)
            .enable_auto_referer(true)
            .enable_strict(false)
            .set_redirect_limit(10)
            .set_connect_timeout(5000)
            .set_request_timeout(10000)
            .set_connection_timeout(Some(60000))
            .set_host_connection_limit(10)
            .build().unwrap();

    //Optinos测试
    let httpc_copy = httpc.clone();
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("http://127.0.0.1/fs", AsyncHttpRequestMethod::Optinos)
            .set_pairs(&[("d", "d=:bi(:client((js()png()tpl()):app((js()png()tpl()):res((js()png()tpl()):images(js()png()tpl())))))"), ("f", ":pi(:widget(js(util:widget:painter:forelet:style:frame_mgr:event:virtual_node):)util(js(html:util:tpl:task_mgr:res_mgr:log:hash:math:match:task_pool:event):)gui_virtual(js(frame_mgr):)lang(js(type:mod:time):))")])
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, body: {:?}", body.as_ref());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //批量获取文件
    let httpc_copy = httpc.clone();
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("http://127.0.0.1/fs", AsyncHttpRequestMethod::Get)
            .set_pairs(&[("d", "d=:bi(:client((js()png()tpl()):app((js()png()tpl()):res((js()png()tpl()):images(js()png()tpl())))))"), ("f", ":pi(:widget(js(util:widget:painter:forelet:style:frame_mgr:event:virtual_node):)util(js(html:util:tpl:task_mgr:res_mgr:log:hash:math:match:task_pool:event):)gui_virtual(js(frame_mgr):)lang(js(type:mod:time):))")])
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //传递表单数据
    println!("");
    let httpc_copy = httpc.clone();
    let body = AsyncHttpRequestBody::with_string("user=test001&pwd=你好".to_string(), true);
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("http://127.0.0.1/", AsyncHttpRequestMethod::Post)
            .add_header("content-type", "application/x-www-form-urlencoded")
            .set_body(body)
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                let mut b = Vec::new();
                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                            b.put_slice(body.as_ref());
                        },
                        Ok(None) => break,
                    }
                }
                println!("!!!!!!body: {}", String::from_utf8(b).unwrap());
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //上传指定文件
    println!("");
    let httpc_copy = httpc.clone();
    let mut form = AsyncHttpRequestBody::form();
    let bin = fs::read(r#"E:\tmp\冷备份\windows\mdb_stat.exe"#).unwrap();
    form = form.add_field("method".to_string(), "".to_string());
    form = form.add_field("file_name".to_string(), "".to_string());
    form = form.add_file("content".to_string(),
                         "mdb_stat.exe".to_string(),
                         "application/octet-stream".to_string(),
                         bin).unwrap();
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("http://127.0.0.1/upload", AsyncHttpRequestMethod::Post)
            .set_body(form.into_body())
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //移除指定文件
    println!("");
    let httpc_copy = httpc.clone();
    let mut form = AsyncHttpRequestBody::form();
    form = form.add_field("method".to_string(), "_$remove".to_string());
    form = form.add_field("file_name".to_string(), "mdb_stat.exe".to_string());
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("http://127.0.0.1/upload", AsyncHttpRequestMethod::Post)
            .set_body(form.into_body())
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_https_request() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    //启动文件异步运行时
    let mut builder = MultiTaskRuntimeBuilder::default();
    let file_rt = builder.build();

    //启动网络异步运行时
    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);

    //构建请求处理器
    let handler
        = Arc::new(TestHttpGatewayHandler(file_rt.clone()));

    //构建全局静态资源缓存，并启动缓存的整理
    let cache = Arc::new(StaticCache::new(1024 * 1024 * 1024, 99999));
    StaticCache::run_collect(cache.clone(), "test https cache".to_string(), 10000);

    //构建中间件
    let cors_handler = CORSHandler::new("OPTIONS, GET, POST".to_string(), None);
    cors_handler.allow_origin("https".to_string(), "msg.highapp.com".to_string(), 443, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    cors_handler.allow_origin("https".to_string(), "127.0.0.1".to_string(), 443, &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()], &[], Some(10));
    let cors_handler = Arc::new(cors_handler);
    let parser = Arc::new(DefaultParser::with(128, None, None));
    let multi_parts = Arc::new(MutilParts::with(8 * 1024 * 1024));
    let range_load = Arc::new(RangeLoad::new());
    let file_load = Arc::new(FileLoad::new(file_rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let files_load = Arc::new(FilesLoad::new(file_rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let batch_load = Arc::new(BatchLoad::new(file_rt.clone(), "../htdocs", Some(cache.clone()), true, true, true, false, 10));
    let upload = Arc::new(UploadFile::new(file_rt.clone(), "../upload"));
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
    hosts.add("test.17youx.cn", host.clone());
    hosts.add("127.0.0.1", host);

    let mut factory = PortsAdapterFactory::<TlsSocket>::new();
    factory.bind(443,
                 HttpListenerFactory::<TlsSocket, _>::with_hosts(hosts, 10000).new_service());
    let tls_config = TlsConfig::new_server("",
                                           false,
                                           "./tests/7285407__17youx.cn.pem",
                                           "./tests/7285407__17youx.cn.key",
                                           "",
                                           "",
                                           "",
                                           512,
                                           false,
                                           "").unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(443, tls_config)]);

    match SocketListener::bind(vec![rt],
                               factory,
                               config,
                               1024,
                               1024 * 1024,
                               1024,
                               16,
                               16384,
                               16384,
                               Some(10)) {
        Err(e) => {
            println!("!!!> Https Listener Bind Error, reason: {:?}", e);
        },
        Ok(driver) => {
            println!("===> Https Listener Bind Ok");
        }
    }

    //初始化异步Http客户端
    let httpc =
        AsyncHttpcBuilder::new()
            .bind_address("127.0.0.1")
            .set_default_user_agent("TestHttpClient")
            .new_default_header("XXX_XXX_XXX", "undefined")
            .add_default_header("XXX_XXX_XXX", "null")
            .enable_cookie(true)
            .enable_gzip(true)
            .enable_auto_referer(true)
            .enable_strict(true)
            .set_redirect_limit(10)
            .set_connect_timeout(5000)
            .set_request_timeout(10000)
            .set_connection_timeout(Some(60000))
            .set_host_connection_limit(10)
            .build().unwrap();

    //Options测试
    let httpc_copy = httpc.clone();
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("https://msg.highapp.com/fs", AsyncHttpRequestMethod::Optinos)
            .set_pairs(&[("d", "d=:bi(:client((js()png()tpl()):app((js()png()tpl()):res((js()png()tpl()):images(js()png()tpl())))))"), ("f", ":pi(:widget(js(util:widget:painter:forelet:style:frame_mgr:event:virtual_node):)util(js(html:util:tpl:task_mgr:res_mgr:log:hash:math:match:task_pool:event):)gui_virtual(js(frame_mgr):)lang(js(type:mod:time):))")])
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_headers("content-length"));

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, body: {:?}", body.as_ref());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //批量获取文件
    let httpc_copy = httpc.clone();
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("https://msg.highapp.com/fs", AsyncHttpRequestMethod::Get)
            .set_pairs(&[("d", "d=:bi(:client((js()png()tpl()):app((js()png()tpl()):res((js()png()tpl()):images(js()png()tpl())))))"), ("f", ":pi(:widget(js(util:widget:painter:forelet:style:frame_mgr:event:virtual_node):)util(js(html:util:tpl:task_mgr:res_mgr:log:hash:math:match:task_pool:event):)gui_virtual(js(frame_mgr):)lang(js(type:mod:time):))")])
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_headers("content-length"));

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //传递表单数据
    println!("");
    let httpc_copy = httpc.clone();
    let body = AsyncHttpRequestBody::with_string("user=test001&pwd=你好".to_string(), true);
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("http://msg.highapp.com/", AsyncHttpRequestMethod::Post)
            .add_header("content-type", "application/x-www-form-urlencoded")
            .set_body(body)
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //上传指定文件
    println!("");
    let httpc_copy = httpc.clone();
    let mut form = AsyncHttpRequestBody::form();
    let bin = fs::read(r#"E:\tmp\冷备份\windows\mdb_stat.exe"#).unwrap();
    form = form.add_field("method".to_string(), "".to_string());
    form = form.add_field("file_name".to_string(), "".to_string());
    form = form.add_file("content".to_string(),
                         "mdb_stat.exe".to_string(),
                         "application/octet-stream".to_string(),
                         bin).unwrap();
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("https://msg.highapp.com/upload", AsyncHttpRequestMethod::Post)
            .set_body(form.into_body())
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //移除指定文件
    println!("");
    let httpc_copy = httpc.clone();
    let mut form = AsyncHttpRequestBody::form();
    form = form.add_field("method".to_string(), "_$remove".to_string());
    form = form.add_field("file_name".to_string(), "mdb_stat.exe".to_string());
    file_rt.spawn(async move {
        match httpc_copy
            .build_request("https://msg.highapp.com/upload", AsyncHttpRequestMethod::Post)
            .set_body(form.into_body())
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                println!("!!!!!!url: {}", resp.get_url());
                println!("!!!!!!status: {}", resp.get_status());
                println!("!!!!!!version: {}", resp.get_version());
                println!("!!!!!!headers: {:#?}", resp.to_headers());
                println!("!!!!!!body len: {:?}", resp.get_body_len());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        },
                        Ok(None) => break,
                    }
                }
            },
        }
    });
    thread::sleep(Duration::from_millis(5000));

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_access_delay() {
    let rt = RTAsyncRuntimeBuilder::default_multi_thread(None, None, None, None);

    //初始化异步Http客户端
    let httpc =
        AsyncHttpcBuilder::new()
            .bind_address("192.168.35.65")
            .set_default_user_agent("TestHttpClient")
            .new_default_header("XXX_XXX_XXX", "undefined")
            .add_default_header("XXX_XXX_XXX", "null")
            .enable_cookie(true)
            .enable_gzip(true)
            .enable_auto_referer(true)
            .enable_strict(true)
            .set_redirect_limit(10)
            .set_connect_timeout(5000)
            .set_request_timeout(10000)
            .set_connection_timeout(Some(60000))
            .set_host_connection_limit(10)
            .build().unwrap();

    let httpc_copy = httpc.clone();
    rt.spawn(async move {
        let now = Instant::now();
        match httpc_copy
            .build_request("https://newpttest.17youx.cn:8443/api/sso/random", AsyncHttpRequestMethod::Get)
            .set_pairs(&[("login_type", "2"), ("user", "1694151132349ldxNJ")])
            .send().await {
            Err(e) => println!("!!!!!!test request failed, e: {:?}", e),
            Ok(mut resp) => {
                println!("!!!!!!request time: {:?}", now.elapsed());

                loop {
                    match resp.get_body().await {
                        Err(e) => {
                            println!("!!!!!!get body failed, reason: {:?}", e);
                            break;
                        },
                        Ok(Some(_body)) => {
                            continue;
                        },
                        Ok(None) => {
                            println!("!!!!!!response time: {:?}", now.elapsed());
                            println!("!!!!!!peer address: {:?}", resp.get_peer_addr());
                            println!("!!!!!!url: {}", resp.get_url());
                            println!("!!!!!!status: {}", resp.get_status());
                            println!("!!!!!!version: {}", resp.get_version());
                            println!("!!!!!!headers: {:#?}", resp.to_headers());
                            println!("!!!!!!body len: {:?}", resp.get_headers("content-length"));
                            break;
                        },
                    }
                }
            },
        }
    });

    thread::sleep(Duration::from_millis(10000000));
}

