use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use env_logger;
use tokio;

use http::{
    batch_load::BatchLoad,
    cors_handler::CORSHandler,
    default_parser::DefaultParser,
    file_load::FileLoad,
    files_load::FilesLoad,
    gateway::GatewayContext,
    middleware::{Middleware, MiddlewareChain, MiddlewareResult},
    multi_parts::MutilParts,
    port::HttpPort,
    range_load::RangeLoad,
    request::HttpRequest,
    response::{HttpResponse, ResponseHandler},
    route::HttpRoute,
    server::HttpListenerFactory,
    static_cache::StaticCache,
    upload::UploadFile,
    util::HttpRecvResult,
    virtual_host::{VirtualHost, VirtualHostPool, VirtualHostTab},
};
use r#async::rt::{
    multi_thread::{MultiTaskPool, MultiTaskRuntime},
    AsyncRuntime,
};
use tcp::buffer_pool::WriteBufferPool;
use tcp::connect::TcpSocket;
use tcp::driver::SocketConfig;
use tcp::server::{AsyncPortsFactory, SocketListener};
use tcp::tls_connect::TlsSocket;
use tcp::util::TlsConfig;

use async_httpc::{
    AsyncHttpForm, AsyncHttpRequestBody, AsyncHttpRequestMethod, AsyncHttpc, AsyncHttpcBuilder,
};

#[test]
fn test_http_request() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    //构建全局静态资源缓存，并启动缓存的整理
    let cache = Arc::new(StaticCache::new(1024 * 1024 * 1024, 99999));
    StaticCache::run_collect(cache.clone(), "test http cache".to_string(), 10000);

    //构建中间件
    let cors_handler = CORSHandler::new("OPTIONS, GET, POST".to_string(), None);
    cors_handler.allow_origin(
        "http".to_string(),
        "msg.highapp.com".to_string(),
        80,
        &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()],
        &[],
        Some(10),
    );
    cors_handler.allow_origin(
        "http".to_string(),
        "127.0.0.1".to_string(),
        80,
        &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()],
        &[],
        Some(10),
    );
    let cors_handler = Arc::new(cors_handler);
    let parser = Arc::new(DefaultParser::with(128, None));
    let multi_parts = Arc::new(MutilParts::with(8 * 1024 * 1024));
    let range_load = Arc::new(RangeLoad::new());
    let file_load = Arc::new(FileLoad::new(
        "../htdocs",
        Some(cache.clone()),
        true,
        true,
        true,
        false,
        10,
    ));
    let files_load = Arc::new(FilesLoad::new(
        "../htdocs",
        Some(cache.clone()),
        true,
        true,
        true,
        false,
        10,
    ));
    let batch_load = Arc::new(BatchLoad::new(
        "../htdocs",
        Some(cache.clone()),
        true,
        true,
        true,
        false,
        10,
    ));
    let upload = Arc::new(UploadFile::new("../upload"));

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

    //构建路由
    let mut route = HttpRoute::new();
    route
        .at("/")
        .options(cors_middleware.clone())
        .at("/**")
        .options(cors_middleware)
        .at("/")
        .get(file_load_middleware.clone())
        .at("/**")
        .get(file_load_middleware.clone())
        .at("/")
        .post(file_load_middleware.clone())
        .at("/**")
        .post(file_load_middleware)
        .at("/fs")
        .get(files_load_middleware.clone())
        .at("/fs")
        .post(files_load_middleware)
        .at("/batch")
        .get(batch_load_middleware.clone())
        .at("/batch")
        .post(batch_load_middleware)
        .at("/upload")
        .post(upload_middleware.clone());

    //构建虚拟主机
    let host = VirtualHost::with(route);

    //设置虚拟主机
    let mut hosts = VirtualHostTab::new();
    hosts.add("msg.highapp.com", host.clone());
    hosts.add("127.0.0.1", host);

    let mut factory = AsyncPortsFactory::<TcpSocket>::new();
    factory.bind(
        80,
        Box::new(HttpListenerFactory::<TcpSocket, _>::with_hosts(
            hosts, 10000,
        )),
    );
    let mut config = SocketConfig::new("0.0.0.0", factory.bind_ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();

    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Http Listener Bind Error, reason: {:?}", e);
        }
        Ok(driver) => {
            println!("===> Http Listener Bind Ok");
        }
    }

    //初始异步运行时
    let pool = MultiTaskPool::new("Test-Async-Httpc".to_string(), 2, 1024 * 1024, 10, None);
    let rt = pool.startup(false);

    //初始化异步Http客户端
    let httpc = AsyncHttpcBuilder::new()
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
        .build()
        .unwrap();

    //批量获取文件
    let httpc_copy = httpc.clone();
    rt.spawn(rt.alloc(), async move {
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
    rt.spawn(rt.alloc(), async move {
        match httpc_copy
            .build_request("http://127.0.0.1/", AsyncHttpRequestMethod::Post)
            .add_header("content-type", "application/x-www-form-urlencoded")
            .set_body(body)
            .send()
            .await
        {
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
                        }
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        }
                        Ok(None) => break,
                    }
                }
            }
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
    form = form
        .add_file(
            "content".to_string(),
            "mdb_stat.exe".to_string(),
            "application/octet-stream".to_string(),
            bin,
        )
        .unwrap();
    rt.spawn(rt.alloc(), async move {
        match httpc_copy
            .build_request("http://127.0.0.1/upload", AsyncHttpRequestMethod::Post)
            .set_body(form.into_body())
            .send()
            .await
        {
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
                        }
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        }
                        Ok(None) => break,
                    }
                }
            }
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //移除指定文件
    println!("");
    let httpc_copy = httpc.clone();
    let mut form = AsyncHttpRequestBody::form();
    form = form.add_field("method".to_string(), "_$remove".to_string());
    form = form.add_field("file_name".to_string(), "mdb_stat.exe".to_string());
    rt.spawn(rt.alloc(), async move {
        match httpc_copy
            .build_request("http://127.0.0.1/upload", AsyncHttpRequestMethod::Post)
            .set_body(form.into_body())
            .send()
            .await
        {
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
                        }
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        }
                        Ok(None) => break,
                    }
                }
            }
        }
    });
    thread::sleep(Duration::from_millis(5000));

    thread::sleep(Duration::from_millis(10000000));
}

