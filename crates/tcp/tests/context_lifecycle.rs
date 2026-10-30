//! 连接侧绑定（binding）的实际生命周期（lifecycle）验证，对应BL-V01～V06。
//! TCP/TLS分别经过公开监听器（listener）、所属运行时（owner runtime）和正常关闭回调（closed callback）。
//! 每个场景（scenario）使用独立且有截止的进程（process），仅通过公开接口（API）操作。

use std::cell::{Cell, UnsafeCell};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use bytes::Buf;
use crossbeam_channel::{bounded, Sender};
use futures::{channel::oneshot, future::{FutureExt, LocalBoxFuture}};
use pi_async_rt::rt::{serial::AsyncRuntimeBuilder, serial_local_thread::LocalTaskRuntime};
use pi_tcp::{AsyncService, Socket, Stream, SocketAdapterFactory, SocketConfig,
    SocketContextGetError, SocketDriver, SocketHandle, SocketStatus};
use pi_tcp::connect::TcpSocket;
use pi_tcp::connect_pool::TcpSocketPool;
use pi_tcp::server::{PortsAdapter, PortsAdapterFactory, SocketListener};
use pi_tcp::tls_connect::TlsSocket;
use pi_tcp::utils::TlsConfig;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode { Direct, Cache, Pending }

#[derive(Default)]
struct State {
    drops: AtomicUsize,
    entered: AtomicBool,
    left: AtomicBool,
    cache_checked: AtomicBool,
}

/// 业务值（value）保留本地Rc，并在归还时经公开取得接口（API）观察解除已完成。
struct Item<S: Socket> {
    handle: SocketHandle<S>,
    owner: ThreadId,
    number: Rc<Cell<u32>>,
    state: Arc<State>,
}

impl<S: Socket> Drop for Item<S> {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.owner);
        assert_eq!(self.number.get(), 74);
        assert!(self.state.left.load(Ordering::SeqCst));
        assert!(matches!(self.handle.get_context(), Err(SocketContextGetError::Unavailable)));
        assert!(self.handle.get_context_binding().detach_context().unwrap().is_none());
        assert_eq!(self.state.drops.fetch_add(1, Ordering::SeqCst), 0);
    }
}

