//! WQM-B02真实TCP、官方OTLP导出及SDK记录耗时专项。
//! 被测connect_pool的冷准备/15秒采样/共享仪表；4个外部Local各带一个池，8个真实客户。
//! 官方导出器经回环gRPC发送原始协议，接收端只保存和应答，不补标签或数值。
//! 仅测试Provider用公开API包住实际SDK的record，计时不包含保存样本/打印；不访问私有锁。
//! 总耗时不是锁等待，也不能推算精确冲突率。可配合strace的futex/阶段标记观察睡眠等待。
//! 本专项不属于正式吞吐矩阵；独占子进程、100秒硬截止、串行，结束清空进程组。

#![cfg(all(target_os = "linux", feature = "write_queue_metrics"))]

#[path = "support/hibernate_process.rs"]
mod process;
#[path = "support/write_queue_metrics_network.rs"]
mod network;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::process::Command;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};
use opentelemetry::{global, InstrumentationScope, KeyValue,
    metrics::{Gauge, InstrumentBuilder, InstrumentProvider, Meter, MeterProvider, SyncInstrument,
        noop::NoopMeterProvider}};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_proto::tonic::{collector::metrics::v1::{ExportMetricsServiceRequest,
    ExportMetricsServiceResponse, metrics_service_server::{MetricsService, MetricsServiceServer}},
    common::v1::{KeyValue as WireKeyValue, any_value}, metrics::v1::{metric, number_data_point}};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use serde_json::{json, Value};
use network::{Client, QueuePoint, Server, Workers};

/// 测试专用公开API适配器。只装饰目标来源；其它来源保持原SDK能力与身份。
struct TimedProvider { sdk: SdkMeterProvider, samples: Arc<Mutex<Vec<Value>>>, trace: Option<Arc<File>> }

impl MeterProvider for TimedProvider {
    fn meter_with_scope(&self, scope: InstrumentationScope) -> Meter {
        let timed = scope.name() == "pi_tcp.connection_pool";
        let meter = self.sdk.meter_with_scope(scope);
        if !timed { return meter; }
        Meter::new(Arc::new(TimedInstruments { meter, samples: self.samples.clone(), trace: self.trace.clone() }))
    }
}

struct TimedInstruments { meter: Meter, samples: Arc<Mutex<Vec<Value>>>, trace: Option<Arc<File>> }

impl InstrumentProvider for TimedInstruments {
    /// 原样转交本来源唯一使用的u64 Gauge参数，不改变真实SDK缓存/聚合/属性语义。
    fn u64_gauge(&self, builder: InstrumentBuilder<'_, Gauge<u64>>) -> Gauge<u64> {
        assert_eq!(builder.name, "pi_tcp.write_queue.messages");
        let mut real = self.meter.u64_gauge(builder.name);
        if let Some(description) = builder.description { real = real.with_description(description); }
        if let Some(unit) = builder.unit { real = real.with_unit(unit); }
        Gauge::new(Arc::new(TimedGauge { real: real.build(), samples: self.samples.clone(),
            calls: AtomicUsize::new(0), trace: self.trace.clone() }))
    }
}

struct TimedGauge { real: Gauge<u64>, samples: Arc<Mutex<Vec<Value>>>, calls: AtomicUsize, trace: Option<Arc<File>> }

impl SyncInstrument<u64> for TimedGauge {
    /// 只计实际SDK调用。打印与结果锁均在计时之外；会扰动调度，不能当作正式性能结果。
    fn measure(&self, value: u64, attributes: &[KeyValue]) {
        let ordinal = self.calls.fetch_add(1, Ordering::Relaxed);
        let target = attributes.iter().find(|kv| kv.key.as_str() == "target")
            .expect("生产记录缺target").value.as_str();
        // 直接写测试专用文件，不使用标准输出的进程共享锁；阶段标记不进入计时区间。
        if let Some(file) = &self.trace {
            (&**file).write_all(format!("WQM_RECORD_BEGIN {target} {ordinal}\n").as_bytes()).expect("测试开始标记写入失败");
        }
        let began = Instant::now();
        self.real.record(value, attributes);
        let elapsed = began.elapsed().as_nanos();
        if let Some(file) = &self.trace {
            (&**file).write_all(format!("WQM_RECORD_END {target} {ordinal}\n").as_bytes()).expect("测试结束标记写入失败");
        }
        self.samples.lock().expect("测试耗时结果锁失败").push(json!({
            "target": target, "ordinal": ordinal, "cold": ordinal == 0,
            "elapsed_ns": elapsed, "value": value,
        }));
    }
}

/// 原始OTLP接收端；有界缓存满则明确返回失败，不丢数据或伪造导出成功。
struct Collector { requests: Sender<ExportMetricsServiceRequest> }

