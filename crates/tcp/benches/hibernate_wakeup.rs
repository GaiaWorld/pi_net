//! Hibernate 外部线程唤醒的真实 TCP/TLS 基准，关联 TCP-HIB-REPAIR-001/D-HIB-04。
//!
//! 每个操作都收到实际网络回包才计成功；休眠模式每次调用原 Hibernate Future，
//! 外部固定线程只用原公开句柄唤醒。普通收发对照复用同一网络路径但不休眠。
//! 正常库产物没有 cfg(test) 阶段探针；不把确定性交错或等待超时计为吞吐样本。
//! 各点独立进程组、串行执行，退出/失败/截止均统一清理；不运行 test.rs。
//! 固定矩阵和断言用于修复前后比较，不能测后缩减数据或放宽断言。

use pi_tcp as tcp;

#[path = "../tests/support/hibernate_network.rs"]
mod network;
#[path = "../tests/support/hibernate_process.rs"]
mod process;

use std::process::Command;
use std::sync::{Arc, Barrier, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use network::{Client, ServiceMode, Transport, WakeWorkers};

const CHILD_ENV: &str = "PI_TCP_HIB_BENCH_CHILD";
const WARMUP: Duration = Duration::from_secs(1);
const MEASURE: Duration = Duration::from_secs(3);
const REPEATS: usize = 5;

/// 一个冻结的测量点；只使用固定数值，不接受无界线程数或连接数。
#[derive(Clone, Copy)]
struct Point {
    transport: Transport,
    hibernate: bool,
    connections: usize,
    external: usize,
    owners: usize,
    repeat: usize,
}

impl Point {
    fn transport_name(self) -> &'static str {
        match self.transport { Transport::Tcp => "tcp", Transport::Tls => "tls" }
    }

    fn mode_name(self) -> &'static str { if self.hibernate { "hibernate" } else { "echo" } }

    /// 同二进制子进程参数，只包含固定枚举和整数，不向 shell 插入命令。
    fn encode(self) -> String {
        format!("{},{},{},{},{},{}", self.transport_name(), self.mode_name(),
            self.connections, self.external, self.owners, self.repeat)
    }

    fn decode(value: &str) -> Self {
        let fields = value.split(',').collect::<Vec<_>>();
        assert_eq!(fields.len(), 6);
        let transport = match fields[0] { "tcp" => Transport::Tcp, "tls" => Transport::Tls,
            _ => panic!("未知基准传输") };
        let hibernate = match fields[1] { "hibernate" => true, "echo" => false,
            _ => panic!("未知基准模式") };
        let point = Self { transport, hibernate, connections: fields[2].parse().unwrap(),
            external: fields[3].parse().unwrap(), owners: fields[4].parse().unwrap(),
            repeat: fields[5].parse().unwrap() };
        assert!([(1, 1, 1), (32, 4, 1), (32, 4, 4), (128, 8, 1), (128, 8, 4)]
            .contains(&(point.connections, point.external, point.owners)));
        assert!(point.repeat < REPEATS);
        point
    }
}

/// 客户端完整回包的独立样本；延迟覆盖网络、owner、外部队列与实际休眠恢复。
struct ClientSamples {
    all_operations: usize,
    measured: Vec<u64>,
    last_completed: Instant,
    client: Option<Client>,
}

/// 最近秩百分位，输入原始纳秒数组；O(n log n)排序在测量结束后执行。
/// 空数组返回零仅用于无外部唤醒的 Echo 对照，测量往返数组不允许为空。
fn percentiles(values: &mut [u64]) -> [u64; 4] {
    if values.is_empty() { return [0; 4]; }
    values.sort_unstable();
    let at = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    [at(50), at(95), at(99), *values.last().unwrap()]
}

