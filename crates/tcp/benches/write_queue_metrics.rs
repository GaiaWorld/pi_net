//! 发送队列观测的真实TCP开关及普通标签对比专项；WQM-B02/WQM-G03。
//! 四场景、20秒预热/60秒测量/各3轮，使用同源的开/关两份Release二进制。
//! --matrix <off> <on> <新归档目录>按交替顺序串行运行24点，每点120秒截止，
//! 原监控确认所属进程组清空后再继续；--point仅由此调度器在受监控子进程调用。
//! --three-way <旧off> <旧Scope> <新标签> <新目录>沿用完全相同的点负载，
//! 每轮轮换三种产物的执行顺序，共36点，分别比较新标签对off和旧Scope的差异。
//! --cache-focus <旧Scope> <内联标签> <Box标签> <新目录>仅复验128/8/4，三轮9点；
//! 原负载/阶段/统计不变，分别比较Box标签对旧Scope及内联标签，不重跑其它场景。
//! --smoke <新目录>执行1秒预热/1秒测量的夹具检查，明确不是性能或采样验收数据。
//!
//! 被测生产：connect_pool原真实发送路径和可选write_queue_metrics，固定256字节、
//! 每连接一个在途请求、外部发送线程、原Poll=None。延迟为完整网络往返，不是入队。
//! HDR固定1ns～120秒/3有效数字，不自动扩容，超范围/统计/网络错误使该点无效。
//! 直方图每客户独占，结束后合并；精确最大值另存。SDK采集由相同测试宿主执行，
//! 不向生产路径加计时探针。CPU/RSS含同进程的客户、应用线程、SDK及连接池，
//! 不是连接池单线程归因。仅Linux真实TCP，不运行TLS/test.rs，不自动创建Git提交。

#[path = "../tests/support/hibernate_process.rs"]
mod process;
#[path = "../tests/support/write_queue_metrics_network.rs"]
mod network;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use crossbeam_channel::{bounded, Receiver, Sender};
use hdrhistogram::{Histogram, serialization::{Serializer, V2Serializer}};
use serde_json::{json, Value};
use network::{Client, Host, Server, Workers, FEATURE_ENABLED, FRAME_BYTES};

const WARM_SECONDS: u64 = 20;
const MEASURE_SECONDS: u64 = 60;

/// 固定冻结场景；外部发送线程数不包括真实客户线程和宿主SDK线程。
#[derive(Clone, Copy)]
struct Scenario { name: &'static str, connections: usize, workers: usize, pools: usize, idle: bool }

const SCENARIOS: [Scenario; 4] = [
    Scenario { name: "idle-1-1-1", connections: 1, workers: 1, pools: 1, idle: true },
    Scenario { name: "active-1-1-1", connections: 1, workers: 1, pools: 1, idle: false },
    Scenario { name: "active-32-4-1", connections: 32, workers: 4, pools: 1, idle: false },
    Scenario { name: "active-128-8-4", connections: 128, workers: 8, pools: 4, idle: false },
];

/// 单客户固定空间统计；exchanges包括截止边缘已开始但迟到完成的真实往返。
/// 迟到仍必须完整校验，只是不混入固定测量窗的吞吐/延迟，数量单列而不是丢弃。
struct Samples { histogram: Histogram<u64>, exchanges: u64, late: u64, exact_max: u64 }

impl Samples {
    fn new() -> Self {
        let mut histogram = Histogram::new_with_bounds(1, 120_000_000_000, 3).expect("固定HDR配置无效");
        histogram.auto(false);
        Self { histogram, exchanges: 0, late: 0, exact_max: 0 }
    }