struct Service<S: Socket> {
    runtime: Mutex<LocalTaskRuntime<()>>,
    mode: Mode,
    state: Arc<State>,
    connected: Sender<SocketHandle<S>>,
    closed: Sender<SocketHandle<S>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl<S: Socket> AsyncService<S> for Service<S> {
    /// 普通句柄（context handle）与容器（container）跨本地等待（await），访问许可（access permit）提前归还。
    fn handle_connected(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let state = self.state.clone();
        let connected = self.connected.clone();
        async move {
            assert!(matches!(status, SocketStatus::Connected(Ok(()))));
            let context = handle.get_context().unwrap();
            assert!(Rc::ptr_eq(&context, &handle.get_context().unwrap()));
            assert!(context.is_empty());
            assert!(context.set(Item {
                handle: handle.clone(), owner: thread::current().id(),
                number: Rc::new(Cell::new(73)), state,
            }));
            let cached = context.get::<Item<S>>().unwrap();
            {
                let mut view = cached.try_write().unwrap();
                view.as_mut().number.set(74);
                assert!(Rc::ptr_eq(&context, &handle.get_context().unwrap()));
            }
            connected.send(handle.clone()).unwrap();
            let length = match handle.read_ready(1) {
                Ok(wait) => wait.await,
                Err(length) => length,
            };
            assert!(length >= 1);
            {
                let buffer = handle.get_read_buffer();
                // 原读取接口（read API）的短借用（borrow），不保存到等待（await）步骤。
                let bytes = unsafe { &mut *buffer.get() }.as_mut().unwrap();
                assert_eq!(bytes[0], 23);
                bytes.advance(1);
            }
            assert_eq!(cached.try_read().unwrap().as_ref().number.get(), 74);
            drop(cached);
            drop(context);
            handle.close(Ok(())).unwrap();
        }.boxed_local()
    }

    /// 输入由建连任务（connected task）的公开读取等待（read wait）消费。
    fn handle_readed(&self, _handle: SocketHandle<S>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }

    /// 本场景（scenario）不提交应用输出（application output）。
    fn handle_writed(&self, _handle: SocketHandle<S>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }

    /// 原回调（callback）返回前仍可取得；缓存（cache）分支由所属线程（owner thread）随后观察独立持有。
    fn handle_closed(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let state = self.state.clone();
        let closed = self.closed.clone();
        let mode = self.mode;
        let release = self.release.lock().unwrap().take();
        let runtime = self.runtime.lock().unwrap().clone();
        async move {
            assert!(matches!(status, SocketStatus::Closed(Ok(()))));
            assert!(!state.entered.swap(true, Ordering::SeqCst));
            let context = handle.get_context().unwrap();
            let cached = context.get::<Item<S>>().unwrap();
            assert_eq!(cached.try_read().unwrap().as_ref().number.get(), 74);
            closed.send(handle.clone()).unwrap();
            match mode {
                Mode::Direct => {
                    drop(cached);
                    drop(context);
                },
                Mode::Pending => {
                    release.unwrap().await.unwrap();
                    assert!(Rc::ptr_eq(&context, &handle.get_context().unwrap()));
                    assert_eq!(cached.try_read().unwrap().as_ref().number.get(), 74);
                    drop(cached);
                    drop(context);
                },
                Mode::Cache => {
                    let observed = state.clone();
                    // 此投递（spawn）发生在原回调（callback）的所属线程（owner thread）上。
                    runtime.spawn(async move {
                        release.unwrap().await.unwrap();
                        assert!(matches!(handle.get_context(), Err(SocketContextGetError::Unavailable)));
                        assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
                        assert_eq!(cached.try_read().unwrap().as_ref().number.get(), 74);
                        assert_eq!(context.get::<Item<S>>().unwrap().try_read().unwrap().as_ref().number.get(), 74);
                        drop(context);
                        assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
                        assert_eq!(cached.try_read().unwrap().as_ref().number.get(), 74);
                        drop(cached);
                        assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
                        observed.cache_checked.store(true, Ordering::SeqCst);
                    });
                },
            }
            state.left.store(true, Ordering::SeqCst);
        }.boxed_local()
    }

    /// 本场景（scenario）不设置定时事件（timer event）。
    fn handle_timeouted(&self, _handle: SocketHandle<S>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
}

/// 外部线程（thread）只经send提交观察，断言（assertion）在所属线程（owner thread）执行。
fn observe<S: Socket>(runtime: &LocalTaskRuntime<()>, handle: SocketHandle<S>, attached: bool) {
    let (sent, received) = bounded(1);
    runtime.send(async move {
        if attached {
            let context = handle.get_context().unwrap();
            assert_eq!(context.get::<Item<S>>().unwrap().try_read().unwrap().as_ref().number.get(), 74);
        } else {
            assert!(matches!(handle.get_context(), Err(SocketContextGetError::Unavailable)));
        }
        sent.send(()).unwrap();
    });
    received.recv_timeout(Duration::from_secs(3)).unwrap();
}

fn owner_runtime() -> LocalTaskRuntime<()> {
    let runtime = AsyncRuntimeBuilder::<()>::default_local_thread(Some("ContextOwner"), None);
    let started = Instant::now();
    while !runtime.is_running() {
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::yield_now();
    }
    runtime
}

fn port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
}

/// 仅使用仓库已有静态证书（certificate）数据；不读取其它验证程序（validation program）。
fn server_config() -> TlsConfig {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    TlsConfig::new_server("", false,
        root.join("hibernate-server.pem").to_str().unwrap(),
        root.join("hibernate-server.key").to_str().unwrap(),
        "", "", "", 0, false, "").unwrap()
}

fn client(tls: bool, address: SocketAddr) -> Box<dyn ReadWrite> {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    if !tls { return Box::new(stream); }
    let mut roots = rustls::RootCertStore::empty();
    let mut pem = std::io::Cursor::new(include_bytes!("fixtures/hibernate-ca.pem"));
    for bytes in rustls_pemfile::certs(&mut pem).unwrap() {
        roots.add(&rustls::Certificate(bytes)).unwrap();
    }
    let config = rustls::ClientConfig::builder().with_safe_defaults()
        .with_root_certificates(roots).with_no_client_auth();
    let connection = rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap();
    Box::new(rustls::StreamOwned::new(connection, stream))
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// BL-V01～V04：原监听器（listener）完成正常关闭；仅缓存（cache）分支延后归还本地持有。
fn network_case<S: Socket + Stream>(tls: bool, mode: Mode) {
    let runtime = owner_runtime();
    let port = port();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let (connected_tx, connected_rx) = bounded(2);
    let (closed_tx, closed_rx) = bounded(2);
    let (release_tx, release_rx) = oneshot::channel();
    let state = Arc::new(State::default());
    let mut factory = PortsAdapterFactory::<S>::new();
    factory.bind(port, Box::new(Service {
        runtime: Mutex::new(runtime.clone()), mode, state: state.clone(),
        connected: connected_tx, closed: closed_tx, release: Mutex::new(Some(release_rx)),
    }));
    let config = if tls { SocketConfig::with_tls("127.0.0.1", &[(port, server_config())]) }
        else { SocketConfig::new("127.0.0.1", &[port]) };
    let listener = SocketListener::try_bind(vec![runtime.clone()], factory, config,
        1, 2 * 1024 * 1024, 16, 1, 4096, 4096, Some(10)).unwrap();
    let mut client = client(tls, address);
    let connected = connected_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(connected.get_context(), Err(SocketContextGetError::WrongThread)));
    client.write_all(&[23]).unwrap();
    client.flush().unwrap();
    let closed = closed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(connected.get_token(), closed.get_token());
    assert!(matches!(closed.get_context(), Err(SocketContextGetError::WrongThread)));
    observe(&runtime, closed.clone(), mode == Mode::Pending);
    assert!(state.entered.load(Ordering::SeqCst));
    if mode == Mode::Direct {
        assert!(state.left.load(Ordering::SeqCst));
        assert_eq!(state.drops.load(Ordering::SeqCst), 1);
        drop(release_tx);
    } else {
        assert_eq!(state.drops.load(Ordering::SeqCst), 0);
        assert_eq!(state.left.load(Ordering::SeqCst), mode == Mode::Cache);
        release_tx.send(()).unwrap();
        observe(&runtime, closed.clone(), false);
        assert!(state.left.load(Ordering::SeqCst));
        assert_eq!(state.drops.load(Ordering::SeqCst), 1);
        assert_eq!(state.cache_checked.load(Ordering::SeqCst), mode == Mode::Cache);
    }
    assert!(closed_rx.try_recv().is_err());
    assert!(connected_rx.try_recv().is_err());
    drop(connected);
    thread::spawn(move || {
        assert!(matches!(closed.get_context(), Err(SocketContextGetError::WrongThread)));
        drop(closed);
    }).join().unwrap();
    assert_eq!(state.drops.load(Ordering::SeqCst), 1);
    drop(client);
    listener.close(Ok(()));
}

/// 成对实际流（stream）供公开手工装配（manual assembly）入口使用，无后台接受器（acceptor）。
fn pair() -> (TcpSocket, TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let client = TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(3)).unwrap();
    let (server, remote) = listener.accept().unwrap();
    let local = server.local_addr().unwrap();
    server.set_nonblocking(true).unwrap();
    (TcpSocket::new(&local, &remote, None, mio::net::TcpStream::from_std(server),
        1, 4096, 4096, TlsConfig::Empty), client)
}

