//! B-CTX-P01上下文（Context）生命周期修复前后真实TCP/TLS基准（benchmark）。
//!
//! 只驱动当前公开生产链，关联tests/support/context_network.rs与本地冻结计划。
//! 五组规模/三负载/两种传输（transport）各5轮，预热1秒、固定测量窗口3秒。
//! 仅窗口内开始且正确完成的真实往返（round trip）计吞吐，迟到单列，不挑轮次。
//! 每点独立进程组（process group），30秒硬截止，串行（serial）清理；不运行test.rs。
//! 原始数据在测量后写入docs，不安装Provider，不采集任何生产覆盖率（coverage）。
//! 仅Linux支持该进程（process）监控；无格式化、生产修改或新依赖。

use pi_tcp as tcp;

#[path = "../tests/support/context_network.rs"]
mod network;
#[path = "../tests/support/hibernate_process.rs"]
mod process;

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, OnceLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use serde_json::{json, Value};
use network::{Client, Event, Transport, Window, Workload};

const CHILD_ENV: &str = "PI_TCP_CONTEXT_POINT";
const DATA_ENV: &str = "PI_TCP_CONTEXT_DATA";
const WARMUP: Duration = Duration::from_secs(1);
const MEASURE: Duration = Duration::from_secs(3);
const REPEATS: usize = 5;
const CONFIGURATIONS: [(usize, usize); 5] = [(1, 1), (32, 1), (32, 4), (128, 1), (128, 4)];

/// 一个固定测量点；所有资源参数在解析（parse）时限制，不能从环境扩为无界资源。
#[derive(Clone, Copy, Debug)]
struct Point { transport: Transport, workload: Workload, connections: usize, owners: usize, round: usize }

impl Point {
    /// 冷标识（identifier），O(1)长度，有一次字符串分配，不进入测量窗口（window）。
    fn transport_name(self) -> &'static str { match self.transport { Transport::Tcp => "tcp", Transport::Tls => "tls" } }

    /// 结构化参数（parameter）编码，不经过命令行解释器（shell）。
    fn encode(self) -> Value {
        json!({"transport": self.transport_name(), "workload": self.workload.name(),
            "connections": self.connections, "owners": self.owners, "round": self.round})
    }

    /// 仅接受冻结矩阵（matrix）成员；非法值在分配/线程（thread）启动前失败。
    fn decode(value: &str) -> Self {
        let value: Value = serde_json::from_str(value).expect("基准参数必须是JSON");
        let number = |key| usize::try_from(value[key].as_u64().expect("基准整数参数缺失")).unwrap();
        let point = Self {
            transport: match value["transport"].as_str().unwrap() { "tcp" => Transport::Tcp, "tls" => Transport::Tls, _ => panic!("非法传输") },
            workload: match value["workload"].as_str().unwrap() { "plain" => Workload::Plain,
                "context_access" => Workload::Access, "context_replace" => Workload::Replace, _ => panic!("非法负载") },
            connections: number("connections"), owners: number("owners"), round: number("round"),
        };
        assert!(CONFIGURATIONS.contains(&(point.connections, point.owners)) && point.round < REPEATS);
        point
    }

    /// 唯一目录（directory）分量，不使用不受限的环境字符串作为路径（path）。
    fn tag(self) -> String {
        format!("{}-{}-{}-{}-{}", self.transport_name(), self.workload.name(), self.connections, self.owners, self.round)
    }
}

/// 一个客户端（client）的原始样本；连接（connection）另随返回值保留至全部测量结束。
struct Samples {
    warmup: u64,
    late: Vec<u64>,
    rtt: Vec<u64>,
    all: u64,
}

/// 对有限采样数组（sample array）计算最近秩百分位（percentile），O(N log N)。
/// 只在测量后排序；空组件样本返回null，端到端（end-to-end）样本由调用方拒绝为空。
fn summary(values: &mut [u64]) -> Value {
    if values.is_empty() { return Value::Null; }
    values.sort_unstable();
    let at = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    json!({"count": values.len(), "p50_ns": at(50), "p95_ns": at(95),
        "p99_ns": at(99), "max_ns": values.last().unwrap(),
        "mean_ns": values.iter().map(|v| *v as u128).sum::<u128>() as f64 / values.len() as f64})
}