    /// 只有收到相同完整帧才记录成功；不截短或饱和延迟，任何超界都直接失败。
    fn run(&mut self, client: &mut Client, until: Instant, idle: bool) {
        while Instant::now() < until {
            let began = Instant::now();
            client.round_trip();
            let completed = Instant::now();
            self.exchanges = self.exchanges.checked_add(1).expect("往返计数溢出");
            if completed <= until {
                let nanos = u64::try_from(completed.duration_since(began).as_nanos()).expect("延迟溢出u64");
                assert!((1..=120_000_000_000).contains(&nanos), "延迟超出冻结HDR范围");
                self.histogram.record(nanos).expect("HDR记录失败，不能丢样");
                self.exact_max = self.exact_max.max(nanos);
            } else { self.late = self.late.checked_add(1).expect("迟到计数溢出"); }
            if idle {
                let next = (began + Duration::from_secs(1)).min(until);
                if let Some(wait) = next.checked_duration_since(Instant::now()) { thread::sleep(wait); }
            }
        }
        assert_eq!(self.histogram.len().checked_add(self.late), Some(self.exchanges));
    }
}

/// 测试控制通道，仅同步客户阶段；不是生产任务队列或指标采样机制。
enum Phase { Run(Instant), Finish }
enum Event { Ready, Warmed(u64), Measured(Samples) }

/// 读取Linux公开进程快照，不穿透生产/SDK状态；CPU包含整个被测子进程。
fn cpu_ticks() -> u64 {
    let stat = fs::read_to_string("/proc/self/stat").expect("读取进程CPU失败");
    let fields = stat.rsplit_once(") ").expect("进程stat格式错误").1.split_whitespace().collect::<Vec<_>>();
    fields[11].parse::<u64>().unwrap().checked_add(fields[12].parse().unwrap()).expect("CPU计数溢出")
}

/// 当前RSS与生命周期最大RSS，单位KiB；解析失败使该点失败，不用零掩盖缺失。
fn memory_kib() -> (u64, u64) {
    let status = fs::read_to_string("/proc/self/status").expect("读取内存失败");
    let get = |key: &str| status.lines().find_map(|line| line.strip_prefix(key))
        .expect("缺少Linux内存字段").split_whitespace().next().unwrap().parse::<u64>().unwrap();
    (get("VmRSS:"), get("VmHWM:"))
}

/// 运行一相位并在宿主线程逐秒采集；所有客户都有完成通知，硬截止仍由外层进程保证。
/// 结束时返回客户原统计和RSS序列，不把重复SDK导出次数作为生产采样次数。
fn wait_phase(receiver: &Receiver<Event>, count: usize, measured: bool, host: &Host,
    server: &Server, max_seconds: u64) -> (u64, Vec<Samples>, Vec<Value>, Vec<network::QueuePoint>) {
    let started = Instant::now();
    let mut next_collect = started;
    let mut completed = 0;
    let mut warm_exchanges = 0u64;
    let mut samples = Vec::new();
    let mut memory = Vec::new();
    let mut latest = Vec::new();
    while completed < count {
        assert!(started.elapsed() < Duration::from_secs(max_seconds), "客户阶段没有按时完成");
        assert!(server.errors.is_empty(), "服务端错误: {:?}", server.errors.try_iter().collect::<Vec<_>>());
        if Instant::now() >= next_collect {
            let points = host.collect();
            if !FEATURE_ENABLED || !points.is_empty() { server.check_points(&points, count); }
            if !points.is_empty() { latest = points; }
            let (rss, peak) = memory_kib();
            memory.push(json!({"elapsed_ms": started.elapsed().as_millis(), "rss_kib": rss, "peak_rss_kib": peak}));
            next_collect = Instant::now() + Duration::from_secs(1);
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Event::Warmed(exchanges)) if !measured => {
                warm_exchanges = warm_exchanges.checked_add(exchanges).expect("预热计数溢出");
                completed += 1;
            }
            Ok(Event::Measured(value)) if measured => { samples.push(value); completed += 1; }
            Ok(_) => panic!("客户阶段消息与当前相位不一致"),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("客户阶段通道提前关闭: {error}"),
        }
    }
    (warm_exchanges, samples, memory, latest)
}

