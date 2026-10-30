//! B-CTX-P01的真实网络夹具（test fixture），由benches/context_lifetime.rs独立驱动。
//!
//! 被测入口为SocketListener::try_bind、SocketHandle::get_context/write_ready，以及
//! utils::SocketContext的set/get/remove和ContextHandle的短读取/短写入。
//! 按主线第43节仅适配已接受接口（API），不改负载（workload）、次数、计时或断言（assertion）。
//! 业务对象和接收缓冲区只在所属线程（owner thread）访问，所有借用在等待前结束。
//! 采样数据（sample）保存在所属线程（owner thread），全部真实回应结束后才传回。
//! 不主动关闭运行时（runtime），不为生产关闭补功能；进程（process）截止由父入口负责。
//! 网络等待有3秒超时，全部异常直接失败；TLS校验测试CA/localhost且不恢复会话。
//! 合同见docs/TCP_CONTEXT_BASELINE_PLAN_20261028.md；前测原件（baseline artifact）保持。

use super::tcp;
use std::cell::RefCell;
use std::hint::black_box;
use std::io::{BufReader, Read, Write};
use std::marker::PhantomData;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};
use bytes::Buf;
use crossbeam_channel::{bounded, Receiver, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use tcp::{AsyncService, Socket, Stream, SocketConfig, SocketHandle, SocketStatus};
use tcp::connect::TcpSocket;
use tcp::tls_connect::TlsSocket;
use tcp::server::{PortsAdapterFactory, SocketListener};
use tcp::utils::TlsConfig;

/// 两种传输（transport）；只负责选择原生产实现，不增加协议（protocol）代理。
#[derive(Clone, Copy, Debug)]
pub(super) enum Transport { Tcp, Tls }

/// 固定负载（workload）；不能通过外部整数扩展为无界循环。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Workload { Plain, Access, Replace }

impl Workload {
    /// 返回稳定结果标识（identifier），纯函数（pure function），O(1)，无分配。
    pub fn name(self) -> &'static str {
        match self { Self::Plain => "plain", Self::Access => "context_access", Self::Replace => "context_replace" }
    }
}

/// 同进程（process）共用的测量窗口（window），只在启动客户端（client）前发布一次。
#[derive(Clone, Copy)]
pub(super) struct Window { pub begin: Instant, pub end: Instant }

/// 固定16字节业务值；不持有句柄（handle）或其它资源，析构（drop）无外部副作用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Value { sequence: u64, sum: u64 }

/// 单个所属线程（owner thread）的原始批次（batch）采样；跨线程只移动这些普通数据。
pub(super) struct OwnerSamples {
    pub owner: ThreadId,
    pub requests: u64,
    pub nanos: Vec<u64>,
}

thread_local! {
    static SAMPLES: RefCell<Option<OwnerSamples>> = const { RefCell::new(None) };
}

/// 一条冷通知（notification）或错误；没有逐消息跨线程上报。
pub(super) enum Event {
    Connected(ThreadId),
    Closed,
    Samples(OwnerSamples),
    Error(String),
}

/// 通知（notification）失败表示夹具（test fixture）容量或生命周期失效，不能丢弃换通过。
/// 输入为一个拥有所有权（ownership）的事件；O(1)通道（channel）提交，不在异步等待中持锁。
fn emit(events: &Sender<Event>, event: Event) {
    events.try_send(event).unwrap_or_else(|_| panic!("基准通知通道已满或失效"));
}

/// 安装当前连接（connection）的真实业务值。只由建连回调（connected callback）调用。
/// 在所属线程（owner thread）取得真实持有，不跨等待（await）或并列访问许可（access permit）。
fn install<S: Socket>(handle: &SocketHandle<S>, value: Value) {
    let context = handle.get_context().expect("benchmark owner context unavailable");
    assert!(context.is_empty(), "初始业务上下文必须为空");
    assert!(context.set(value), "不能用重复安装掩盖初始状态偏差");
}

