//! Hibernate 锁序真实网络专项，关联 utils::Hibernate::poll/wakeup 和 TCP/TLS wakeup。
//!
//! 两个仅测试阶段探针不写生产状态，不暴露锁，也不为生产补唤醒。第一轮正常登记后
//! 强制再次 poll，精确控制 W 已持有与外部线程尝试 W 的交错。相同产品断言用于
//! 修复前失败和修复后通过。每种传输先跑不重叠的负面控制，再跑重叠场景。
//! 所有可能停留在同步锁内的动作都在独立进程组；父进程截止并清理，不永久等待。

use crate as tcp;

#[path = "../tests/support/hibernate_network.rs"]
mod network;
#[path = "../tests/support/hibernate_process.rs"]
mod process;

#[path = "hibernate_unit.rs"]
mod unit;

use std::io::Write;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicUsize, Ordering}};
use std::time::Duration;
use crossbeam_channel::{bounded, Receiver, Sender};
use network::{Client, ServiceMode, Transport, WakeWorkers};
use super::Hibernate;
use crate::Socket;

const CHILD_ENV: &str = "PI_TCP_HIB_REPRO_CHILD";
static PROBE: OnceLock<Mutex<Option<Arc<Probe>>>> = OnceLock::new();

/// 单连接测试控制，只存公开 UID 和阶段信号，不保存生产对象或内部地址。
struct Probe {
    uid: usize,
    polls: AtomicUsize,
    poll_held: Sender<()>,
    wake_attempt: Sender<()>,
    wait_for_wake: Receiver<()>,
}

/// 仅匹配当前独立进程内选定连接，释放测试表锁后才允许等待阶段。
fn current_probe<S: Socket>(hibernate: &Hibernate<S>) -> Option<Arc<Probe>> {
    PROBE.get()?.lock().expect("测试探针表异常").as_ref()
        .filter(|probe| probe.uid == hibernate.0.handle.get_uid()).cloned()
}

/// 探针一：真实 poll 已持有 W、尚未取得 H。只在第二次 poll 控制一次交错。
/// 有两秒阶段截止；本函数不出现在正常库/基准产物，不构成生产阻塞。
pub(super) fn after_poll_waker_lock<S: Socket>(hibernate: &Hibernate<S>) {
    if let Some(probe) = current_probe(hibernate) {
        if probe.polls.fetch_add(1, Ordering::Relaxed) == 1 {
            println!("TCP-HIB-REPRO 阶段=poll持有W uid={}", probe.uid);
            std::io::stdout().flush().unwrap();
            probe.poll_held.send(()).unwrap();
            probe.wait_for_wake.recv_timeout(Duration::from_secs(2))
                .expect("外部线程未到达尝试 W 的阶段");
        }
    }
}

/// 探针二：外部真实 wakeup 即将尝试 W；不取得或修改 H/W，仅通知第一阶段继续。
pub(super) fn before_wakeup_waker_lock<S: Socket>(hibernate: &Hibernate<S>) {
    if let Some(probe) = current_probe(hibernate) {
        if probe.polls.load(Ordering::Relaxed) >= 2 {
            println!("TCP-HIB-REPRO 阶段=wakeup尝试W uid={}", probe.uid);
            std::io::stdout().flush().unwrap();
            let _ = probe.wake_attempt.try_send(());
        }
    }
}

/// 独立子进程的真实网络主体；两次闭环同时保护恢复及后续重新休眠。
fn child_case(transport: Transport, overlap: bool) {
    let (held_tx, held_rx) = bounded(1);
    let (wake_tx, wake_rx) = bounded(1);
    let workers = WakeWorkers::start(1, overlap.then_some(held_rx));
    let server = network::start_server(transport, 1, ServiceMode {
        jobs: Some(workers.sender.clone()), force_repoll: overlap, suppress_tls_tickets: false,
    });
    let mut client = Client::connect(transport, server.port);
    let target = server.connected.recv_timeout(Duration::from_secs(3)).expect("未收到真实连接回调");
    if overlap {
        *PROBE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(Arc::new(Probe {
            uid: target.uid(), polls: AtomicUsize::new(0), poll_held: held_tx,
            wake_attempt: wake_tx, wait_for_wake: wake_rx,
        }));
    }
    println!("TCP-HIB-REPRO 阶段=真实连接已建立 传输={transport:?} uid={}", target.uid());
    std::io::stdout().flush().unwrap();
    client.round_trip(37);
    // 只控制首个交错；后续恢复不得依赖继续保留测试探针。
    if let Some(probe) = PROBE.get() { *probe.lock().unwrap() = None; }
    client.round_trip(93);
    let samples = workers.finish();
    assert_eq!(samples.len(), 2, "两次请求必须各完成一次真实外部唤醒");
    assert!(server.errors.try_iter().collect::<Vec<_>>().is_empty(), "服务端不能隐藏错误");
    assert!(samples.iter().all(|sample| sample.nanos > 0 && sample.started <= std::time::Instant::now()));
    println!("TCP-HIB-REPRO 阶段=两次真实回包已完成 传输={transport:?} 重试={}",
        samples.iter().map(|sample| sample.retries).sum::<u64>());
}