#[test]
fn test_https_request() {
    //启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    //构建全局静态资源缓存，并启动缓存的整理
    let cache = Arc::new(StaticCache::new(1024 * 1024 * 1024, 99999));
    StaticCache::run_collect(cache.clone(), "test http cache".to_string(), 10000);

    //构建中间件
    let cors_handler = CORSHandler::new("OPTIONS, GET, POST".to_string(), None);
    cors_handler.allow_origin(
        "http".to_string(),
        "msg.highapp.com".to_string(),
        80,
        &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()],
        &[],
        Some(10),
    );
    cors_handler.allow_origin(
        "http".to_string(),
        "127.0.0.1".to_string(),
        80,
        &["OPTIONS".to_string(), "GET".to_string(), "POST".to_string()],
        &[],
        Some(10),
    );
    let cors_handler = Arc::new(cors_handler);
    let parser = Arc::new(DefaultParser::with(128, None));
    let multi_parts = Arc::new(MutilParts::with(8 * 1024 * 1024));
    let range_load = Arc::new(RangeLoad::new());
    let file_load = Arc::new(FileLoad::new(
        "../htdocs",
        Some(cache.clone()),
        true,
        true,
        true,
        false,
        10,
    ));
    let files_load = Arc::new(FilesLoad::new(
        "../htdocs",
        Some(cache.clone()),
        true,
        true,
        true,
        false,
        10,
    ));
    let batch_load = Arc::new(BatchLoad::new(
        "../htdocs",
        Some(cache.clone()),
        true,
        true,
        true,
        false,
        10,
    ));
    let upload = Arc::new(UploadFile::new("../upload"));

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

    //构建路由
    let mut route = HttpRoute::new();
    route
        .at("/")
        .options(cors_middleware.clone())
        .at("/**")
        .options(cors_middleware)
        .at("/")
        .get(file_load_middleware.clone())
        .at("/**")
        .get(file_load_middleware.clone())
        .at("/")
        .post(file_load_middleware.clone())
        .at("/**")
        .post(file_load_middleware)
        .at("/fs")
        .get(files_load_middleware.clone())
        .at("/fs")
        .post(files_load_middleware)
        .at("/batch")
        .get(batch_load_middleware.clone())
        .at("/batch")
        .post(batch_load_middleware)
        .at("/upload")
        .post(upload_middleware.clone());

    //构建虚拟主机
    let host = VirtualHost::with(route);

    //设置虚拟主机
    let mut hosts = VirtualHostTab::new();
    hosts.add("msg.highapp.com", host.clone());
    hosts.add("127.0.0.1", host);

    let mut factory = AsyncPortsFactory::<TlsSocket>::new();
    factory.bind(
        443,
        Box::new(HttpListenerFactory::<TlsSocket, _>::with_hosts(
            hosts, 30000,
        )),
    );
    let tls_config = TlsConfig::new_server(
        "",
        false,
        "./3376363_msg.highapp.com.pem",
        "./3376363_msg.highapp.com.key",
        "",
        "",
        "",
        512,
        false,
        "",
    )
    .unwrap();
    let mut config = SocketConfig::with_tls("0.0.0.0", &[(443, tls_config)]);
    config.set_option(16384, 16384, 16384, 16);
    let buffer = WriteBufferPool::new(10000, 10, 3).ok().unwrap();
    match SocketListener::bind(factory, buffer, config, 1024, 1024 * 1024, 1024, Some(10)) {
        Err(e) => {
            println!("!!!> Https Listener Bind Error, reason: {:?}", e);
        }
        Ok(driver) => {
            println!("===> Https Listener Bind Ok");
        }
    }

    //初始异步运行时
    let pool = MultiTaskPool::new("Test-Async-Httpc".to_string(), 2, 1024 * 1024, 10, None);
    let rt = pool.startup(false);

    //初始化异步Http客户端
    let httpc = AsyncHttpcBuilder::new()
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
        .build()
        .unwrap();

    //批量获取文件
    let httpc_copy = httpc.clone();
    rt.spawn(rt.alloc(), async move {
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
    rt.spawn(rt.alloc(), async move {
        match httpc_copy
            .build_request("http://msg.highapp.com/", AsyncHttpRequestMethod::Post)
            .add_header("content-type", "application/x-www-form-urlencoded")
            .set_body(body)
            .send()
            .await
        {
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
                        }
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        }
                        Ok(None) => break,
                    }
                }
            }
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
    form = form
        .add_file(
            "content".to_string(),
            "mdb_stat.exe".to_string(),
            "application/octet-stream".to_string(),
            bin,
        )
        .unwrap();
    rt.spawn(rt.alloc(), async move {
        match httpc_copy
            .build_request(
                "https://msg.highapp.com/upload",
                AsyncHttpRequestMethod::Post,
            )
            .set_body(form.into_body())
            .send()
            .await
        {
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
                        }
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        }
                        Ok(None) => break,
                    }
                }
            }
        }
    });
    thread::sleep(Duration::from_millis(5000));

    //移除指定文件
    println!("");
    let httpc_copy = httpc.clone();
    let mut form = AsyncHttpRequestBody::form();
    form = form.add_field("method".to_string(), "_$remove".to_string());
    form = form.add_field("file_name".to_string(), "mdb_stat.exe".to_string());
    rt.spawn(rt.alloc(), async move {
        match httpc_copy
            .build_request(
                "https://msg.highapp.com/upload",
                AsyncHttpRequestMethod::Post,
            )
            .set_body(form.into_body())
            .send()
            .await
        {
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
                        }
                        Ok(Some(body)) => {
                            println!("!!!!!!get body ok, len: {}", body.len());
                        }
                        Ok(None) => break,
                    }
                }
            }
        }
    });
    thread::sleep(Duration::from_millis(5000));

    thread::sleep(Duration::from_millis(10000000));
}