/// 一轮真实读取/写入，不复制业务对象到替代存储。返回更新后的标量副本。
/// 一个普通句柄（handle）作用域内完成操作；不等待、不移除，O(1)且无深复制。
fn access_once<S: Socket>(handle: &SocketHandle<S>, byte: u8) -> Value {
    let context = handle.get_context().expect("benchmark owner context unavailable");
    let value = context.get::<Value>().expect("基准业务值不存在");
    let before = {
        let view = value.try_read().expect("benchmark read permit unavailable");
        black_box(*view.as_ref())
    };
    let mut view = value.try_write().expect("benchmark write permit unavailable");
    let target = view.as_mut();
    target.sequence = before.sequence.wrapping_add(1);
    target.sum = before.sum.wrapping_add(u64::from(byte) + 1);
    black_box(*target)
}

/// 每请求64轮合法访问，返回依赖实际更新值的单字节回应，O(64)，不跨等待持有借用。
fn access<S: Socket>(handle: &SocketHandle<S>, byte: u8) -> u8 {
    let mut last = Value { sequence: 0, sum: 0 };
    for _ in 0..64 { last = access_once(handle, byte); }
    byte ^ last.sequence as u8 ^ last.sum as u8
}

/// 真实安装/访问/普通移除，O(1)操作及一次生产分配（allocation）。
/// access_once返回前普通句柄（handle）已归还；不触发已知的存活句柄移除偏差。
fn replace<S: Socket>(handle: &SocketHandle<S>, byte: u8) -> u8 {
    install(handle, Value { sequence: 0, sum: u64::from(byte) });
    let updated = access_once(handle, byte);
    let context = handle.get_context().expect("benchmark owner context unavailable");
    let value = context.remove::<Value>().expect("合法普通移除失败").expect("移除不能伪报空值");
    assert_eq!(value, updated, "移出值必须是刚才真实写入的对象");
    assert!(context.is_empty(), "普通移除后容器必须可复用");
    byte ^ value.sequence as u8 ^ value.sum as u8
}

/// 原生产适配器（adapter）；共享字段（field）均可安全跨线程，业务数据留在所属线程。
struct Service<S> {
    workload: Workload,
    events: Sender<Event>,
    window: Arc<OnceLock<Window>>,
    finishing: Arc<AtomicBool>,
    marker: PhantomData<fn() -> S>,
}

impl<S: Socket + Stream> AsyncService<S> for Service<S> {
    /// 准备业务值与本线程采样容器后发布建连完成；失败不伪造可用连接（connection）。
    fn handle_connected(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let events = self.events.clone();
        let workload = self.workload;
        async move {
            assert!(matches!(status, SocketStatus::Connected(Ok(()))), "建连失败: {status:?}");
            assert_ne!(handle.get_uid(), 0, "必须使用真实生产连接身份");
            assert_eq!(std::mem::size_of::<Value>(), 16);
            SAMPLES.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(OwnerSamples { owner: thread::current().id(), requests: 0,
                        nanos: Vec::with_capacity(if workload == Workload::Plain { 0 } else { 4096 }) });
                }
            });
            if workload == Workload::Access { install(&handle, Value { sequence: 0, sum: 0 }); }
            emit(&events, Event::Connected(thread::current().id()));
        }.boxed_local()
    }

    /// 消费一个真实字节后执行负载（workload）并提交回应；所有内部借用先于发送结束。
    /// 正常无同步锁（lock）或异步等待；接收缓冲访问严格限于所属线程（owner thread）。
    fn handle_readed(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let events = self.events.clone();
        let workload = self.workload;
        let window = if workload == Workload::Plain { None } else { self.window.get().copied() };
        let finishing = if matches!(status, SocketStatus::Readed(Err(_))) { Some(self.finishing.clone()) } else { None };
        async move {
            if !matches!(status, SocketStatus::Readed(Ok(_))) {
                if !finishing.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    emit(&events, Event::Error(format!("读取回调失败: {status:?}")));
                }
                return;
            }
            let byte = {
                let buffer = handle.get_read_buffer();
                // 安全边界：原缓冲区只在此所属线程消费；不把Rc或引用交给客户端线程。
                let buffer = unsafe { &mut *buffer.get() };
                buffer.as_mut().and_then(|bytes| {
                    if bytes.is_empty() { None } else { let byte = bytes[0]; bytes.advance(1); Some(byte) }
                })
            };
            let Some(byte) = byte else { return; };
            let started = (workload != Workload::Plain).then(Instant::now);
            let reply = match workload {
                Workload::Plain => byte,
                Workload::Access => access(&handle, byte),
                Workload::Replace => replace(&handle, byte),
            };
            if let Some(started) = started {
                let completed = Instant::now();
                SAMPLES.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    let sample = slot.as_mut().expect("采样必须在建连后执行");
                    assert_eq!(sample.owner, thread::current().id());
                    sample.requests += 1;
                    if window.is_some_and(|w| started >= w.begin && completed <= w.end) {
                        sample.nanos.push(completed.duration_since(started).as_nanos() as u64);
                    }
                });
            }
            if let Err(error) = handle.write_ready(vec![reply]) {
                emit(&events, Event::Error(format!("真实回应提交失败: {error}")));
            }
        }.boxed_local()
    }

    /// 发送错误不能混入成功样本；正常回调（callback）不增加计时或跨线程上报。
    fn handle_writed(&self, _handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        if !matches!(status, SocketStatus::Writed(Ok(_))) {
            emit(&self.events, Event::Error(format!("发送回调失败: {status:?}")));
        }
        async {}.boxed_local()
    }

    /// 客户端全部结束后取回线程内采样，并执行本夹具业务值的普通清理。
    /// 不替生产移除内部句柄（handle），不调用Runtime.close，不把通知当作全部资源已释放。
    fn handle_closed(&self, handle: SocketHandle<S>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let events = self.events.clone();
        let finishing = self.finishing.clone();
        let workload = self.workload;
        async move {
            if !finishing.load(Ordering::Acquire) {
                emit(&events, Event::Error(format!("测量尚未结束就关闭: {status:?}")));
            }
            if workload == Workload::Access {
                let context = handle.get_context().expect("benchmark owner context unavailable");
                assert!(context.remove::<Value>().expect("业务值清理失败").is_some());
            }
            let samples = SAMPLES.with(|slot| slot.borrow_mut().take());
            if let Some(samples) = samples { emit(&events, Event::Samples(samples)); }
            emit(&events, Event::Closed);
        }.boxed_local()
    }

    /// 本负载不设置定时器（timer）；意外到达是夹具（test fixture）或生产链异常。
    fn handle_timeouted(&self, _handle: SocketHandle<S>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        emit(&self.events, Event::Error("本负载不应产生业务超时".into()));
        async {}.boxed_local()
    }
}