#[tonic::async_trait]
impl MetricsService for Collector {
    async fn export(&self, request: tonic::Request<ExportMetricsServiceRequest>)
        -> Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
        self.requests.try_send(request.into_inner())
            .map_err(|error| tonic::Status::resource_exhausted(error.to_string()))?;
        Ok(tonic::Response::new(ExportMetricsServiceResponse { partial_success: None }))
    }
}

/// 转换协议中的原始身份以便断言；明确保留PID整数类型检查，不依赖后端属性提升。
fn attributes(values: &[WireKeyValue]) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for item in values {
        let value = item.value.as_ref().and_then(|value| value.value.as_ref()).expect("空协议属性");
        let text = match value {
            any_value::Value::StringValue(value) => value.clone(),
            any_value::Value::IntValue(value) if item.key == "process.pid" => value.to_string(),
            other => panic!("协议身份类型错误: {other:?}"),
        };
        if item.key == "process.pid" { assert!(matches!(value, any_value::Value::IntValue(_))); }
        assert!(result.insert(item.key.clone(), text).is_none(), "协议中不能有重复身份键");
    }
    result
}

/// 同一原始报告至多一个队列Metric，各池必须为普通标签数据点；原控制指标仍完整。
fn inspect(request: &ExportMetricsServiceRequest) -> Vec<QueuePoint> {
    let mut points = Vec::new();
    let mut count = 0;
    let mut control = false;
    for resource in &request.resource_metrics {
        for scope in &resource.scope_metrics {
            for metric in &scope.metrics {
                if metric.name == "wqm.otlp.control" {
                    let Some(metric::Data::Gauge(gauge)) = &metric.data else { panic!("控制类型改变") };
                    assert_eq!(gauge.data_points.len(), 1);
                    assert_eq!(gauge.data_points[0].value, Some(number_data_point::Value::AsInt(42)));
                    assert_eq!(attributes(&gauge.data_points[0].attributes),
                        BTreeMap::from([("existing".into(), "unchanged".into())]));
                    control = true;
                }
                if metric.name != "pi_tcp.write_queue.messages" { continue; }
                count += 1;
                let source = scope.scope.as_ref().expect("缺来源");
                assert_eq!(source.name, "pi_tcp.connection_pool");
                assert_eq!(source.version, env!("CARGO_PKG_VERSION"));
                assert!(source.attributes.is_empty(), "不能依赖Scope中的池身份");
                assert_eq!(metric.unit, "{message}");
                let Some(metric::Data::Gauge(gauge)) = &metric.data else { panic!("必须是Gauge") };
                for point in &gauge.data_points {
                    let attrs = attributes(&point.attributes);
                    assert_eq!(attrs.len(), 4);
                    assert_eq!(attrs["process.pid"], std::process::id().to_string());
                    assert_eq!(attrs["target"], format!("{}:{}:write-queue-{}",
                        attrs["process.pid"], attrs["rt.name"], attrs["pi_tcp.pool.id"]));
                    let Some(number_data_point::Value::AsInt(value)) = point.value else { panic!("队列数量非整数") };
                    points.push(QueuePoint { attributes: attrs, value: u64::try_from(value).expect("队列数量为负") });
                }
            }
        }
    }
    assert!(control, "原有控制仪表必须真实上传");
    assert!(count <= 1, "全部池应使用同一指标");
    points.sort_unstable_by(|left, right| left.attributes["target"].cmp(&right.attributes["target"]));
    points
}

/// 逐池验证三次真实15秒采样、实际OTLP身份及关闭后不再record；有界进程防等待失控。
#[test]
fn test_write_queue_metrics_real_otlp_and_record_cost() {
    if std::env::var_os("PI_TCP_WQM_OTLP_CHILD").is_some() { run_case(); return; }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "test_write_queue_metrics_real_otlp_and_record_cost", "--nocapture", "--test-threads=1"])
        .env("PI_TCP_WQM_OTLP_CHILD", "1");
    let result = process::run_child(&mut command, Duration::from_secs(100), "wqm-otlp");
    assert!(!result.timed_out && result.status.success(), "OTLP/耗时专项失败: {}\n{}\n{}",
        result.artifacts.display(), result.stdout, result.stderr);
    println!("{}\nOTLP/耗时专项通过，进程组已清空：{}", result.stdout, result.artifacts.display());
}