/// 单点完整装配和统计；新目录防覆盖，所有真实客户/外部发送线程均显式join。
/// 失败由外层准确进程组监控结束，不能把失败点补作成功。smoke不要求15秒后输出。
fn run_point(scenario: Scenario, round: usize, output: &Path, smoke: bool) {
    fs::create_dir(output).expect("点归档目录必须不存在");
    let warm_seconds = if smoke { 1 } else { WARM_SECONDS };
    let measure_seconds = if smoke { 1 } else { MEASURE_SECONDS };
    let host = Host::new();
    let workers = Workers::start(scenario.workers);
    let server = Server::start(scenario.pools, false, workers.sender.clone());
    let (events_tx, events_rx) = bounded(scenario.connections);
    let mut commands: Vec<Sender<Phase>> = Vec::new();
    let mut clients = Vec::new();
    for index in 0..scenario.connections {
        let (command_tx, command_rx) = bounded(1);
        commands.push(command_tx);
        let events = events_tx.clone();
        let port = server.port;
        clients.push(thread::Builder::new().name(format!("wqm-client-{index}")).spawn(move || {
            let mut client = Client::connect(port, index as u64);
            let mut stats = Samples::new();
            events.send(Event::Ready).unwrap();
            let Phase::Run(until) = command_rx.recv_timeout(Duration::from_secs(5)).unwrap() else { panic!("缺预热指令") };
            stats.run(&mut client, until, scenario.idle);
            let warmed = stats.exchanges;
            stats.histogram.reset();
            stats.exchanges = 0;
            stats.late = 0;
            stats.exact_max = 0;
            events.send(Event::Warmed(warmed)).unwrap();
            let Phase::Run(until) = command_rx.recv_timeout(Duration::from_secs(10)).unwrap() else { panic!("缺测量指令") };
            stats.run(&mut client, until, scenario.idle);
            events.send(Event::Measured(stats)).unwrap();
            assert!(matches!(command_rx.recv_timeout(Duration::from_secs(10)).unwrap(), Phase::Finish));
        }).expect("启动真实客户线程失败"));
    }
    drop(events_tx);
    for _ in 0..scenario.connections {
        assert!(matches!(events_rx.recv_timeout(Duration::from_secs(5)).expect("客户未准备好"), Event::Ready));
        server.connected.recv_timeout(Duration::from_secs(3)).expect("原连接回调未执行");
    }
    let until = Instant::now() + Duration::from_secs(warm_seconds);
    for command in &commands { command.send(Phase::Run(until)).unwrap(); }
    let (warm_exchanges, _, _, warm_points) = wait_phase(&events_rx, scenario.connections, false,
        &host, &server, warm_seconds + 8);
    let (rss_start, _) = memory_kib();
    let cpu_start = cpu_ticks();
    let began = Instant::now();
    let until = began + Duration::from_secs(measure_seconds);
    for command in &commands { command.send(Phase::Run(until)).unwrap(); }
    let (_, samples, rss_samples, mut points) = wait_phase(&events_rx, scenario.connections, true,
        &host, &server, measure_seconds + 8);
    let actual_wall = began.elapsed().as_secs_f64();
    let cpu_used = cpu_ticks().checked_sub(cpu_start).expect("CPU计数倒退");
    let (rss_end, _) = memory_kib();
    if points.is_empty() { points = warm_points; }
    if !smoke { server.check_points(&points, scenario.connections); }
    let mut merged = Samples::new();
    for stats in samples {
        merged.histogram.add(&stats.histogram).expect("HDR合并失败");
        merged.exchanges = merged.exchanges.checked_add(stats.exchanges).expect("往返合计溢出");
        merged.late = merged.late.checked_add(stats.late).expect("迟到合计溢出");
        merged.exact_max = merged.exact_max.max(stats.exact_max);
    }
    assert_eq!(merged.histogram.len().checked_add(merged.late), Some(merged.exchanges));
    assert!(merged.histogram.len() > 0, "测量必须有真实网络成功样本");
    for command in commands { command.send(Phase::Finish).unwrap(); }
    let joined = clients.into_iter().map(|thread| thread.join()).collect::<Vec<_>>();
    for result in joined { result.expect("真实客户线程失败"); }
    assert!(server.errors.is_empty(), "测量期服务端存在错误");
    let submitted = workers.finish();
    assert_eq!(submitted, warm_exchanges.checked_add(merged.exchanges).unwrap());
    server.finish();
    drop(host);
    let ticks_output = Command::new("getconf").arg("CLK_TCK").output().expect("读取CPU时钟单位失败");
    assert!(ticks_output.status.success());
    let ticks_per_second: u64 = String::from_utf8(ticks_output.stdout).unwrap().trim().parse().unwrap();
    assert!(ticks_per_second > 0);
    let histogram = &merged.histogram;
    let successes = histogram.len();
    V2Serializer::new().serialize(histogram, &mut File::create(output.join("latency.hdr")).unwrap())
        .expect("保存原始HDR失败");
    let (_, peak_rss) = memory_kib();
    let result = json!({
        "schema": 1, "scenario": scenario.name, "round": round, "feature": FEATURE_ENABLED, "smoke": smoke,
        "connections": scenario.connections, "external_senders": scenario.workers, "pools": scenario.pools,
        "payload_bytes": FRAME_BYTES, "poll_timeout": null, "warm_seconds": warm_seconds,
        "measure_seconds": measure_seconds, "actual_wall_seconds": actual_wall,
        "successes": successes, "exchanges": merged.exchanges, "late_completions": merged.late,
        "warm_exchanges": warm_exchanges, "errors": 0, "timeouts": 0,
        "messages_per_second": successes as f64 / measure_seconds as f64,
        "payload_bytes_per_second": successes as f64 * FRAME_BYTES as f64 / measure_seconds as f64,
        "p50_ns": histogram.value_at_quantile(0.5), "p95_ns": histogram.value_at_quantile(0.95),
        "p99_ns": histogram.value_at_quantile(0.99), "exact_max_ns": merged.exact_max,
        "cpu_ticks": cpu_used, "cpu_ticks_per_second": ticks_per_second,
        "cpu_percent_one_core": cpu_used as f64 / ticks_per_second as f64 / actual_wall * 100.0,
        "rss_start_kib": rss_start, "rss_end_kib": rss_end, "peak_rss_kib": peak_rss,
        "rss_samples": rss_samples,
        "encoding": if FEATURE_ENABLED { "data-point-labels" } else { "disabled" },
        "label_storage": if FEATURE_ENABLED { "box" } else { "disabled" },
        "queue_points": points.iter().map(|point| json!({"attributes": point.attributes, "value": point.value})).collect::<Vec<_>>(),
        "sdk_control_unchanged": true, "owner_threads_exited": true,
    });
    fs::write(output.join("result.json"), serde_json::to_vec_pretty(&result).unwrap()).expect("保存点统计失败");
    println!("{}", serde_json::to_string(&result).unwrap());
}