/// 读取当前进程（process）CPU tick；按procfs规范处理带空格的命令名，不补零吞错。
/// O(文件长度)，同步文件读取，仅测量边界各一次；不是生产采集（collection）。
fn cpu_ticks() -> u64 {
    let text = fs::read_to_string("/proc/self/stat").expect("读取进程CPU失败");
    let tail = text.rsplit_once(") ").expect("非法procfs格式").1;
    let fields: Vec<_> = tail.split_whitespace().collect();
    fields[11].parse::<u64>().unwrap().checked_add(fields[12].parse().unwrap()).unwrap()
}

/// 当前驻留内存（RSS）和历史峰值，单位KiB；O(文件长度)，仅冷测量边界读取。
/// 峰值包含本点启动，不冒充纯生产堆分配（heap allocation）或测量窗口（window）独占峰值。
fn memory() -> Value {
    let text = fs::read_to_string("/proc/self/status").expect("读取进程内存失败");
    let read = |key| text.lines().find_map(|line| line.strip_prefix(key))
        .expect("缺少进程内存字段").split_whitespace().next().unwrap().parse::<u64>().unwrap();
    json!({"rss_kib": read("VmRSS:"), "peak_rss_kib": read("VmHWM:")})
}

/// 等到一个有限绝对时点（instant），不忙等；仅父测量线程（thread）使用。
fn wait_until(at: Instant) { thread::sleep(at.saturating_duration_since(Instant::now())); }

/// 独立客户端（client）逻辑：闭式计算64次更新的期望值，不复制服务端逐轮实现。
/// 同步阻塞网络；全部请求受3秒读写及外部30秒截止约束，原始样本空间O(完成次数)。
fn sample_client(mut client: Client, index: usize, workload: Workload,
                 gate: Arc<Barrier>, epoch: Arc<OnceLock<Window>>) -> (Client, Samples) {
    let mut result = Samples { warmup: 0, late: Vec::new(), rtt: Vec::with_capacity(4096), all: 0 };
    let mut sequence = 0u64;
    let mut sum = 0u64;
    gate.wait();
    let window = *epoch.get().unwrap();
    loop {
        let started = Instant::now();
        if started >= window.end { break; }
        let byte = ((index as u64 + result.all) % 251) as u8;
        let expected = match workload {
            Workload::Plain => byte,
            Workload::Access => {
                sequence = sequence.wrapping_add(64);
                sum = sum.wrapping_add(64 * (u64::from(byte) + 1));
                byte ^ sequence as u8 ^ sum as u8
            },
            Workload::Replace => byte ^ 1 ^ (2 * u64::from(byte) + 1) as u8,
        };
        client.exchange(byte, expected);
        let completed = Instant::now();
        let nanos = completed.duration_since(started).as_nanos() as u64;
        result.all += 1;
        if completed > window.end { result.late.push(nanos); }
        else if started < window.begin { result.warmup += 1; }
        else { result.rtt.push(nanos); }
    }
    assert_eq!(result.all, result.warmup + result.late.len() as u64 + result.rtt.len() as u64);
    (client, result)
}

/// 创建新文件（file），不能覆盖旧证据；O(输出大小)，只在测量完成后执行。
fn write_json(path: &Path, value: &Value) {
    let file = OpenOptions::new().write(true).create_new(true).open(path).expect("创建唯一数据文件失败");
    // 原始样本很多，缓冲写入（buffered write）仅在测量结束后进行，避免逐数字系统调用。
    let mut writer = std::io::BufWriter::with_capacity(64 * 1024, file);
    serde_json::to_writer(&mut writer, value).expect("保存原始JSON数据失败");
    std::io::Write::flush(&mut writer).expect("原始JSON数据刷新失败");
}