/// BL-V03：重复set_handle及全部镜像（SocketImage）共享同一绑定（binding），手工解除后取得一致。
fn mirrors_case() {
    let (mut socket, _client) = pair();
    socket.set_token(Some(mio::Token(1)));
    socket.set_uid(1);
    let (close, _close_rx) = bounded(4);
    let (timer, _timer_rx) = bounded(4);
    socket.set_close_listener(Some(close));
    socket.set_timer_listener(Some(timer));
    let binding = socket.get_context_binding();
    assert!(matches!(socket.get_context(), Err(SocketContextGetError::Unavailable)));
    binding.prepare_context().unwrap();
    let shared = Arc::new(UnsafeCell::new(socket));
    let first = unsafe {
        let current = &mut *shared.get();
        current.set_handle(&shared);
        current.get_handle()
    };
    let first_context = first.get_context().unwrap();
    assert!(first_context.set(73_u32));
    let second = unsafe {
        let current = &mut *shared.get();
        current.set_handle(&shared);
        current.get_handle()
    };
    let second_context = second.get_context().unwrap();
    assert!(Rc::ptr_eq(&first_context, &second_context));
    second.get_context_binding().prepare_context().unwrap();
    let detached = binding.detach_context().unwrap().unwrap();
    let internal = unsafe { (&mut *shared.get()).remove_handle() };
    drop(internal);
    assert!(matches!(first.get_context(), Err(SocketContextGetError::Unavailable)));
    assert!(matches!(second.get_context(), Err(SocketContextGetError::Unavailable)));
    assert_eq!(detached.remove::<u32>().unwrap(), Some(73));
    drop(detached);
    drop(first_context);
    drop(second_context);
    drop(first);
    drop(second);
    assert_eq!(Arc::strong_count(&shared), 1);
}