/// 一点的真实服务持有者；Drop只沿原监听器（listener）关闭，不补关任何运行时（runtime）。
/// 不跨异步等待持有；最终进程（process）隔离清理不等于生产资源已全部回收。
pub(super) struct Server {
    pub port: u16,
    pub events: Receiver<Event>,
    pub window: Arc<OnceLock<Window>>,
    finishing: Arc<AtomicBool>,
    close: Option<Box<dyn FnOnce()>>,
}

impl Server {
    /// 仅宣告测试请求全部结束，用于区分主动断开和测量期间异常，不修改生产状态。
    pub fn begin_teardown(&self) { self.finishing.store(true, Ordering::Release); }

    /// 只测原close调用返回用时，不是线程退出或网络排空时延；重复调用是夹具错误。
    pub fn close(&mut self) -> u64 {
        let close = self.close.take().expect("基准服务不能重复关闭");
        let started = Instant::now();
        close();
        started.elapsed().as_nanos() as u64
    }
}

impl Drop for Server {
    fn drop(&mut self) { if let Some(close) = self.close.take() { close(); } }
}

/// 按固定传输（transport）冷建1或4个原生Local运行时（runtime），返回可观察服务。
/// 同步且有建连/线程/分配副作用，非幂等；O(owners)，不重试抢占端口失败。
pub(super) fn start_server(transport: Transport, owners: usize, workload: Workload) -> Server {
    assert!([1, 4].contains(&owners));
    match transport {
        Transport::Tcp => start_typed::<TcpSocket>(owners, workload, None),
        Transport::Tls => start_typed::<TlsSocket>(owners, workload, Some(tls_config())),
    }
}