/// 一个隔离点的完整测量；握手/连接/owner启动在预热之前，统计排序在测量之后。
/// 所有等待最终受外层30秒进程截止约束，线程或网络失败直接失败，不补成功次数。
fn measure_point(point: Point) {
    let workers = point.hibernate.then(|| WakeWorkers::start(point.external, None));
    let server = network::start_server(point.transport, point.owners, ServiceMode {
        jobs: workers.as_ref().map(|workers| workers.sender.clone()), force_repoll: false,
        suppress_tls_tickets: true,
    });
    let clients = (0..point.connections).map(|_| {
        let client = Client::connect(point.transport, server.port);
        let target = server.connected.recv_timeout(Duration::from_secs(3)).expect("基准未完成真实建连");
        assert_ne!(target.uid(), 0, "真实连接必须具有原生产UID");
        client
    }).collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(point.connections + 1));
    let epoch = Arc::new(OnceLock::<Instant>::new());
    let threads = clients.into_iter().enumerate().map(|(index, mut client)| {
        let barrier = barrier.clone();
        let epoch = epoch.clone();
        thread::Builder::new().name(format!("hibernate-client-{index}")).spawn(move || {
            let mut samples = ClientSamples { all_operations: 0,
                measured: Vec::with_capacity(4096), last_completed: Instant::now(), client: None };
            barrier.wait();
            let measured_start = *epoch.get().unwrap() + WARMUP;
            let end = measured_start + MEASURE;
            loop {
                let started = Instant::now();
                if started >= end { break; }
                let byte = ((index + samples.all_operations) % 251) as u8;
                client.round_trip(byte);
                samples.last_completed = Instant::now();
                samples.all_operations += 1;
                if started >= measured_start {
                    samples.measured.push(started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
                }
            }
            samples.client = Some(client);
            samples
        }).expect("创建真实网络客户端线程失败")
    }).collect::<Vec<_>>();
    epoch.set(Instant::now()).unwrap();
    barrier.wait();
    let measured_start = *epoch.get().unwrap() + WARMUP;
    let scheduled_end = measured_start + MEASURE;
    let mut measured = Vec::new();
    let mut all_operations = 0;
    let mut last_completed = scheduled_end;
    let mut retained_clients = Vec::with_capacity(point.connections);
    for thread in threads {
        let mut samples = thread.join().unwrap_or_else(|error| {
            panic!("真实网络客户端失败: {error:?}; 服务端错误: {:?}",
                server.errors.try_iter().collect::<Vec<_>>())
        });
        all_operations += samples.all_operations;
        last_completed = last_completed.max(samples.last_completed);
        measured.append(&mut samples.measured);
        retained_clients.push(samples.client.take().unwrap());
    }
    assert!(!measured.is_empty(), "不能以空测量区间给出吞吐结果");
    let wakes = workers.map(WakeWorkers::finish).unwrap_or_default();
    assert_eq!(wakes.len(), if point.hibernate { all_operations } else { 0 },
        "每次休眠请求必须恰好完成一个真实外部唤醒");
    let errors = server.errors.try_iter().collect::<Vec<_>>();
    assert!(errors.is_empty(), "真实服务不能隐藏失败: {errors:?}");
    let retries = wakes.iter().map(|sample| sample.retries).sum::<u64>();
    let mut wake_nanos = wakes.iter().filter(|sample|
        sample.started >= measured_start && sample.started < scheduled_end)
        .map(|sample| sample.nanos).collect::<Vec<_>>();
    let seconds = last_completed.duration_since(measured_start).as_secs_f64();
    let operations = measured.len();
    let rtt = percentiles(&mut measured);
    let wake = percentiles(&mut wake_nanos);
    println!("HIB_BENCH_RESULT\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.9}\t{:.3}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        point.transport_name(), point.mode_name(), point.connections, if point.hibernate { point.external } else { 0 },
        point.owners, point.repeat, all_operations, operations, seconds,
        operations as f64 / seconds, rtt[0], rtt[1], rtt[2], rtt[3],
        wake_nanos.len(), wake[0], wake[1], wake[2], wake[3], retries);
}

/// 每点均经过同一进程组监控；冒烟只缩减点数，不改变单点负载或成功断言。
fn run_point(point: Point) {
    eprintln!("开始真实网络基准 {}", point.encode());
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env(CHILD_ENV, point.encode());
    let result = process::run_child(&mut command, Duration::from_secs(30),
        &format!("bench-{}-{}-{}-{}-{}", point.transport_name(), point.mode_name(),
            point.connections, point.owners, point.repeat));
    eprintln!("基准结束 超时={} 阶段停推={} CPU_tick={} 原件={}",
        result.timed_out, result.stalled_after_probes, result.cpu_ticks, result.artifacts.display());
    assert!(!result.timed_out && result.status.success(),
        "基准未完成，所属进程组已清理\n{}\n{}", result.stdout, result.stderr);
    let rows = result.stdout.lines().filter(|line| line.starts_with("HIB_BENCH_RESULT\t"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "每个隔离点必须输出一份完整结果");
    println!("{}", rows[0]);
}

/// 矩阵父进程；同点 Echo/Hibernate 相邻运行，各重复5次，避免并行资源污染。
/// 任何异常先清理再失败，尚未执行的点必须记为缺口，不能伪造基准数据。
fn main() {
    if let Ok(encoded) = std::env::var(CHILD_ENV) {
        measure_point(Point::decode(&encoded));
        return;
    }
    println!("标记\t传输\t模式\t连接\t外部线程\towner\t重复\t全部操作\t测量成功\t实际测量秒\t成功每秒\t往返P50ns\t往返P95ns\t往返P99ns\t往返最大ns\t唤醒样本\t唤醒P50ns\t唤醒P95ns\t唤醒P99ns\t唤醒最大ns\t全部重试");
    if std::env::args().any(|arg| arg == "--smoke") {
        for transport in [Transport::Tcp, Transport::Tls] {
            for hibernate in [false, true] {
                run_point(Point { transport, hibernate, connections: 1, external: 1, owners: 1, repeat: 0 });
            }
        }
        return;
    }
    for transport in [Transport::Tcp, Transport::Tls] {
        for (connections, external, owners) in [(1, 1, 1), (32, 4, 1), (32, 4, 4),
            (128, 8, 1), (128, 8, 4)] {
            for repeat in 0..REPEATS {
                for hibernate in [false, true] {
                    let point = Point { transport, hibernate, connections, external, owners, repeat };
                    run_point(point);
                }
            }
        }
    }
}
