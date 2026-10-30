//! 原关闭批次（close batch）的两条实际连接（connection）顺序，对应D-LOCAL-04-BATCH。
//! 同一次所属线程（owner thread）投递中同步提交固定事件（event），客户端（client）保持打开。
//! 子进程（subprocess）有20秒截止（deadline），回调（callback）观察有3秒截止（deadline）。

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use pi_tcp::{AsyncService, SocketConfig, SocketHandle, SocketStatus};
use pi_tcp::connect::TcpSocket;
use pi_tcp::server::{PortsAdapterFactory, SocketListener};

struct Service {
    connected: Sender<SocketHandle<TcpSocket>>,
    closed: Sender<mio::Token>,
}

impl AsyncService<TcpSocket> for Service {
    /// 只保存控制句柄（SocketHandle），业务数据（application data）不参与本顺序（sequence）。
    fn handle_connected(&self, handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let connected = self.connected.clone();
        async move {
            assert!(matches!(status, SocketStatus::Connected(Ok(()))));
            connected.send(handle).unwrap();
        }.boxed_local()
    }
    fn handle_readed(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
    fn handle_writed(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
    /// 记录完整令牌（full token）及唯一完成次数（completion count），不代替生产清理（cleanup）。
    fn handle_closed(&self, handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let closed = self.closed.clone();
        async move {
            assert!(matches!(status, SocketStatus::Closed(Ok(()))));
            closed.send(*handle.get_token()).unwrap();
        }.boxed_local()
    }
    fn handle_timeouted(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
}

/// 第一连接（connection）处理完成后再建立第二连接（connection），可观察完整令牌（full token）隔离。
#[test]
#[ignore]
fn test_close_sequence_child() {
    let mode = std::env::var("PI_NET_CLOSE_SEQUENCE").unwrap();
    assert!(mode == "batch" || mode == "late");
    let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let (connected_tx, connected_rx) = bounded(4);
    let (closed_tx, closed_rx) = bounded(4);
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port, Box::new(Service { connected: connected_tx, closed: closed_tx }));
    let runtime = AsyncRuntimeBuilder::<()>::default_local_thread(Some("CloseSequenceOwner"), None);
    let started = Instant::now();
    while !runtime.is_running() {
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::yield_now();
    }
    let listener = SocketListener::try_bind(vec![runtime.clone()], factory,
        SocketConfig::new("127.0.0.1", &[port]), 1, 2 * 1024 * 1024, 16, 1, 4096, 4096, Some(10)).unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let first_client = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    let first = connected_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let first_token = *first.get_token();
    if mode == "late" {
        let closing = first.clone();
        runtime.send(async move { closing.close(Ok(())).unwrap(); });
        assert_eq!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap(), first_token);
        let (fence_tx, fence_rx) = bounded(1);
        runtime.send(async move { fence_tx.send(()).unwrap(); });
        fence_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    let second_client = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    let second = connected_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let second_token = *second.get_token();
    assert_ne!(first_token, second_token);
    let (submitted_tx, submitted_rx) = bounded(1);
    let batch = mode == "batch";
    runtime.send(async move {
        // 一次同步步骤（synchronous step）完成全部提交；没有等待（await）或其它调度（scheduling）。
        first.close(Ok(())).unwrap();
        if batch { first.close(Ok(())).unwrap(); }
        second.close(Ok(())).unwrap();
        submitted_tx.send(()).unwrap();
    });
    submitted_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    if batch {
        assert_eq!(closed_rx.recv_timeout(Duration::from_secs(3)).unwrap(), first_token);
    }
    assert_eq!(closed_rx.recv_timeout(Duration::from_secs(3))
        .expect("the second connection must complete in this sequence"), second_token);
    let (fence_tx, fence_rx) = bounded(1);
    runtime.send(async move { fence_tx.send(()).unwrap(); });
    fence_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(closed_rx.try_recv().is_err());
    assert!(connected_rx.try_recv().is_err());
    drop(first_client);
    drop(second_client);
    listener.close(Ok(()));
}

/// 主入口（parent entry）按固定场景（scenario）启动一个进程（process），结束后归还观察文件（observation file）。
fn run_child(mode: &str) {
    let path = std::env::temp_dir().join(format!("pi-net-close-sequence-{}-{mode}.txt", std::process::id()));
    let output = std::fs::OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "test_close_sequence_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PI_NET_CLOSE_SEQUENCE", mode).env("RUST_BACKTRACE", "0")
        .stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() { break status; }
        if started.elapsed() >= Duration::from_secs(20) {
            child.kill().unwrap();
            child.wait().unwrap();
            let output = std::fs::read_to_string(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            panic!("close sequence exceeded deadline: {mode}\n{output}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    println!("{mode}\n{output}");
    assert!(status.success(), "close sequence mode={mode} status={status}");
}

/// 同批A/A/B，A只完成一次，B仍完成一次。
#[test]
fn test_duplicate_then_other_connection() { run_child("batch"); }

/// A已移出后，旧完整令牌（full token）事件（event）不影响新B的关闭。
#[test]
fn test_previous_token_then_new_connection() { run_child("late"); }