/// 一个真实点的全部测量；不补关运行时（runtime），不以进程（process）结束假报产品关闭。
/// 建连分布按实际所属线程（owner thread）记录，不强制四池均衡或更改路由（routing）。
fn measure(point: Point, data: &Path) {
    let startup = Instant::now();
    let mut server = network::start_server(point.transport, point.owners, point.workload);
    let mut clients = Vec::with_capacity(point.connections);
    let mut owners = Vec::<ThreadId>::new();
    for _ in 0..point.connections {
        clients.push(Client::connect(point.transport, server.port));
        match server.events.recv_timeout(Duration::from_secs(3)).expect("建连回调未完成") {
            Event::Connected(owner) => { if !owners.contains(&owner) { owners.push(owner); } },
            Event::Error(error) => panic!("真实建连失败: {error}"),
            _ => panic!("建连阶段收到非预期事件"),
        }
    }
    assert!(!owners.is_empty() && owners.len() <= point.owners);
    let startup_ns = startup.elapsed().as_nanos() as u64;
    let gate = Arc::new(Barrier::new(point.connections + 1));
    let epoch = Arc::new(OnceLock::<Window>::new());
    let threads = clients.into_iter().enumerate().map(|(index, client)| {
        let gate = gate.clone();
        let epoch = epoch.clone();
        thread::Builder::new().name(format!("ctx-client-{index}")).spawn(move ||
            sample_client(client, index, point.workload, gate, epoch)).expect("创建客户端线程失败")
    }).collect::<Vec<_>>();
    let begin = Instant::now() + WARMUP;
    let window = Window { begin, end: begin + MEASURE };
    server.window.set(window).unwrap_or_else(|_| panic!("测量窗口重复发布"));
    epoch.set(window).unwrap_or_else(|_| panic!("客户端测量窗口重复发布"));
    gate.wait();
    wait_until(window.begin);
    let cpu_begin_at = Instant::now();
    let cpu_begin = cpu_ticks();
    let memory_begin = memory();
    wait_until(window.end);
    let cpu_end_at = Instant::now();
    let cpu_end = cpu_ticks();
    let memory_end = memory();
    let (retained, samples): (Vec<_>, Vec<_>) = threads.into_iter()
        .map(|thread| thread.join().expect("真实网络测量线程失败")).unzip();
    server.begin_teardown();
    drop(retained);
    let mut closed = 0usize;
    let mut reports = Vec::new();
    let teardown_deadline = Instant::now() + Duration::from_secs(5);
    while closed < point.connections || reports.len() < owners.len() {
        let remaining = teardown_deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "真实关闭回调或采样未返回");
        match server.events.recv_timeout(remaining).expect("真实关闭回调未完成") {
            Event::Closed => closed += 1,
            Event::Samples(report) => {
                assert!(owners.contains(&report.owner) && !reports.iter().any(|r: &network::OwnerSamples| r.owner == report.owner));
                reports.push(report);
            },
            Event::Error(error) => panic!("服务端不能隐藏失败: {error}"),
            Event::Connected(_) => panic!("测量结束后不应出现新建连"),
        }
    }
    assert_eq!(closed, point.connections);
    let close_return_ns = server.close();
    assert!(server.events.try_recv().is_err(), "测量结束不能遗留额外事件");
    let raw = json!({"point": point.encode(),
        "rtt_window_ns_by_client": samples.iter().map(|s| &s.rtt).collect::<Vec<_>>(),
        "late_ns_by_client": samples.iter().map(|s| &s.late).collect::<Vec<_>>(),
        "warmup_by_client": samples.iter().map(|s| s.warmup).collect::<Vec<_>>(),
        "component_ns_by_owner": reports.iter().map(|s| &s.nanos).collect::<Vec<_>>(),
        "owner_ids": reports.iter().map(|s| format!("{:?}", s.owner)).collect::<Vec<_>>()});
    write_json(&data.join("raw_samples.json"), &raw);
    drop(raw);
    let all = samples.iter().map(|s| s.all).sum::<u64>();
    let warmup = samples.iter().map(|s| s.warmup).sum::<u64>();
    let late = samples.iter().map(|s| s.late.len() as u64).sum::<u64>();
    let mut rtt = samples.into_iter().flat_map(|s| s.rtt).collect::<Vec<_>>();
    assert!(!rtt.is_empty() && all == warmup + late + rtt.len() as u64);
    if point.workload != Workload::Plain {
        assert_eq!(reports.iter().map(|s| s.requests).sum::<u64>(), all);
    }
    let mut component = reports.into_iter().flat_map(|s| s.nanos).collect::<Vec<_>>();
    if point.workload != Workload::Plain { assert!(!component.is_empty()); }
    let operations = rtt.len();
    let rtt_summary = summary(&mut rtt);
    let component_summary = summary(&mut component);
    let result = json!({"point": point.encode(), "pid": std::process::id(), "owners_used": owners.len(),
        "all_requests": all, "warmup_requests": warmup, "window_requests": operations, "late_requests": late,
        "measurement_ns": MEASURE.as_nanos() as u64, "throughput": operations as f64 / MEASURE.as_secs_f64(),
        "rtt": rtt_summary, "component_batch": component_summary,
        "component_operations_per_batch": if point.workload == Workload::Access { 64 } else { 1 },
        "cpu_ticks": cpu_end.checked_sub(cpu_begin).expect("CPU计数倒退"),
        "cpu_interval_ns": cpu_end_at.duration_since(cpu_begin_at).as_nanos() as u64,
        "cpu_begin_late_ns": cpu_begin_at.saturating_duration_since(window.begin).as_nanos() as u64,
        "cpu_end_late_ns": cpu_end_at.saturating_duration_since(window.end).as_nanos() as u64,
        "memory_begin": memory_begin, "memory_end": memory_end,
        "startup_ns": startup_ns, "listener_close_return_ns": close_return_ns,
        "sizes": {"socket_context": std::mem::size_of::<tcp::utils::SocketContext>(),
            "context_handle": std::mem::size_of::<tcp::utils::ContextHandle<[u64; 2]>>(),
            "tcp_socket": std::mem::size_of::<tcp::connect::TcpSocket>(),
            "tls_socket": std::mem::size_of::<tcp::tls_connect::TlsSocket>()},
        "errors": 0, "data": data});
    write_json(&data.join("summary.json"), &result);
    println!("CTX_BASELINE_RESULT\t{result}");
}

