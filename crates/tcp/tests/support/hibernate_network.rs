//! Hibernate 真实网络夹具：原监听器、原 Local、原 TCP/TLS、外部唤醒线程。
//!
//! 不代替休眠状态转换、不访问生产锁、不手工补发网络完成信号。每个请求只有一个
//! 字节，客户端逐次请求/回应；服务端先消费真实输入，再原样休眠/外部唤醒并回复。
//! Echo 对照跳过休眠但保留同一网络路径。只在所属 Local 访问接收缓冲区。

use super::tcp;

use std::future::Future;
use std::io::{self, BufReader, Read, Write};
use std::marker::PhantomData;
use std::net::{TcpListener, TcpStream};
use std::pin::Pin;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::task::{Context, Poll};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use bytes::Buf;
use crossbeam_channel::{bounded, Receiver, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use tcp::{AsyncService, Socket, Stream, SocketConfig, SocketHandle, SocketStatus};
use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;
use tcp::server::{PortsAdapterFactory, SocketListener};
use tcp::utils::{Hibernate, Ready, TlsConfig};

/// 两种真实传输；TLS 必须通过测试 CA 和 localhost 名称校验。
#[derive(Clone, Copy, Debug)]
pub(super) enum Transport { Tcp, Tls }

/// 外部线程只持有公开的安全句柄；不传递 Rc、缓冲区或 Local 本身。
pub(super) enum WakeTarget {
    Tcp(SocketHandle<TcpSocket>),
    Tls(SocketHandle<TlsSocket>),
}

impl WakeTarget {
    /// 用镜像公开 UID 关联测试探针，不用 UID 恢复生产对象地址。
    pub(super) fn uid(&self) -> usize {
        match self { Self::Tcp(h) => h.get_uid(), Self::Tls(h) => h.get_uid() }
    }

    /// 原样调用生产 wakeup；false 不伪装为完成，重试由有界外部测试线程负责。
    fn wakeup(&self) -> bool {
        match self { Self::Tcp(h) => h.wakeup(Ok(())), Self::Tls(h) => h.wakeup(Ok(())) }
    }
}

trait FixtureSocket: Socket + Stream {
    fn target(handle: SocketHandle<Self>) -> WakeTarget;
}

impl FixtureSocket for TcpSocket {
    fn target(handle: SocketHandle<Self>) -> WakeTarget { WakeTarget::Tcp(handle) }
}

impl FixtureSocket for TlsSocket {
    fn target(handle: SocketHandle<Self>) -> WakeTarget { WakeTarget::Tls(handle) }
}

/// 同一夹具的功能模式；force_repoll 仅用于已批准的确定性交错，基准固定为 false。
#[derive(Clone)]
pub(super) struct ServiceMode {
    pub jobs: Option<Sender<WakeTarget>>,
    pub force_repoll: bool,
    // 基准不测会话恢复；保留原专项配置，避免改变已观察的交错前提。
    pub suppress_tls_tickets: bool,
}

/// 返回 Pending 后才发布外部唤醒工作；没有在脚手架中设置生产结果或推进运行时。
struct RegisteredSleep<S: Socket> {
    sleep: Hibernate<S>,
    target: Option<WakeTarget>,
    jobs: Sender<WakeTarget>,
    force_repoll: bool,
}

impl<S: Socket> Unpin for RegisteredSleep<S> {}

impl<S: Socket> Future for RegisteredSleep<S> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.sleep).poll(cx);
        if result.is_pending() {
            if let Some(target) = this.target.take() {
                this.jobs.try_send(target).unwrap_or_else(|_| panic!("外部唤醒工作队列容量不足或已退出"));
                if this.force_repoll { cx.waker().wake_by_ref(); }
            }
        }
        result
    }
}

struct NetworkService<S> {
    mode: ServiceMode,
    connected: Sender<WakeTarget>,
    errors: Sender<String>,
    marker: PhantomData<fn() -> S>,
}

impl<S: FixtureSocket> AsyncService<S> for NetworkService<S> {
    fn handle_connected(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let connected = self.connected.clone();
        let errors = self.errors.clone();
        async move {
            if matches!(status, SocketStatus::Connected(Ok(()))) {
                connected.try_send(S::target(handle)).unwrap_or_else(|_| panic!("测试建连通知队列失效"));
            } else { let _ = errors.try_send(format!("建连回调失败: {status:?}")); }
        }.boxed_local()
    }