/// 同一断言执行控制与交错；截止是失败，不将旧版等待当作期望通过。
/// 前置阶段/CPU证据用于区分产品停推与环境失败，原件保留且进程组必须清空。
fn run_case(transport: Transport, name: &str) {
    if let Ok(role) = std::env::var(CHILD_ENV) {
        child_case(transport, role == "overlap");
        return;
    }
    for role in ["control", "overlap"] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, role);
        let result = process::run_child(&mut command, Duration::from_secs(8),
            &format!("{transport:?}-{role}"));
        println!("隔离场景={transport:?}/{role} 超时={} 阶段后停推={} CPU_tick={} 原件={}",
            result.timed_out, result.stalled_after_probes, result.cpu_ticks, result.artifacts.display());
        if result.timed_out {
            assert!(result.stdout.contains("阶段=真实连接已建立"), "超时发生在网络建立之前\n{}", result.stderr);
            assert!(result.stdout.contains("阶段=poll持有W") && result.stdout.contains("阶段=wakeup尝试W"),
                "未到达两阶段，不能作为锁序复现\n{}\n{}", result.stdout, result.stderr);
            assert!(result.cpu_ticks > 0, "缺少等待阶段 CPU 活动证据");
        }
        assert!(!result.timed_out && result.status.success(),
            "真实休眠/外部唤醒/再次休眠必须有界完成；子进程已清理\n{}\n{}",
            result.stdout, result.stderr);
    }
}

/// TCP 生产专项：先正常对照，再按已批准阶段稳定交错；修复前后断言完全相同。
#[test]
fn test_hibernate_lock_order_tcp_real_network() {
    run_case(Transport::Tcp, "utils::hibernate_regression::test_hibernate_lock_order_tcp_real_network");
}

/// TLS 生产专项：使用真实握手/证书校验和加密回包，不用 TCP 结果替代 TLS 证据。
#[test]
fn test_hibernate_lock_order_tls_real_network() {
    run_case(Transport::Tls, "utils::hibernate_regression::test_hibernate_lock_order_tls_real_network");
}

/// 测试监控自己的单元：真实TCP回包后分别正常退出/保持进程等待截止，核对清理。
/// 两秒截止是被测工具的预期行为，不代表生产Hibernate成功；原专项仍禁止以截止
/// 冒充成功。不改变原交错断言或基准负载，也不添加生产状态/探针。
#[test]
fn test_hibernate_process_monitor_owns_identity() {
    const MONITOR_ENV: &str = "PI_TCP_HIB_MONITOR_CHILD";
    const TEST: &str = "utils::hibernate_regression::test_hibernate_process_monitor_owns_identity";
    if let Ok(role) = std::env::var(MONITOR_ENV) {
        assert!(role == "normal" || role == "deadline");
        let server = network::start_server(Transport::Tcp, 1, ServiceMode {
            jobs: None, force_repoll: false, suppress_tls_tickets: false,
        });
        let mut client = Client::connect(Transport::Tcp, server.port);
        let _target = server.connected.recv_timeout(Duration::from_secs(2)).unwrap();
        client.round_trip(17);
        assert!(server.errors.try_iter().collect::<Vec<_>>().is_empty());
        println!("TCP-HIB-MONITOR 阶段=真实回包完成 角色={role}");
        std::io::stdout().flush().unwrap();
        if role == "deadline" { std::thread::sleep(Duration::from_secs(10)); }
        return;
    }
    for role in ["normal", "deadline"] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([TEST, "--exact", "--nocapture", "--test-threads=1"]).env(MONITOR_ENV, role);
        let deadline = Duration::from_secs(if role == "normal" { 8 } else { 2 });
        let result = process::run_child(&mut command, deadline, &format!("monitor-{role}"));
        assert!(result.stdout.contains("TCP-HIB-MONITOR 阶段=真实回包完成"),
            "清理用例必须先完成真实网络装配\n{}\n{}", result.stdout, result.stderr);
        assert_eq!(result.timed_out, role == "deadline");
        assert_eq!(result.status.success(), role == "normal");
        assert!(!result.stalled_after_probes, "监控单元不得冒充生产交错复现");
        assert!(std::fs::read_to_string(result.artifacts.join("monitor.txt")).unwrap().contains("remaining=0"));
    }
}