/// 父入口（parent entry）的单点隔离，复用原身份安全监控；任何失败均先清组再报错。
fn run(point: Point, root: &Path) -> Value {
    let data = root.join(point.tag());
    fs::create_dir(&data).expect("不能覆盖既有基准点");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env(CHILD_ENV, point.encode().to_string()).env(DATA_ENV, &data).env("RUST_BACKTRACE", "1");
    eprintln!("CTX_BASELINE_BEGIN\t{}", point.tag());
    let child = process::run_child(&mut command, Duration::from_secs(30), &format!("context-{}", point.tag()));
    eprintln!("CTX_BASELINE_END\t{}\ttimeout={}\tstatus={}\tmonitor_cpu_ticks={}\tprobe_stall={}\tmonitor={}",
        point.tag(), child.timed_out, child.status, child.cpu_ticks, child.stalled_after_probes, child.artifacts.display());
    assert!(!child.timed_out && child.status.success(), "基准点失败，所属组已清理\n{}\n{}", child.stdout, child.stderr);
    let rows = child.stdout.lines().filter_map(|line| line.strip_prefix("CTX_BASELINE_RESULT\t")).collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "必须恰有一份完整测量结果");
    let result = serde_json::from_str(rows[0]).expect("非法测量JSON");
    println!("CTX_BASELINE_RESULT\t{}", rows[0]);
    result
}

/// 串行（serial）矩阵（matrix）入口；--smoke仅选6个单连接点，不改变参数/断言。
/// 父进程（process）不创建运行时（runtime），环境设置仅在单线程启动阶段进行。
fn execute() {
    assert!(cfg!(target_os = "linux"), "本基准监控只支持Linux");
    if let Ok(point) = std::env::var(CHILD_ENV) {
        measure(Point::decode(&point), &PathBuf::from(std::env::var_os(DATA_ENV).expect("缺少原始数据目录")));
        return;
    }
    let args = std::env::args().skip(1).filter(|arg| arg != "--bench").collect::<Vec<_>>();
    assert!(args.is_empty() || args == ["--smoke"], "只接受固定整批或--smoke");
    let smoke = !args.is_empty();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/tcp_context_20261028");
    fs::create_dir_all(&base).expect("创建基准归档根目录失败");
    let root = base.canonicalize().unwrap().join(format!("{}-{}-{}", if smoke { "smoke" } else { "before" },
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(), std::process::id()));
    fs::create_dir(&root).expect("创建独立整批目录失败");
    let monitor = root.join("monitor");
    fs::create_dir(&monitor).unwrap();
    std::env::set_var("PI_TCP_HIB_ARTIFACT_DIR", &monitor);
    eprintln!("CTX_BASELINE_ROOT\t{}", root.display());
    let mut results = Vec::new();
    for transport in [Transport::Tcp, Transport::Tls] {
        for (connections, owners) in CONFIGURATIONS {
            if smoke && connections != 1 { continue; }
            for round in 0..if smoke { 1 } else { REPEATS } {
                for workload in [Workload::Plain, Workload::Access, Workload::Replace] {
                    results.push(run(Point { transport, workload, connections, owners, round }, &root));
                }
            }
        }
    }
    assert_eq!(results.len(), if smoke { 6 } else { 150 });
    write_json(&root.join("results.json"), &json!({"plan": "B-CTX-P01", "smoke": smoke, "results": results}));
    println!("CTX_BASELINE_COMPLETE\t{}\t{}", results.len(), root.display());
}

fn main() { execute(); }