/// 原公开装配（assembly），不改变路由（routing）或强制均衡池分布；O(owners)。
fn start_typed<S: Socket + Stream>(owners: usize, workload: Workload, tls: Option<TlsConfig>) -> Server {
    let port = TcpListener::bind("127.0.0.1:0").expect("选取回环端口失败").local_addr().unwrap().port();
    let (events, receiver) = bounded(512);
    let window = Arc::new(OnceLock::new());
    let finishing = Arc::new(AtomicBool::new(false));
    let mut factory = PortsAdapterFactory::<S>::new();
    factory.bind(port, Box::new(Service { workload, events, window: window.clone(),
        finishing: finishing.clone(), marker: PhantomData }));
    let mut config = match tls {
        Some(tls) => SocketConfig::with_tls("127.0.0.1", &[(port, tls)]),
        None => SocketConfig::new("127.0.0.1", &[port]),
    };
    config.set_option(64 * 1024, 64 * 1024, 64 * 1024, 64);
    let runtimes = (0..owners).map(|_| AsyncRuntimeBuilder::custom_local_thread(
        Some("tcp-context-owner"), Some(2 * 1024 * 1024), None, Some(0), Some(Duration::from_millis(1)),
    )).collect();
    let listener = SocketListener::try_bind(runtimes, factory, config, 16, 2 * 1024 * 1024,
        256, 64, 4096, 4096, Some(1)).expect("启动真实上下文基准服务失败");
    Server { port, events: receiver, window, finishing, close: Some(Box::new(move || listener.close(Ok(())))) }
}

/// 解析既有测试证书（certificate）；只在冷准备分配，非法输入立即失败。
fn certificates(pem: &[u8]) -> Vec<rustls::Certificate> {
    rustls_pemfile::certs(&mut BufReader::new(pem)).expect("解析测试证书失败")
        .into_iter().map(rustls::Certificate).collect()
}

/// 复用既有身份及关闭会话恢复（session resumption）的对照配置，不改生产TLS逻辑。
fn tls_config() -> TlsConfig {
    let chain = certificates(include_bytes!("../fixtures/hibernate-server.pem"));
    let keys = rustls_pemfile::pkcs8_private_keys(&mut BufReader::new(
        include_bytes!("../fixtures/hibernate-server.key").as_slice())).expect("解析测试私钥失败");
    let mut config = rustls::ServerConfig::builder().with_safe_defaults().with_no_client_auth()
        .with_single_cert(chain, rustls::PrivateKey(keys.into_iter().next().expect("缺少测试私钥")))
        .expect("构建测试TLS配置失败");
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    TlsConfig::Server(Arc::new(config))
}

/// 真实客户端（client）；每连接最多一个未完成请求，读写最多等待3秒。
pub(super) enum Client {
    Tcp(TcpStream),
    Tls(rustls::StreamOwned<rustls::ClientConnection, TcpStream>),
}

impl Client {
    /// 打开真实网络；TLS验证CA及localhost，不关闭认证。同步、非幂等且可能阻塞。
    pub fn connect(transport: Transport, port: u16) -> Self {
        let stream = TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_secs(3)).expect("真实连接失败");
        stream.set_nodelay(true).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        match transport {
            Transport::Tcp => Self::Tcp(stream),
            Transport::Tls => {
                let mut roots = rustls::RootCertStore::empty();
                for certificate in certificates(include_bytes!("../fixtures/hibernate-ca.pem")) {
                    roots.add(&certificate).expect("信任测试CA失败");
                }
                let config = rustls::ClientConfig::builder().with_safe_defaults()
                    .with_root_certificates(roots).with_no_client_auth();
                let mut connection = rustls::ClientConnection::new(Arc::new(config),
                    "localhost".try_into().unwrap()).expect("创建TLS客户端失败");
                let mut stream = stream;
                while connection.is_handshaking() {
                    connection.complete_io(&mut stream).expect("TLS握手或身份校验失败");
                }
                Self::Tls(rustls::StreamOwned::new(connection, stream))
            }
        }
    }

    /// 完成一个真实往返（round trip）并独立核验期望值；只有正确回应才能计成功。
    /// 输入/输出固定一个字节，O(1)，同步网络等待；不可并发使用同一个客户端（client）。
    pub fn exchange(&mut self, byte: u8, expected: u8) {
        fn exchange(stream: &mut (impl Read + Write), byte: u8, expected: u8) {
            stream.write_all(&[byte]).expect("请求写入失败");
            stream.flush().expect("请求刷新失败");
            let mut reply = [0];
            stream.read_exact(&mut reply).expect("真实回应读取失败");
            assert_eq!(reply[0], expected, "真实回应与独立计算值不符");
        }
        match self { Self::Tcp(stream) => exchange(stream, byte, expected), Self::Tls(stream) => exchange(stream, byte, expected) }
    }
}
