//! TCP 连接池启动专项测试。
//!
//! 本测试文件只覆盖 `pi_tcp::connect_pool::TcpSocketPool::run` 的首个事件循环投递语义。
//! 生产侧修复点是：`run` 作为外部启动入口可能从非 `LocalTaskRuntime` 所属线程调用，
//! 因此首个连接池事件循环必须通过 `LocalTaskRuntime::send` 进入线程安全外部队列并唤醒
//! 带轮询器的本地运行时；进入所属线程后的 `event_loop` 内部自调度仍可使用
//! `LocalTaskRuntime::spawn`。
//!
//! 覆盖范围：
//! - 真实 TCP 监听器、接收器、连接池事件循环和 `TcpSocket` 建连路径。
//! - `SocketListener::try_bind` 间接覆盖 `TcpSocketPool::run` 的非所属线程启动路径。
//! - 带轮询器且无超时的 `custom_local_thread`。若首个投递误用 `spawn`，运行时睡眠后不会
//!   被唤醒，本测试会在等待 `connected` 回调时超时失败。
//!
//! 非目标：
//! - 不覆盖 TLS、读写回显、关闭回调完整性或长时间人工观察场景。
//! - 不执行 `crates/tcp/tests/test.rs` 中的人工观察测试。
//!
//! 环境依赖：
//! - 本测试会在 `127.0.0.1` 上打开一个真实 TCP 端口，并使用标准库 `TcpStream` 真实连接。
//! - 端口通过临时绑定 `127.0.0.1:0` 选择；若测试机端口竞争极端严重，可能需要重跑。

use std::{
    io::Result,
    net::{TcpListener as StdTcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{bounded, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;

use pi_tcp::{
    connect::TcpSocket,
    server::{PortsAdapterFactory, SocketListener},
    AsyncService, Socket, SocketConfig, SocketHandle, SocketStatus,
};

/// 真实 TCP 启动探针服务。
///
/// 被测生产入口：
/// - `SocketListener::try_bind`
/// - `TcpSocketPool::run`
/// - `PortsAdapter::connected`
///
/// 行为：
/// - 只在连接成功回调中发送一次通知。
/// - 不读取、不写入业务数据，避免把专项测试扩展到读写状态机。
///
/// 线程/异步安全：
/// - `Sender<()>` 可跨线程克隆和发送。
/// - 处理函数返回的异步任务不阻塞操作系统线程，不持有连接内部借用跨 `.await`。
struct StartupProbeService {
    connected: Sender<()>,
}

impl<S: Socket> AsyncService<S> for StartupProbeService {
    fn handle_connected(
        &self,
        _handle: SocketHandle<S>,
        status: SocketStatus,
    ) -> LocalBoxFuture<'static, ()> {
        let connected = self.connected.clone();
        async move {
            if matches!(status, SocketStatus::Connected(Ok(()))) {
                let _ = connected.send(());
            }
        }
        .boxed_local()
    }

    fn handle_readed(
        &self,
        _handle: SocketHandle<S>,
        _status: SocketStatus,
    ) -> LocalBoxFuture<'static, ()> {
        async move {}.boxed_local()
    }

    fn handle_writed(
        &self,
        _handle: SocketHandle<S>,
        _status: SocketStatus,
    ) -> LocalBoxFuture<'static, ()> {
        async move {}.boxed_local()
    }

    fn handle_closed(
        &self,
        _handle: SocketHandle<S>,
        _status: SocketStatus,
    ) -> LocalBoxFuture<'static, ()> {
        async move {}.boxed_local()
    }

    fn handle_timeouted(
        &self,
        _handle: SocketHandle<S>,
        _status: SocketStatus,
    ) -> LocalBoxFuture<'static, ()> {
        async move {}.boxed_local()
    }
}

/// 选择一个当前可绑定的本机 TCP 端口。
///
/// 返回值是快照，不持有端口；调用者必须尽快绑定。时间复杂度 `O(1)`，有短暂端口竞争风险。
fn reserve_local_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("绑定本机临时 TCP 端口失败")
        .local_addr()
        .expect("读取本机临时 TCP 端口失败")
        .port()
}

/// 带重试地连接本机 TCP 端口。
///
/// 该辅助函数只用于等待接收器线程完成启动，不隐藏连接池启动失败：连接成功后仍必须等待
/// `connected` 回调，否则测试失败。
fn connect_with_retry(port: u16) -> Result<TcpStream> {
    let addr = ("127.0.0.1", port);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
                if error.kind() == std::io::ErrorKind::ConnectionRefused {
                    continue;
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// 测试 `TcpSocketPool::run` 的首个事件循环可从非所属线程安全启动。
///
/// 被测生产语义：
/// - `SocketListener::try_bind` 在调用线程中启动连接池。
/// - 连接池的首个事件循环必须使用 `LocalTaskRuntime::send`，以便唤醒已经睡眠的
///   `custom_local_thread`。
/// - 真实 TCP 连接到达后，连接池必须处理接收器路由过来的连接并触发 `connected` 回调。
///
/// 失败判据：
/// - 如果首个事件循环仍使用 `spawn`，本测试所用运行时会在无任务时进入无超时睡眠，
///   `connected` 回调不会触发，`recv_timeout` 会失败。
#[test]
fn test_tcp_socket_pool_initial_dispatch_wakes_polling_runtime() {
    let _ = env_logger::builder().is_test(true).try_init();

    let rt = AsyncRuntimeBuilder::custom_local_thread(
        Some("tcp-pool-initial-dispatch"),
        None,
        None,
        Some(0),
        None,
    );
    thread::sleep(Duration::from_millis(50));

    let port = reserve_local_port();
    let (connected_tx, connected_rx) = bounded(1);
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(
        port,
        Box::new(StartupProbeService {
            connected: connected_tx,
        }),
    );

    let mut config = SocketConfig::new("127.0.0.1", &[port]);
    config.set_option(16 * 1024, 16 * 1024, 16 * 1024, 16);

    let listener = SocketListener::try_bind(
        vec![rt],
        factory,
        config,
        16,
        2 * 1024 * 1024,
        64,
        16,
        4096,
        4096,
        Some(1_000),
    )
    .expect("为首个事件循环投递专项测试绑定 TCP 监听器失败");

    let _client = connect_with_retry(port).expect("连接测试用 TCP 监听器失败");
    connected_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("TCP 连接池首个事件循环必须处理已接收连接");

    listener.close(Ok(()));
}