fn median(mut values: Vec<f64>) -> f64 { assert_eq!(values.len(), 3); values.sort_by(f64::total_cmp); values[1] }

/// 冻结观察线：3轮中位数吞吐下降>5%或P99上升>5%只停止验收，不事后放宽。
fn compare(rows: &[Value], root: &Path) -> bool {
    assert_eq!(rows.len(), 24);
    let mut csv = String::from("scenario,feature,round,messages_per_second,p50_ns,p95_ns,p99_ns,exact_max_ns,cpu_percent_one_core,rss_start_kib,rss_end_kib,peak_rss_kib,successes,late_completions\n");
    for row in rows {
        csv.push_str(&format!("{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            row["scenario"].as_str().unwrap(), row["feature"], row["round"], row["messages_per_second"],
            row["p50_ns"], row["p95_ns"], row["p99_ns"], row["exact_max_ns"], row["cpu_percent_one_core"],
            row["rss_start_kib"], row["rss_end_kib"], row["peak_rss_kib"], row["successes"], row["late_completions"]));
    }
    fs::write(root.join("rounds.csv"), csv).unwrap();
    let mut observations = Vec::new();
    let mut accepted = true;
    for scenario in SCENARIOS {
        let med = |feature: bool, field: &str| median(rows.iter().filter(|row| {
            row["scenario"] == scenario.name && row["feature"] == feature
        }).map(|row| row[field].as_f64().unwrap()).collect());
        let throughput_delta = (med(true, "messages_per_second") / med(false, "messages_per_second") - 1.0) * 100.0;
        let p99_delta = (med(true, "p99_ns") / med(false, "p99_ns") - 1.0) * 100.0;
        let within = throughput_delta >= -5.0 && p99_delta <= 5.0;
        accepted &= within;
        observations.push(json!({"scenario": scenario.name, "throughput_delta_percent": throughput_delta,
            "p99_delta_percent": p99_delta, "within_observation_lines": within,
            "off_throughput": med(false, "messages_per_second"), "on_throughput": med(true, "messages_per_second"),
            "off_p99_ns": med(false, "p99_ns"), "on_p99_ns": med(true, "p99_ns"),
            "off_cpu_percent": med(false, "cpu_percent_one_core"), "on_cpu_percent": med(true, "cpu_percent_one_core"),
            "off_peak_rss_kib": med(false, "peak_rss_kib"), "on_peak_rss_kib": med(true, "peak_rss_kib")}));
    }
    fs::write(root.join("comparison.json"), serde_json::to_vec_pretty(&observations).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(&observations).unwrap());
    accepted
}

