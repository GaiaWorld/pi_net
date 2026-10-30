//! 实际连接（connection）的两条固定处理顺序（sequence）。
//!
//! 每条顺序（sequence）在一个独立进程（process）中运行，连接（connection）与数据操作
//! 均通过公开接口（API）完成。主进程（parent process）只读取标量结果（scalar result）。
//! 对应主线记录第40节；使用当前取得接口（API），对照原件单独归档。

use std::fs::{self, OpenOptions};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use pi_tcp::{AsyncService, SocketConfig, SocketHandle, SocketStatus};
use pi_tcp::connect::TcpSocket;
use pi_tcp::server::{PortsAdapterFactory, SocketListener};

const MODE_ENV: &str = "PI_NET_SEQUENCE_MODE";

/// 值（value）归还时记录次数（count）及所在的线程（thread）。
struct Item {
    number: u32,
    owner: ThreadId,
    count: Arc<AtomicUsize>,
    same_thread: Arc<AtomicBool>,
}

impl Drop for Item {
    fn drop(&mut self) {
        if thread::current().id() != self.owner {
            self.same_thread.store(false, Ordering::SeqCst);
        }
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

/// 传回主线程（main thread）的结果（result）不持有业务对象（object）。
#[derive(Debug)]
struct Report {
    blocked: bool,
    before: usize,
    after: usize,
    number: u32,
    empty: bool,
    same_thread: bool,
}

/// 一条连接（connection）在同一服务线程（service thread）内完成全部数据步骤（step）。
fn evaluate(
    handle: &SocketHandle<TcpSocket>,
    held: bool,
    count: Arc<AtomicUsize>,
    same_thread: Arc<AtomicBool>,
) -> Report {
    let holder = handle.get_context().expect("context must be present");
    let context = holder.as_ref();

    assert!(context.is_empty(), "the initial context must be empty");
    assert!(context.set(Item {
        number: 73,
        owner: thread::current().id(),
        count: count.clone(),
        same_thread: same_thread.clone(),
    }));
    let before = count.load(Ordering::SeqCst);
    let (blocked, item) = if held {
        let first = context.get::<Item>().expect("the first value must be present");
        let second = context.get::<Item>().expect("the second value must be present");
        match context.remove::<Item>() {
            Err(_) => {
                drop(first);
                drop(second);
                (true, context.remove::<Item>().unwrap().unwrap())
            },
            Ok(item) => {
                std::mem::forget(first);
                std::mem::forget(second);
                (false, item.expect("the value must be present"))
            },
        }
    } else {
        (true, context.remove::<Item>().unwrap().unwrap())
    };
    let number = item.number;
    drop(item);
    Report {
        blocked,
        before,
        after: count.load(Ordering::SeqCst),
        number,
        empty: context.is_empty(),
        same_thread: same_thread.load(Ordering::SeqCst),
    }
}

/// 服务（service）只将结果（result）送回主线程（main thread），不保存业务句柄（context handle）。
struct Service {
    held: bool,
    count: Arc<AtomicUsize>,
    same_thread: Arc<AtomicBool>,
    report: Sender<Report>,
    finished: Sender<()>,
}

impl AsyncService<TcpSocket> for Service {
    /// 收到连接（connection）事件（event）后执行固定顺序（sequence）。
    fn handle_connected(&self, handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let held = self.held;
        let count = self.count.clone();
        let same_thread = self.same_thread.clone();
        let report = self.report.clone();
        async move {
            assert!(matches!(status, SocketStatus::Connected(Ok(()))));
            let result = evaluate(&handle, held, count, same_thread);
            report.send(result).expect("the result receiver must remain available");
            handle.close(Ok(())).expect("the connection request must be accepted");
        }.boxed_local()
    }

    /// 本顺序（sequence）没有应用数据（application data）输入。
    fn handle_readed(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }

    /// 本顺序（sequence）没有应用数据（application data）输出。
    fn handle_writed(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }

    /// 记录该连接（connection）的最后一个服务事件（service event）。
    fn handle_closed(&self, _handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let finished = self.finished.clone();
        async move {
            assert!(matches!(status, SocketStatus::Closed(Ok(()))));
            finished.send(()).expect("the event receiver must remain available");
        }.boxed_local()
    }

    /// 本顺序（sequence）不设置定时事件（timer event）。
    fn handle_timeouted(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
}

/// 子进程（child process）只运行由主进程（parent process）选定的一条顺序（sequence）。
#[test]
#[ignore]
fn test_sequence_child() {
    let mode = std::env::var(MODE_ENV).expect("a sequence must be selected");
    assert!(mode == "plain" || mode == "held");
    let held = mode == "held";
    let port = {
        let reserved = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        reserved.local_addr().unwrap().port()
    };
    let (report_tx, report_rx) = bounded(1);
    let (finished_tx, finished_rx) = bounded(1);
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port, Box::new(Service {
        held,
        count: Arc::new(AtomicUsize::new(0)),
        same_thread: Arc::new(AtomicBool::new(true)),
        report: report_tx,
        finished: finished_tx,
    }));
    let runtime = AsyncRuntimeBuilder::<()>::default_local_thread(Some("SequenceOwner"), None);
    let start = Instant::now();
    while !runtime.is_running() {
        assert!(start.elapsed() < Duration::from_secs(2), "the owner must become available");
        thread::yield_now();
    }
    let listener = SocketListener::try_bind(
        vec![runtime.clone()], factory, SocketConfig::new("127.0.0.1", &[port]),
        8, 2 * 1024 * 1024, 16, 1, 4096, 4096, Some(10),
    ).unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let client = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    let report = report_rx.recv_timeout(Duration::from_secs(3)).expect("the result must arrive");
    finished_rx.recv_timeout(Duration::from_secs(3)).expect("the service event must arrive");
    let (fence_tx, fence_rx) = bounded(1);
    runtime.send(async move { fence_tx.send(()).unwrap(); });
    fence_rx.recv_timeout(Duration::from_secs(3)).expect("the next owner step must complete");
    drop(client);
    listener.close(Ok(()));
    println!(
        "SEQUENCE mode={} blocked={} before={} after={} number={} empty={} same_thread={}",
        mode, report.blocked, report.before, report.after, report.number, report.empty, report.same_thread,
    );
    assert_eq!(report.before, 0);
    assert_eq!(report.number, 73);
    assert_eq!(report.after, 1);
    assert!(report.empty);
    assert!(report.same_thread);
    assert!(report.blocked, "the value must remain while ordinary handles are present");
}

/// 每条顺序（sequence）使用独立进程（process）和20秒截止（deadline），结束后读取结果（result）。
fn run_child(mode: &str) {
    let path = std::env::temp_dir().join(format!("pi-net-sequence-{}-{}.txt", std::process::id(), mode));
    let output = OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "test_sequence_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env(MODE_ENV, mode).env("RUST_BACKTRACE", "0")
        .stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    let begin = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() { break status; }
        if begin.elapsed() >= Duration::from_secs(20) {
            child.kill().unwrap();
            let _ = child.wait();
            let text = fs::read_to_string(&path).unwrap();
            fs::remove_file(&path).unwrap();
            panic!("the sequence exceeded its time limit\n{text}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let text = fs::read_to_string(&path).unwrap();
    fs::remove_file(&path).unwrap();
    println!("{text}");
    assert!(status.success(), "the selected sequence must complete: {status}");
}

/// 第一条顺序（sequence）在没有普通句柄（context handle）时移出值（value）。
#[test]
fn test_a_plain_sequence() {
    run_child("plain");
}

/// 第二条顺序（sequence）先取得两份普通句柄（context handle），再按固定顺序（sequence）归还。
#[test]
fn test_b_held_sequence() {
    run_child("held");
}
