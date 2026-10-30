//! 主线第43节：真实HTTP连续请求（request）及等待（await）期间关闭的必要验证（validation）。
//! 原SocketListener/HttpListener处理网络（network）和清理，测试（test）只提供业务服务（service）。
//! 每项在25秒截止的子进程（subprocess）内执行；读写和通知（notification）各限3秒。

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}, mpsc::{sync_channel, SyncSender}};
use std::thread;
use std::time::{Duration, Instant};

use futures::{channel::oneshot, future::{FutureExt, LocalBoxFuture}};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use pi_http::{connect::HttpConnect, request::HttpRequest, response::HttpResponse,
    server::{HttpConnectionEvent, HttpListenerFactory}, service::{HttpService, ServiceFactory},
    virtual_host::VirtualHostPool};
use tcp::{SocketConfig, SocketContextGetError, SocketHandle, connect::TcpSocket,
    server::{PortsAdapterFactory, SocketListener}, utils::ContextAccessError};

struct Shared {
    created: AtomicUsize,
    request: SyncSender<(usize, SocketHandle<TcpSocket>)>,
    resumed: SyncSender<()>,
    pause: Mutex<Option<oneshot::Receiver<()>>>,
}

#[derive(Clone)]
struct Host(Arc<Shared>);
struct Service { shared: Arc<Shared>, number: usize }

impl ServiceFactory<TcpSocket> for Host {
    type Service = Service;
    fn new_service(&self) -> Self::Service {
        Service { shared: self.0.clone(), number: self.0.created.fetch_add(1, Ordering::SeqCst) + 1 }
    }
}

impl HttpService<TcpSocket> for Service {
    type Error = std::io::Error;
    fn call(&mut self, request: HttpRequest<TcpSocket>) -> LocalBoxFuture<'static, Result<HttpResponse, Self::Error>> {
        let shared = self.shared.clone();
        let number = self.number;
        let handle = request.get_handle().clone();
        assert_eq!(request.url().path(), format!("/{number}"));
        let pause = if number == 2 { shared.pause.lock().unwrap().take() } else { None };
        async move {
            let context = handle.get_context().unwrap();
            let old = if number > 1 {
                let stored = context.get::<HttpConnect<TcpSocket, Host, Service>>().unwrap();
                // 原克隆（clone）必须在调用服务（service）前结束读取许可（read permit）。
                let view = stored.try_write().expect("the caller permit must end before service execution");
                drop(view);
                Some(stored)
            } else { assert!(context.is_empty()); None };
            drop(context);
            shared.request.send((number, handle)).unwrap();
            if let Some(pause) = pause {
                pause.await.unwrap();
                assert!(matches!(old.as_ref().unwrap().try_read(), Err(ContextAccessError::Closed)));
                shared.resumed.send(()).unwrap();
                return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "request completed after close"));
            }
            let body = format!("reply-{number}");
            let mut response = HttpResponse::new(1);
            response.header("content-length", &body.len().to_string());
            let buffer = response.as_mut_body().unwrap();
            assert!(buffer.init());
            buffer.push(body.as_bytes());
            Ok(response)
        }.boxed_local()
    }
}

#[derive(Clone)]
struct Hosts(Host);
impl VirtualHostPool<TcpSocket> for Hosts {
    type Host = Host;
    fn size(&self) -> usize { 1 }
    fn get(&self, _: &str) -> Option<&Host> { Some(&self.0) }
    fn add(&mut self, _: &str, host: Host) -> std::io::Result<()> { self.0 = host; Ok(()) }
    fn add_default(&mut self, host: Host) -> std::io::Result<()> { self.0 = host; Ok(()) }
}

fn read_response(client: &mut TcpStream, number: usize) {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 8192);
        let mut byte = [0_u8; 1]; client.read_exact(&mut byte).unwrap(); header.push(byte[0]);
    }
    assert!(header.starts_with(b"HTTP/1.1 200"));
    let header = String::from_utf8(header).unwrap().to_ascii_lowercase();
    let length = header.lines().find_map(|line| line.strip_prefix("content-length:")).unwrap().trim().parse::<usize>().unwrap();
    let mut body = vec![0; length]; client.read_exact(&mut body).unwrap();
    assert_eq!(body, format!("reply-{number}").as_bytes());
}