/// BL-V06：错误线程（thread）已准备的流（stream）被原池拒绝；同批下一连接（connection）仍正常完成。
fn rejected_preparation_case() {
    let runtime = owner_runtime();
    let (bad, mut bad_client) = pair();
    let binding = bad.get_context_binding();
    binding.prepare_context().unwrap();
    let (good, mut good_client) = pair();
    let port = good.get_local().port();
    let good_remote = *good.get_remote();
    let (connected_tx, connected_rx) = bounded(2);
    let (closed_tx, closed_rx) = bounded(2);
    let (_release_tx, release_rx) = oneshot::channel();
    let state = Arc::new(State::default());
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port, Box::new(Service {
        runtime: Mutex::new(runtime.clone()), mode: Mode::Direct, state: state.clone(),
        connected: connected_tx, closed: closed_tx, release: Mutex::new(Some(release_rx)),
    }));
    let (sent, received) = crossbeam_channel::unbounded();
    sent.send(bad).unwrap();
    sent.send(good).unwrap();
    let pool = TcpSocketPool::<TcpSocket, PortsAdapter<TcpSocket>>::with_capacity(
        254, "ContextPreparation".into(), received, SocketConfig::default(), 1).unwrap();
    let mut driver = SocketDriver::new(&[]);
    driver.set_adapter(factory.get_instance());
    pool.run(runtime.clone(), driver, 16, Some(10)).unwrap();
    bad_client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    assert_eq!(bad_client.read(&mut [0_u8; 1]).unwrap(), 0);
    let connected = connected_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(*connected.get_remote(), good_remote);
    good_client.write_all(&[23]).unwrap();
    let closed = closed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    observe(&runtime, closed.clone(), false);
    assert_eq!(state.drops.load(Ordering::SeqCst), 1);
    assert!(connected_rx.try_recv().is_err());
    assert!(closed_rx.try_recv().is_err());
    assert!(binding.get_context().unwrap().is_empty());
    drop(binding.detach_context().unwrap().unwrap());
    drop(connected);
    drop(closed);
    drop(good_client);
    drop(bad_client);
    drop(sent);
    runtime.close();
}