    fn handle_readed(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let mode = self.mode.clone();
        let errors = self.errors.clone();
        async move {
            if !matches!(status, SocketStatus::Readed(Ok(_))) {
                let _ = errors.try_send(format!("读取回调失败: {status:?}"));
                return;
            }
            let byte = {
                let buffer = handle.get_read_buffer();
                // 接收缓冲区只由当前连接所属 Local 消费，借用在 await 前结束；
                // 外部线程仅调用 wakeup，不访问该 Rc 或其中 BytesMut。
                let buffer = unsafe { &mut *buffer.get() };
                buffer.as_mut().and_then(|bytes| {
                    if bytes.is_empty() { None } else {
                        let byte = bytes[0];
                        bytes.advance(1);
                        Some(byte)
                    }
                })
            };
            let Some(byte) = byte else { return };
            if let Some(jobs) = mode.jobs {
                let sleep = handle.hibernate(handle.clone(), Ready::ReadWrite)
                    .expect("已建立连接不能构造休眠对象");
                let sleep = RegisteredSleep {
                    sleep, target: Some(S::target(handle.clone())), jobs,
                    force_repoll: mode.force_repoll,
                };
                if let Err(error) = sleep.await {
                    let _ = errors.try_send(format!("休眠恢复失败: {error}"));
                    return;
                }
            }
            if let Err(error) = handle.write_ready(vec![byte]) {
                let _ = errors.try_send(format!("回复真实网络数据失败: {error}"));
            }
        }.boxed_local()
    }

    fn handle_writed(&self, _handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let errors = self.errors.clone();
        async move {
            if !matches!(status, SocketStatus::Writed(Ok(_))) {
                let _ = errors.try_send(format!("发送回调失败: {status:?}"));
            }
        }.boxed_local()
    }

    fn handle_closed(&self, _handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let errors = self.errors.clone();
        async move {
            if let SocketStatus::Closed(Err(error)) = status {
                let _ = errors.try_send(format!("服务端异常关闭: {error}"));
            }
        }.boxed_local()
    }

    fn handle_timeouted(&self, _handle: SocketHandle<S>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
}

/// 原监听器的独占关闭责任；运行时不跨线程驱动，退出不能替代在途休眠的完成断言。
pub(super) struct Server {
    pub port: u16,
    pub connected: Receiver<WakeTarget>,
    pub errors: Receiver<String>,
    close: Option<Box<dyn FnOnce()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(close) = self.close.take() { close(); }
    }
}

/// 构造并绑定原 TCP/TLS 服务；仅冷路径创建 owner 个原生 Local。
/// 有临时端口选取竞争，绑定失败是环境失败，不是有效的锁序复现。
pub(super) fn start_server(transport: Transport, owners: usize, mode: ServiceMode) -> Server {
    match transport {
        Transport::Tcp => start_typed::<TcpSocket>(owners, mode, None),
        Transport::Tls => {
            let tls = tls_server_config(mode.suppress_tls_tickets);
            start_typed::<TlsSocket>(owners, mode, Some(tls))
        },
    }
}

fn start_typed<S: FixtureSocket>(owners: usize, mode: ServiceMode, tls: Option<TlsConfig>) -> Server {
    assert!((1..=4).contains(&owners));
    let port = TcpListener::bind("127.0.0.1:0").expect("选择真实网络端口失败")
        .local_addr().unwrap().port();
    let (connected_tx, connected) = bounded(256);
    let (errors_tx, errors) = bounded(256);
    let mut factory = PortsAdapterFactory::<S>::new();
    factory.bind(port, Box::new(NetworkService {
        mode, connected: connected_tx, errors: errors_tx, marker: PhantomData,
    }));
    let mut config = match tls {
        Some(tls) => SocketConfig::with_tls("127.0.0.1", &[(port, tls)]),
        None => SocketConfig::new("127.0.0.1", &[port]),
    };
    config.set_option(64 * 1024, 64 * 1024, 64 * 1024, 64);
    let runtimes = (0..owners).map(|_| AsyncRuntimeBuilder::custom_local_thread(
        Some("tcp-hibernate-test-owner"), Some(2 * 1024 * 1024), None, Some(0), Some(Duration::from_millis(1)),
    )).collect::<Vec<_>>();
    let shutdown = runtimes.clone();
    let listener = SocketListener::try_bind(runtimes, factory, config, 16, 2 * 1024 * 1024,
        256, 64, 4096, 4096, Some(1)).expect("启动真实 TCP/TLS 监听器失败");
    Server { port, connected, errors, close: Some(Box::new(move || {
        listener.close(Ok(()));
        for runtime in shutdown { let _ = runtime.close(); }
    })) }
}

fn certificates(pem: &[u8]) -> Vec<rustls::Certificate> {
    rustls_pemfile::certs(&mut BufReader::new(pem)).expect("解析测试证书失败")
        .into_iter().map(rustls::Certificate).collect()
}

