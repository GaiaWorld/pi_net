//! WQM-B01真实TCP/官方SDK专项，不是单元模拟或运行时采集器的重复测试。
//! 验证connect_pool::run/handle_timer/handle_write_event真实装配、15秒后Gauge、
//! 同核心多池隔离、外部线程发送和原控制指标。每项独立进程、45秒硬截止，必须串行。
//! 不执行TLS或test.rs；不把SDK重复输出旧值当作生产又采了一次，也不声称线上后端验证。
//! 监控依赖Linux的/proc与进程组，本目标仅在Linux构建，不宣称其它平台已经验证。

#![cfg(target_os = "linux")]

#[path = "support/hibernate_process.rs"]
mod process;
#[path = "support/write_queue_metrics_network.rs"]
mod network;

use std::process::Command;
use std::time::{Duration, Instant};
use network::{Client, Host, Server, Workers, FEATURE_ENABLED};

/// 被测TCP指标装配：两种生命周期分开运行；原监控确保前项进程组清空后再下一项。
#[test]
fn test_write_queue_metrics_real_tcp() {
    if let Ok(case) = std::env::var("PI_TCP_WQM_CASE") { run_case(&case); return; }
    for case in ["single", "shared"] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "test_write_queue_metrics_real_tcp", "--nocapture", "--test-threads=1"])
            .env("PI_TCP_WQM_CASE", case);
        let result = process::run_child(&mut command, Duration::from_secs(45), &format!("wqm-{case}"));
        assert!(!result.timed_out && result.status.success(), "TCP指标专项失败: {}\n{}\n{}",
            result.artifacts.display(), result.stdout, result.stderr);
        println!("监控：{case} CPU ticks={} 阶段停推={} 进程组已清空", result.cpu_ticks, result.stalled_after_probes);
    }
}

/// 前1秒确认未虚构初值，然后持续真实收发到17秒，至少跨过一个真实15秒周期。
/// shared使用同一个外部Local的Clone形成两个池，Scope必须仍为两点且rt.name相同。
fn run_case(case: &str) {
    assert!(matches!(case, "single" | "shared"));
    let host = Host::new();
    let workers = Workers::start(2);
    let pools = if case == "shared" { 2 } else { 1 };
    let server = Server::start(pools, case == "shared", workers.sender.clone());
    let mut clients = (0..4).map(|index| Client::connect(server.port, index)).collect::<Vec<_>>();
    for _ in 0..clients.len() { server.connected.recv_timeout(Duration::from_secs(3)).expect("缺建连回调"); }
    let start = Instant::now();
    let mut exchanges = 0u64;
    while start.elapsed() < Duration::from_secs(1) {
        for client in &mut clients { client.round_trip(); exchanges += 1; }
    }
    assert!(host.collect().is_empty(), "未满15秒不能记录初始零值");
    while start.elapsed() < Duration::from_secs(17) {
        for client in &mut clients { client.round_trip(); exchanges += 1; }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(server.errors.is_empty(), "测量中服务端失败: {:?}", server.errors.try_iter().collect::<Vec<_>>());
    let points = host.collect();
    server.check_points(&points, clients.len());
    assert_eq!(points.is_empty(), !FEATURE_ENABLED);
    drop(clients);
    assert_eq!(workers.finish(), exchanges, "每个真实完成帧应恰好提交一次");
    server.finish();
    let retained = host.collect();
    // SDK可能保留旧Gauge。只比对历史点没有被伪记归零/改身份，不能据此统计停止后调用数。
    assert_eq!(retained, points);
    println!("场景={case} feature={FEATURE_ENABLED} 往返={exchanges} 池数={pools} 线程退出=已确认");
}