struct PausedService {
    next: Arc<AtomicUsize>,
    connected: Sender<SocketHandle<TcpSocket>>,
    entered: Sender<mio::Token>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl AsyncService<TcpSocket> for PausedService {
    fn handle_connected(&self, handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let next = self.next.clone();
        let connected = self.connected.clone();
        async move {
            assert!(matches!(status, SocketStatus::Connected(Ok(()))));
            assert!(handle.get_context().unwrap().set(next.fetch_add(1, Ordering::SeqCst) + 1));
            connected.send(handle).unwrap();
        }.boxed_local()
    }
    fn handle_readed(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
    fn handle_writed(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
    fn handle_closed(&self, handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let release = self.release.lock().unwrap().take();
        let entered = self.entered.clone();
        async move {
            assert!(matches!(status, SocketStatus::Closed(Ok(()))));
            let context = handle.get_context().unwrap();
            let cached = context.get::<usize>().unwrap();
            let number = *cached.try_read().unwrap().as_ref();
            entered.send(*handle.get_token()).unwrap();
            if let Some(release) = release {
                assert_eq!(number, 1);
                release.await.unwrap();
                assert!(Rc::ptr_eq(&context, &handle.get_context().unwrap()));
                assert_eq!(*cached.try_read().unwrap().as_ref(), 1);
            } else {
                assert_eq!(number, 2);
            }
        }.boxed_local()
    }
    fn handle_timeouted(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
}

/// BL-V01/V02：原A已移表但关闭回调（closed callback）等待；新B入表后，A完成只解除原对象（object）。
fn paused_previous_connection_case() {
    let runtime = owner_runtime();
    let port = port();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let (connected_tx, connected_rx) = bounded(4);
    let (entered_tx, entered_rx) = bounded(4);
    let (release_tx, release_rx) = oneshot::channel();
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port, Box::new(PausedService {
        next: Arc::new(AtomicUsize::new(0)), connected: connected_tx,
        entered: entered_tx, release: Mutex::new(Some(release_rx)),
    }));
    let listener = SocketListener::try_bind(vec![runtime.clone()], factory,
        SocketConfig::new("127.0.0.1", &[port]), 1, 2 * 1024 * 1024, 16, 1, 4096, 4096, Some(10)).unwrap();
    let first_client = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    let first = connected_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let first_token = *first.get_token();
    let closing = first.clone();
    runtime.send(async move { closing.close(Ok(())).unwrap(); });
    assert_eq!(entered_rx.recv_timeout(Duration::from_secs(3)).unwrap(), first_token);
    let second_client = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    let second = connected_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_ne!(first_token, *second.get_token());
    let previous = first.clone();
    let current = second.clone();
    let (checked_tx, checked_rx) = bounded(1);
    runtime.send(async move {
        let old = previous.get_context().unwrap();
        assert_eq!(*old.get::<usize>().unwrap().try_read().unwrap().as_ref(), 1);
        let new = current.get_context().unwrap();
        assert_eq!(*new.get::<usize>().unwrap().try_read().unwrap().as_ref(), 2);
        assert!(!Rc::ptr_eq(&old, &new));
        checked_tx.send(()).unwrap();
    });
    checked_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    release_tx.send(()).unwrap();
    let (checked_tx, checked_rx) = bounded(1);
    let previous = first.clone();
    let current = second.clone();
    runtime.send(async move {
        assert!(matches!(previous.get_context(), Err(SocketContextGetError::Unavailable)));
        let new = current.get_context().unwrap();
        assert_eq!(*new.get::<usize>().unwrap().try_read().unwrap().as_ref(), 2);
        current.close(Ok(())).unwrap();
        checked_tx.send(()).unwrap();
    });
    checked_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(entered_rx.recv_timeout(Duration::from_secs(3)).unwrap(), *second.get_token());
    let (checked_tx, checked_rx) = bounded(1);
    runtime.send(async move {
        assert!(matches!(first.get_context(), Err(SocketContextGetError::Unavailable)));
        assert!(matches!(second.get_context(), Err(SocketContextGetError::Unavailable)));
        checked_tx.send(()).unwrap();
    });
    checked_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(entered_rx.try_recv().is_err());
    drop(first_client);
    drop(second_client);
    listener.close(Ok(()));
}

/// 子入口（child entry）只执行一个选定场景（scenario）；外层进程（process）负责截止（deadline）。
#[test]
#[ignore]
fn test_lifecycle_child() {
    match std::env::var("PI_NET_LIFECYCLE_MODE").unwrap().as_str() {
        "tcp_direct" => network_case::<TcpSocket>(false, Mode::Direct),
        "tcp_cache" => network_case::<TcpSocket>(false, Mode::Cache),
        "tcp_pending" => network_case::<TcpSocket>(false, Mode::Pending),
        "tls_direct" => network_case::<TlsSocket>(true, Mode::Direct),
        "tls_cache" => network_case::<TlsSocket>(true, Mode::Cache),
        "tls_pending" => network_case::<TlsSocket>(true, Mode::Pending),
        "mirrors" => mirrors_case(),
        "rejected" => rejected_preparation_case(),
        "previous_pending" => paused_previous_connection_case(),
        _ => unreachable!(),
    }
}

/// 串行子进程（subprocess）有25秒截止（deadline），先等待前项结束再开始后项。
fn run_child(mode: &str) {
    let path = std::env::temp_dir().join(format!("pi-net-lifecycle-{}-{mode}.txt", std::process::id()));
    let output = std::fs::OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "test_lifecycle_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PI_NET_LIFECYCLE_MODE", mode).env("RUST_BACKTRACE", "0")
        .stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() { break status; }
        if start.elapsed() >= Duration::from_secs(25) {
            child.kill().unwrap();
            child.wait().unwrap();
            let output = std::fs::read_to_string(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            panic!("lifecycle child exceeded deadline: {mode}\n{output}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    println!("{mode}\n{output}");
    assert!(status.success(), "lifecycle child mode={mode} status={status}");
}

/// BL-V01/V02/V04：TCP正常解除及归还顺序（release order）。
#[test]
fn test_tcp_direct() { run_child("tcp_direct"); }
/// BL-V01/V02/V04：TLS正常解除及归还顺序（release order）。
#[test]
fn test_tls_direct() { run_child("tls_direct"); }
/// BL-V01/V02：TCP缓存（cache）跨解除继续真实持有。
#[test]
fn test_tcp_cache() { run_child("tcp_cache"); }
/// BL-V01/V02：TLS缓存（cache）跨解除继续真实持有。
#[test]
fn test_tls_cache() { run_child("tls_cache"); }
/// BL-V01：TCP关闭回调（closed callback）等待时不提前解除。
#[test]
fn test_tcp_pending() { run_child("tcp_pending"); }
/// BL-V01：TLS关闭回调（closed callback）等待时不提前解除。
#[test]
fn test_tls_pending() { run_child("tls_pending"); }
/// BL-V03：同一连接（socket）重复装配镜像（SocketImage）。
#[test]
fn test_shared_mirrors() { run_child("mirrors"); }
/// BL-V06：原池拒绝准备不匹配项，继续处理同批其它连接（connection）。
#[test]
fn test_rejected_preparation() { run_child("rejected"); }
/// BL-V01/V02：旧关闭任务（close task）延后完成与新连接（connection）隔离。
#[test]
fn test_previous_pending_connection() { run_child("previous_pending"); }