/// 测试宿主拥有Provider/采集/运输；工作负载、探针及清理都不进入生产代码。
fn run_case() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2)
        .thread_name("wqm-otlp-host").enable_all().build().expect("建立OTLP宿主运行时失败");
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).expect("绑定OTLP端口失败");
    let address = listener.local_addr().unwrap();
    let (requests_tx, requests_rx) = bounded(128);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let incoming = futures::stream::unfold(listener, |listener| async move {
        Some((listener.accept().await.map(|(stream, _)| stream), listener))
    });
    let service = MetricsServiceServer::new(Collector { requests: requests_tx });
    let serving = runtime.spawn(tonic::transport::Server::builder()
        .serve_with_incoming_shutdown(service, incoming, async { let _ = stop_rx.await; }));
    let exporter = {
        let _entered = runtime.enter();
        opentelemetry_otlp::MetricExporter::builder().with_tonic()
            .with_endpoint(format!("http://{address}")).with_timeout(Duration::from_secs(3))
            .build().expect("建立官方OTLP导出器失败")
    };
    let reader = PeriodicReader::builder(exporter).with_interval(Duration::from_secs(3600)).build();
    let sdk = SdkMeterProvider::builder().with_reader(reader).build();
    let samples = Arc::new(Mutex::new(Vec::new()));
    let trace = std::env::var_os("PI_TCP_WQM_TRACE").map(|path| Arc::new(File::options()
        .write(true).append(true).create_new(true).open(path).expect("创建独立阶段标记文件失败")));
    global::set_meter_provider(TimedProvider { sdk: sdk.clone(), samples: samples.clone(), trace });
    let control = sdk.meter("wqm.otlp.control").u64_gauge("wqm.otlp.control").build();
    control.record(42, &[KeyValue::new("existing", "unchanged")]);
    let workers = Workers::start(4);
    let server = Server::start(4, false, workers.sender.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let clients = (0..8).map(|index| {
        let stop = stop.clone();
        let port = server.port;
        std::thread::spawn(move || {
            let mut client = Client::connect(port, index);
            let mut done = 0u64;
            while !stop.load(Ordering::Acquire) { client.round_trip(); done += 1; }
            done
        })
    }).collect::<Vec<_>>();
    for _ in 0..8 { server.connected.recv_timeout(Duration::from_secs(3)).expect("缺少真实建连"); }
    let started = Instant::now();
    let mut latest = Vec::new();
    while started.elapsed() < Duration::from_secs(47) {
        sdk.force_flush().expect("官方OTLP采集失败");
        let request = requests_rx.recv_timeout(Duration::from_secs(4)).expect("没有收到实际OTLP请求");
        let points = inspect(&request);
        if started.elapsed() < Duration::from_secs(1) { assert!(points.is_empty(), "不能虚构初始值"); }
        if points.len() == 4 { server.check_points(&points, 8); latest = points; }
        assert!(requests_rx.is_empty(), "一轮显式采集只能发送一份报告");
        assert!(server.errors.is_empty(), "真实TCP服务端发生错误");
        std::thread::sleep(Duration::from_millis(100));
    }
    server.check_points(&latest, 8);
    stop.store(true, Ordering::Release);
    let finished = clients.into_iter().map(|client| client.join()).collect::<Vec<_>>();
    let exchanges: u64 = finished.into_iter().map(|result| result.expect("真实客户失败")).sum();
    assert_eq!(workers.finish(), exchanges);
    server.finish();
    let count = samples.lock().unwrap().len();
    sdk.force_flush().expect("关闭后的OTLP采集失败");
    assert_eq!(inspect(&requests_rx.recv_timeout(Duration::from_secs(4)).unwrap()), latest);
    std::thread::sleep(Duration::from_millis(200));
    let recorded = samples.lock().unwrap();
    assert_eq!(recorded.len(), count, "连接池关闭后不能继续record");
    let mut per_pool = BTreeMap::<String, Vec<usize>>::new();
    for sample in recorded.iter() {
        per_pool.entry(sample["target"].as_str().unwrap().to_owned()).or_default()
            .push(sample["ordinal"].as_u64().unwrap() as usize);
    }
    assert_eq!(per_pool.len(), 4);
    for ordinals in per_pool.values_mut() {
        ordinals.sort_unstable();
        assert_eq!(*ordinals, vec![0, 1, 2], "每池应有三次真实15秒采样，不补采/多采");
    }
    println!("{}", json!({"kind": "wqm-record-cost", "samples": *recorded, "exchanges": exchanges,
        "last_points": latest.iter().map(|point| json!({"attributes": point.attributes, "value": point.value})).collect::<Vec<_>>(),
        "meaning": "实际SDK调用总耗时，不等于纯锁等待或精确冲突率"}));
    drop(recorded);
    global::set_meter_provider(NoopMeterProvider::new());
    sdk.shutdown().expect("OTLP宿主关闭失败");
    stop_tx.send(()).expect("OTLP接收端提前退出");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(4), serving).await.expect("接收端关闭超时")
            .expect("接收端任务失败").expect("接收端传输失败");
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
}