fn tls_server_config(suppress_tickets: bool) -> TlsConfig {
    let chain = certificates(include_bytes!("../fixtures/hibernate-server.pem"));
    let keys = rustls_pemfile::pkcs8_private_keys(&mut BufReader::new(
        include_bytes!("../fixtures/hibernate-server.key").as_slice())).expect("解析测试私钥失败");
    let mut config = rustls::ServerConfig::builder().with_safe_defaults().with_no_client_auth()
        .with_single_cert(chain, rustls::PrivateKey(keys.into_iter().next().expect("缺少测试私钥")))
        .expect("构造测试 TLS 配置失败");
    if suppress_tickets {
        // 仅基准选择不恢复会话，避免握手后的票据与一字节回包竞争原库的两字节
        // 发送上限。不补发数据、不改原 TLS 实现，证书与密码套件验证仍然启用。
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    }
    TlsConfig::Server(Arc::new(config))
}

/// 真实客户端；超时保护单次网络读写，外层进程截止仍是最终资源保护。
pub(super) enum Client {
    Tcp(TcpStream),
    Tls(rustls::StreamOwned<rustls::ClientConnection, TcpStream>),
}

impl Client {
    /// 连接真实监听器，TLS 验证测试 CA 和名称；不使用不验证证书的模式。
    pub(super) fn connect(transport: Transport, port: u16) -> Self {
        let stream = TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_secs(3)).expect("真实客户端连接失败");
        stream.set_nodelay(true).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        match transport {
            Transport::Tcp => Self::Tcp(stream),
            Transport::Tls => {
                let mut roots = rustls::RootCertStore::empty();
                for cert in certificates(include_bytes!("../fixtures/hibernate-ca.pem")) {
                    roots.add(&cert).expect("信任测试 CA 失败");
                }
                let config = rustls::ClientConfig::builder().with_safe_defaults()
                    .with_root_certificates(roots).with_no_client_auth();
                let mut connection = rustls::ClientConnection::new(Arc::new(config),
                    "localhost".try_into().unwrap()).expect("构造真实 TLS 客户端失败");
                let mut stream = stream;
                while connection.is_handshaking() {
                    connection.complete_io(&mut stream).expect("真实 TLS 握手或身份验证失败");
                }
                Self::Tls(rustls::StreamOwned::new(connection, stream))
            }
        }
    }

    /// 发送一个字节并读取完全相同的真实回包；只在完成后计入成功次数。
    pub(super) fn round_trip(&mut self, byte: u8) {
        fn exchange(stream: &mut (impl Read + Write), byte: u8) {
            stream.write_all(&[byte]).expect("发送真实网络请求失败");
            stream.flush().expect("刷新真实网络请求失败");
            let mut reply = [0];
            stream.read_exact(&mut reply).expect("等待休眠唤醒后的真实回包失败");
            assert_eq!(reply[0], byte, "回包不得丢失或跨连接混淆");
        }
        match self { Self::Tcp(stream) => exchange(stream, byte), Self::Tls(stream) => exchange(stream, byte) }
    }
}

/// 外部唤醒的单次实测；started 是外部线程首次调用时点，不代表客户端收到时点。
pub(super) struct WakeSample {
    pub started: Instant,
    pub nanos: u64,
    pub retries: u64,
}

/// 只有测试驱动持有停止责任；所有线程均可由最外层进程组截止统一结束。
pub(super) struct WakeWorkers {
    pub sender: Sender<WakeTarget>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Vec<WakeSample>>>,
}

impl WakeWorkers {
    /// 创建固定数量外部 OS 线程；gate 仅用于一次确定性交错，基准必须为 None。
    /// 重试有两秒截止，但不能抢占卡在生产同步 lock 中的一次调用，故仍需进程监控。
    pub(super) fn start(count: usize, gate: Option<Receiver<()>>) -> Self {
        assert!((1..=8).contains(&count));
        let (sender, receiver) = bounded::<WakeTarget>(256);
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..count).map(|index| {
            let receiver = receiver.clone();
            let stop = stop.clone();
            let mut gate = gate.clone();
            thread::Builder::new().name(format!("hibernate-external-{index}")).spawn(move || {
                let mut samples = Vec::with_capacity(4096);
                while !stop.load(Ordering::Acquire) {
                    let target = match receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(target) => target,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    };
                    if let Some(gate) = gate.take() {
                        gate.recv_timeout(Duration::from_secs(2)).expect("等待 poll 持有 W 的探针失败");
                    }
                    let started = Instant::now();
                    let mut retries = 0;
                    while !target.wakeup() {
                        retries += 1;
                        assert!(started.elapsed() < Duration::from_secs(2), "外部唤醒重试未能推进");
                        thread::yield_now();
                    }
                    samples.push(WakeSample { started, retries,
                        nanos: started.elapsed().as_nanos().min(u64::MAX as u128) as u64 });
                }
                samples
            }).expect("创建外部唤醒线程失败")
        }).collect();
        Self { sender, stop, threads }
    }

    /// 客户端均完成后停止并回收所有外部线程；任何线程异常均使测试失败。
    pub(super) fn finish(self) -> Vec<WakeSample> {
        self.stop.store(true, Ordering::Release);
        self.threads.into_iter().flat_map(|thread| thread.join().expect("外部唤醒线程异常")).collect()
    }
}