fn requests(pending: bool) {
    let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let runtime = AsyncRuntimeBuilder::<()>::default_local_thread(Some("RequestOwner"), None);
    let (request_tx, request_rx) = sync_channel(4);
    let (resumed_tx, resumed_rx) = sync_channel(1);
    let (closed_tx, closed_rx) = sync_channel(2);
    let (pause_tx, pause_rx) = oneshot::channel();
    let shared = Arc::new(Shared { created: AtomicUsize::new(0), request: request_tx,
        resumed: resumed_tx, pause: Mutex::new(if pending { Some(pause_rx) } else { None }) });
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port, HttpListenerFactory::with_hosts_and_handler(Hosts(Host(shared.clone())), 60000,
        move |event| { if matches!(event, HttpConnectionEvent::Closed(_, _)) { closed_tx.send(()).unwrap(); } }).new_service());
    let listener = SocketListener::try_bind(vec![runtime.clone()], factory, SocketConfig::new("127.0.0.1", &[port]),
        1, 2 * 1024 * 1024, 16, 1, 4096, 4096, Some(10)).unwrap();
    let mut client = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(3)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    client.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut last: Option<SocketHandle<TcpSocket>> = None;
    for number in 1..=2 {
        client.write_all(format!("GET /{number} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n").as_bytes()).unwrap();
        let (observed, handle) = request_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(number, observed);
        if let Some(previous) = &last { assert_eq!(previous.get_uid(), handle.get_uid()); }
        last = Some(handle);
        if pending && number == 2 { break; }
        read_response(&mut client, number);
    }
    assert_eq!(shared.created.load(Ordering::SeqCst), 2);
    client.shutdown(Shutdown::Both).unwrap();
    closed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let handle = last.unwrap();
    let (fence_tx, fence_rx) = sync_channel(1);
    runtime.send(async move {
        assert!(matches!(handle.get_context(), Err(SocketContextGetError::Unavailable)));
        fence_tx.send(()).unwrap();
    });
    fence_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    if pending { pause_tx.send(()).unwrap(); resumed_rx.recv_timeout(Duration::from_secs(3)).unwrap(); }
    else { drop(pause_tx); }
    assert!(request_rx.try_recv().is_err());
    assert!(closed_rx.try_recv().is_err());
    listener.close(Ok(()));
}

#[test]
#[ignore]
fn test_requests_child() { requests(std::env::var("PI_HTTP_CONTEXT_REQUESTS").unwrap() == "pending"); }

fn run_child(mode: &str) {
    let path = std::env::temp_dir().join(format!("pi-http-context-{}-{mode}.txt", std::process::id()));
    let file = std::fs::OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "test_requests_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PI_HTTP_CONTEXT_REQUESTS", mode).env("RUST_BACKTRACE", "0")
        .stdout(Stdio::from(file.try_clone().unwrap())).stderr(Stdio::from(file));
    #[cfg(unix)]
    { use std::os::unix::process::CommandExt; command.process_group(0); }
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() { break status; }
        if started.elapsed() >= Duration::from_secs(25) {
            child.kill().unwrap(); child.wait().unwrap();
            let text = std::fs::read_to_string(&path).unwrap(); std::fs::remove_file(&path).unwrap();
            panic!("request sequence exceeded deadline: {mode}\n{text}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let text = std::fs::read_to_string(&path).unwrap(); std::fs::remove_file(&path).unwrap();
    println!("{mode}\n{text}"); assert!(status.success(), "request sequence status={status}");
}

/// 连续请求（request）使用原连接（connection）身份和原服务（service）克隆（clone）规则。
#[test]
fn test_two_requests() { run_child("normal"); }
/// 第二次请求（request）等待（await）时关闭，原会话（session）仍可移出，旧句柄（context handle）终止。
#[test]
fn test_close_during_second_request() { run_child("pending"); }