/// 三产物的相同负载比较；原始点JSON/HDR不改写，variant只加在独立汇总数据中。
/// 所有阈值沿用既定5%，一次超线也不能冒称通过；不把跨轮调度噪声归为纯SDK成本。
fn compare_variants(rows: &[Value], root: &Path, scenarios: &[Scenario], baselines: &[&str]) -> bool {
    assert_eq!(rows.len(), scenarios.len() * (baselines.len() + 1) * 3);
    let mut csv = String::from("scenario,variant,round,messages_per_second,p50_ns,p95_ns,p99_ns,exact_max_ns,cpu_percent_one_core,peak_rss_kib,successes,late_completions\n");
    for row in rows {
        csv.push_str(&format!("{},{},{},{},{},{},{},{},{},{},{},{}\n",
            row["scenario"].as_str().unwrap(), row["variant"].as_str().unwrap(), row["round"],
            row["messages_per_second"], row["p50_ns"], row["p95_ns"], row["p99_ns"], row["exact_max_ns"],
            row["cpu_percent_one_core"], row["peak_rss_kib"], row["successes"], row["late_completions"]));
    }
    fs::write(root.join("rounds.csv"), csv).unwrap();
    fs::write(root.join("rows.json"), serde_json::to_vec_pretty(rows).unwrap()).unwrap();
    let mut comparisons = Vec::new();
    let mut accepted = true;
    for scenario in scenarios {
        let med = |variant: &str, field: &str| median(rows.iter().filter(|row| {
            row["scenario"] == scenario.name && row["variant"] == variant
        }).map(|row| row[field].as_f64().unwrap()).collect());
        for &baseline in baselines {
            let throughput = (med("labels", "messages_per_second") / med(baseline, "messages_per_second") - 1.0) * 100.0;
            let p99 = (med("labels", "p99_ns") / med(baseline, "p99_ns") - 1.0) * 100.0;
            let within = throughput >= -5.0 && p99 <= 5.0;
            accepted &= within;
            comparisons.push(json!({"scenario": scenario.name, "baseline": baseline, "candidate": "labels",
                "throughput_delta_percent": throughput, "p99_delta_percent": p99,
                "within_observation_lines": within,
                "baseline_throughput": med(baseline, "messages_per_second"),
                "labels_throughput": med("labels", "messages_per_second"),
                "baseline_p99_ns": med(baseline, "p99_ns"), "labels_p99_ns": med("labels", "p99_ns"),
                "baseline_cpu_percent": med(baseline, "cpu_percent_one_core"),
                "labels_cpu_percent": med("labels", "cpu_percent_one_core"),
                "baseline_peak_rss_kib": med(baseline, "peak_rss_kib"),
                "labels_peak_rss_kib": med("labels", "peak_rss_kib")}));
        }
    }
    fs::write(root.join("comparison.json"), serde_json::to_vec_pretty(&comparisons).unwrap()).unwrap();
    println!("{}", serde_json::to_string_pretty(&comparisons).unwrap());
    accepted
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("--point") if args.len() == 4 => {
            let scenario = SCENARIOS.into_iter().find(|value| value.name == args[1]).expect("未知冻结场景");
            run_point(scenario, args[2].parse().expect("轮次格式错误"), Path::new(&args[3]), false);
        }
        Some("--smoke") if args.len() == 2 => {
            if std::env::var_os("PI_TCP_WQM_SMOKE_CHILD").is_some() {
                run_point(SCENARIOS[1], 0, Path::new(&args[1]), true);
            } else {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command.arg("--smoke").arg(&args[1]).env("PI_TCP_WQM_SMOKE_CHILD", "1");
                let child = process::run_child(&mut command, Duration::from_secs(20), "wqm-smoke");
                assert!(!child.timed_out && child.status.success(), "基准夹具检查失败: {}\n{}\n{}",
                    child.artifacts.display(), child.stdout, child.stderr);
                println!("夹具检查通过，非性能验收数据；进程组已清空：{}", child.artifacts.display());
            }
        }
        Some("--matrix") if args.len() == 4 => {
            assert!(cfg!(target_os = "linux"), "监控仅支持本轮Linux环境");
            let binaries = [fs::canonicalize(&args[1]).unwrap(), fs::canonicalize(&args[2]).unwrap()];
            let root = PathBuf::from(&args[3]);
            fs::create_dir(&root).expect("整批归档目录必须不存在");
            let mut rows = Vec::new();
            for round in 0..3 {
                for (index, scenario) in SCENARIOS.into_iter().enumerate() {
                    let order = if (round + index) % 2 == 0 { [0, 1] } else { [1, 0] };
                    for mode in order {
                        let point = root.join(format!("{}-{}-{round}", if mode == 1 { "on" } else { "off" }, scenario.name));
                        println!("开始：{}", point.display());
                        let mut command = Command::new(&binaries[mode]);
                        command.arg("--point").arg(scenario.name).arg(round.to_string()).arg(&point)
                            .env("PI_TCP_HIB_ARTIFACT_DIR", &root);
                        let child = process::run_child(&mut command, Duration::from_secs(120), "wqm-bench");
                        assert!(!child.timed_out && child.status.success(), "真实TCP基准点失败: {}\n{}\n{}",
                            child.artifacts.display(), child.stdout, child.stderr);
                        let row: Value = serde_json::from_slice(&fs::read(point.join("result.json")).unwrap()).unwrap();
                        assert_eq!(row["feature"], mode == 1, "必须使用不同feature的真实产物");
                        assert_eq!(row["smoke"], false);
                        assert_eq!(row["scenario"], scenario.name);
                        assert_eq!(row["measure_seconds"], MEASURE_SECONDS);
                        rows.push(row);
                        println!("完成：{}，CPU ticks={} 阶段停推={}，进程组已清空",
                            point.display(), child.cpu_ticks, child.stalled_after_probes);
                    }
                }
            }
            if !compare(&rows, &root) { eprintln!("超过既定5%观察线，暂停性能验收并分析"); std::process::exit(2); }
        }
        Some("--three-way" | "--cache-focus") if args.len() == 5 => {
            assert!(cfg!(target_os = "linux"));
            let focus = args[0] == "--cache-focus";
            let variants = if focus { ["scope", "inline", "labels"] } else { ["off", "scope", "labels"] };
            let scenarios = if focus { &SCENARIOS[3..] } else { &SCENARIOS[..] };
            let binaries = args[1..4].iter().map(|path| fs::canonicalize(path).unwrap()).collect::<Vec<_>>();
            let root = PathBuf::from(&args[4]);
            fs::create_dir(&root).expect("整批归档目录必须不存在");
            let mut rows = Vec::new();
            for round in 0..3 {
                for (index, scenario) in scenarios.iter().copied().enumerate() {
                    for position in 0..3 {
                        let mode = (round + index + position) % 3;
                        let point = root.join(format!("{}-{}-{round}", variants[mode], scenario.name));
                        println!("开始：{}", point.display());
                        let mut command = Command::new(&binaries[mode]);
                        command.arg("--point").arg(scenario.name).arg(round.to_string()).arg(&point);
                        let child = process::run_child(&mut command, Duration::from_secs(120), "wqm-three-way");
                        assert!(!child.timed_out && child.status.success(), "三产物真实TCP基准点失败: {}\n{}\n{}",
                            child.artifacts.display(), child.stdout, child.stderr);
                        let mut row: Value = serde_json::from_slice(&fs::read(point.join("result.json")).unwrap()).unwrap();
                        assert_eq!(row["feature"], focus || mode != 0);
                        assert_eq!(row["smoke"], false);
                        assert_eq!(row["scenario"], scenario.name);
                        assert_eq!(row["round"], round);
                        assert_eq!(row["warm_seconds"], WARM_SECONDS);
                        assert_eq!(row["measure_seconds"], MEASURE_SECONDS);
                        assert_eq!(row["errors"], 0);
                        assert_eq!(row["timeouts"], 0);
                        if mode == 2 { assert_eq!(row["encoding"], "data-point-labels"); }
                        if focus && mode > 0 { assert_eq!(row["encoding"], "data-point-labels"); }
                        if focus && mode == 2 { assert_eq!(row["label_storage"], "box"); }
                        row["variant"] = json!(variants[mode]);
                        rows.push(row);
                        println!("完成：{}，CPU ticks={} 阶段停推={}，进程组已清空",
                            point.display(), child.cpu_ticks, child.stalled_after_probes);
                    }
                }
            }
            let baselines: &[&str] = if focus { &["scope", "inline"] } else { &["off", "scope"] };
            if !compare_variants(&rows, &root, scenarios, baselines) {
                eprintln!("三产物比较超过既定5%观察线，暂停验收并分析");
                std::process::exit(2);
            }
        }
        _ => panic!("参数：--matrix <off> <on> <新目录>；--three-way <旧off> <旧Scope> <新标签> <新目录>；--cache-focus <旧Scope> <内联标签> <Box标签> <新目录>；--smoke <新目录>；--point仅供受监控子进程"),
    }
}
