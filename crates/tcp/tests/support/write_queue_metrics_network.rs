//! WQM-B02的真实TCP测试装配，供网络专项与开关对照基准复用。
//!
//! 客户端发送256字节帧，所属Local消费真实接收缓冲，外部OS线程通过原
//! SocketHandle::write_ready提交同一帧。每连接最多一个请求在途，不访问Context，
//! 不模拟生产发送通道、采样时钟或关闭。不支持TLS；本模块只在受监控子进程使用。
//! SDK是测试宿主：先安装Provider，再绑定原监听器；官方内存导出器观察真实仪表，
//! 不向SDK提供访问连接池的回调。恢复全局Provider仅发生在独占测试进程退出前。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use futures::future::{FutureExt, LocalBoxFuture};
use opentelemetry::{global, KeyValue, metrics::{Gauge, MeterProvider, noop::NoopMeterProvider}};
use opentelemetry_sdk::metrics::{data::{AggregatedMetrics, MetricData},
    InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use pi_tcp::{AsyncService, SocketConfig, SocketHandle, SocketStatus,
    connect::TcpSocket, server::{PortsAdapterFactory, SocketListener}};

pub const FRAME_BYTES: usize = 256;
pub const FEATURE_ENABLED: bool = cfg!(all(feature = "write_queue_metrics", not(target_arch = "wasm32")));

/// 一个实际SDK Gauge点；只保留不可变结果，不导出SDK或生产状态的可写能力。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePoint {
    pub attributes: BTreeMap<String, String>,
    pub value: u64,
}

/// 测试进程独占Provider；固定控制仪表用于核对共享仪表没有污染既有来源/标签。
pub struct Host {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    control: Gauge<u64>,
}

impl Host {
    /// 冷安装官方SDK，周期线程设为一小时，测试主线程显式flush；没有自定义回调。
    /// 有SDK线程/锁/分配副作用，非幂等，必须在本子进程所有监听器构造前调用一次。
    pub fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).with_interval(Duration::from_secs(3600)).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        global::set_meter_provider(provider.clone());
        let control = provider.meter("wqm.host.control").u64_gauge("wqm.host.control").build();
        Self { provider, exporter, control }
    }

    /// 在宿主线程采集真实报告并清空导出器缓存，避免基准逐秒积累报告。
    /// 有同步SDK等待，不在连接池线程调用；每轮必须保留原控制点和值/标签。
    /// 返回队列点不是新增采样次数，SDK可以重复导出旧Gauge；O(本轮报告点数)。
    pub fn collect(&self) -> Vec<QueuePoint> {
        self.control.record(42, &[KeyValue::new("existing", "unchanged")]);
        self.exporter.reset();
        self.provider.force_flush().expect("测试宿主采集失败");
        let reports = self.exporter.get_finished_metrics().expect("读取官方SDK报告失败");
        let mut points = Vec::new();
        let mut control_seen = false;
        let mut queue_metrics = 0usize;
        for report in reports {
            for scope in report.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() == "wqm.host.control" {
                        assert_eq!(scope.scope().name(), "wqm.host.control");
                        assert_eq!(scope.scope().attributes().count(), 0);
                        let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data() else {
                            panic!("原控制仪表类型改变")
                        };
                        let values = gauge.data_points().collect::<Vec<_>>();
                        assert_eq!(values.len(), 1);
                        assert_eq!(values[0].value(), 42);
                        let attrs = values[0].attributes().map(|kv| {
                            (kv.key.as_str().to_owned(), kv.value.as_str().into_owned())
                        }).collect::<BTreeMap<_, _>>();
                        assert_eq!(attrs, BTreeMap::from([("existing".to_string(), "unchanged".to_string())]));
                        control_seen = true;
                    }
                    if metric.name() != "pi_tcp.write_queue.messages" { continue; }
                    assert_eq!(scope.scope().name(), "pi_tcp.connection_pool");
                    assert_eq!(scope.scope().version(), Some(env!("CARGO_PKG_VERSION")));
                    assert_eq!(scope.scope().attributes().count(), 0, "池身份必须位于普通数据点标签");
                    assert_eq!(metric.unit(), "{message}");
                    queue_metrics += 1;
                    let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data() else {
                        panic!("发送队列必须是u64 Gauge")
                    };
                    for point in gauge.data_points() {
                        let attrs = point.attributes().map(|kv| {
                            (kv.key.as_str().to_owned(), kv.value.as_str().into_owned())
                        }).collect::<BTreeMap<_, _>>();
                        assert_eq!(attrs.len(), 4);
                        assert_eq!(attrs["process.pid"], std::process::id().to_string());
                        assert_eq!(attrs["target"], format!("{}:{}:write-queue-{}",
                            std::process::id(), attrs["rt.name"], attrs["pi_tcp.pool.id"]));
                        assert!(attrs["pi_tcp.pool.id"].parse::<usize>().expect("池编号非整数") > 0);
                        points.push(QueuePoint { attributes: attrs, value: point.value() });
                    }
                }
            }
        }
        assert!(control_seen, "原控制指标必须仍可用");
        assert!(queue_metrics <= 1, "全部池必须复用同一来源下的同一指标仪表");
        points.sort_unstable_by(|left, right| left.attributes["target"].cmp(&right.attributes["target"]));
        points
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        global::set_meter_provider(NoopMeterProvider::new());
        if let Err(error) = self.provider.shutdown() {
            if !thread::panicking() { panic!("测试宿主关闭失败: {error}"); }
        }
    }
}

type SendJob = (SocketHandle<TcpSocket>, Vec<u8>);

/// 应用层外部发送线程；有界应用队列不是被观测的生产发送队列。
pub struct Workers {
    pub sender: Sender<SendJob>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<u64>>,
}

impl Workers {
    /// 固定1～8个线程，只调用原公开发送API；错误必须使该点失败，绝不补发回包。
    pub fn start(count: usize) -> Self {
        assert!((1..=8).contains(&count));
        let (sender, receiver) = bounded::<SendJob>(256);
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..count).map(|index| {
            let receiver = receiver.clone();
            let stop = stop.clone();
            thread::Builder::new().name(format!("wqm-send-{index}")).spawn(move || {
                let mut submitted = 0u64;
                while !stop.load(Ordering::Acquire) {
                    match receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok((handle, bytes)) => {
                            handle.write_ready(bytes).expect("外部线程提交真实回包失败");
                            submitted = submitted.checked_add(1).expect("发送计数溢出");
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
                assert!(receiver.is_empty(), "客户完成后不应还有应用发送任务");
                submitted
            }).expect("启动外部发送线程失败")
        }).collect();
        Self { sender, stop, threads }
    }

    /// 必须先完成所有客户请求。停止并join全部线程后才检查错误，避免遗留后台任务。
    pub fn finish(self) -> u64 {
        self.stop.store(true, Ordering::Release);
        let results = self.threads.into_iter().map(|thread| thread.join()).collect::<Vec<_>>();
        results.into_iter().map(|value| value.expect("外部发送线程异常")).sum()
    }
}

struct EchoService {
    jobs: Sender<SendJob>,
    connected: Sender<usize>,
    errors: Sender<String>,
}

impl AsyncService<TcpSocket> for EchoService {
    fn handle_connected(&self, handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let connected = self.connected.clone();
        async move {
            assert!(matches!(status, SocketStatus::Connected(Ok(()))), "真实建连失败: {status:?}");
            connected.try_send(handle.get_uid()).expect("测试建连通知容量不足");
        }.boxed_local()
    }

    fn handle_readed(&self, handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let jobs = self.jobs.clone();
        let errors = self.errors.clone();
        async move {
            if !matches!(status, SocketStatus::Readed(Ok(_))) {
                errors.try_send(format!("读取失败: {status:?}")).expect("测试错误通道溢出");
                return;
            }
            let frames = {
                let buffer = handle.get_read_buffer();
                // 安全边界：只在该连接所属Local的同步回调中短借用；中间无await、
                // 无用户回调或重入。跨线程只传公开SocketHandle和独立Vec，不传Rc/引用。
                let buffer = unsafe { &mut *buffer.get() };
                let mut frames = Vec::new();
                if let Some(bytes) = buffer.as_mut() {
                    while bytes.len() >= FRAME_BYTES { frames.push(bytes.split_to(FRAME_BYTES).to_vec()); }
                }
                frames
            };
            for frame in frames {
                jobs.try_send((handle.clone(), frame)).expect("测试应用队列溢出或线程退出");
            }
        }.boxed_local()
    }

    fn handle_writed(&self, _handle: SocketHandle<TcpSocket>, status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        let errors = self.errors.clone();
        async move {
            if !matches!(status, SocketStatus::Writed(Ok(_))) {
                errors.try_send(format!("写入失败: {status:?}")).expect("测试错误通道溢出");
            }
        }.boxed_local()
    }

    fn handle_closed(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        // 正常客户退出后的EOF不作为测量期错误；测量结束前由客户逐帧断言连接有效。
        async {}.boxed_local()
    }

    fn handle_timeouted(&self, _handle: SocketHandle<TcpSocket>, _status: SocketStatus) -> LocalBoxFuture<'static, ()> {
        async {}.boxed_local()
    }
}

/// 只持有原监听器的关闭能力和不可变身份；没有替生产取消Future/解除Context。
pub struct Server {
    pub port: u16,
    pub runtime_names: Vec<String>,
    pub connected: Receiver<usize>,
    pub errors: Receiver<String>,
    close: Option<Box<dyn FnOnce()>>,
    owner_names: Vec<String>,
}

impl Server {
    /// pools为1～4；shared=true时把同一外部Local的Clone注入多个池，验证池身份隔离。
    /// 原生Local由测试宿主创建/推进，pi_tcp仍在原run中冷登记，不手工代登记。
    /// None保持原Poll配置；临时端口竞争导致的bind错误直接失败，不冒充产品红灯。
    pub fn start(pools: usize, shared: bool, jobs: Sender<SendJob>) -> Self {
        assert!((1..=4).contains(&pools));
        let port = TcpListener::bind("127.0.0.1:0").expect("选择测试端口失败").local_addr().unwrap().port();
        let owner_count = if shared { 1 } else { pools };
        let owner_names = (0..owner_count).map(|index| format!("wqm-owner-{index}")).collect::<Vec<_>>();
        let owners = owner_names.iter().map(|name| AsyncRuntimeBuilder::custom_local_thread(
            Some(name.as_str()), Some(2 * 1024 * 1024), None, Some(0), Some(Duration::from_millis(1)),
        )).collect::<Vec<_>>();
        let runtimes = (0..pools).map(|index| owners[if shared { 0 } else { index }].clone()).collect::<Vec<_>>();
        let runtime_names = runtimes.iter().map(|runtime| format!("pi_tcp-local-{}", runtime.get_id())).collect();
        let (connected_tx, connected) = bounded(256);
        let (errors_tx, errors) = bounded(256);
        let mut factory = PortsAdapterFactory::<TcpSocket>::new();
        factory.bind(port, Box::new(EchoService { jobs, connected: connected_tx, errors: errors_tx }));
        let mut config = SocketConfig::new("127.0.0.1", &[port]);
        config.set_option(64 * 1024, 64 * 1024, 64 * 1024, 64);
        let listener = SocketListener::try_bind(runtimes, factory, config, 16, 2 * 1024 * 1024,
            256, 64, 4096, 4096, None).expect("启动原TCP监听器失败");
        Self { port, runtime_names, connected, errors, owner_names,
            close: Some(Box::new(move || { listener.close(Ok(())); drop(owners); })) }
    }

    /// 验证收到的是本次真实池/运行时的普通标签且不存在覆盖；允许真实瞬时长度为零。
    /// 最大值由每连接一个在途请求的负载约束给出，不拿它当生产队列容量。
    pub fn check_points(&self, points: &[QueuePoint], max_in_flight: usize) {
        if !FEATURE_ENABLED { assert!(points.is_empty(), "关闭feature不得产生队列仪表"); return; }
        assert_eq!(points.len(), self.runtime_names.len());
        let mut names = points.iter().map(|point| {
            assert!(point.value <= max_in_flight as u64);
            point.attributes["rt.name"].clone()
        }).collect::<Vec<_>>();
        names.sort_unstable();
        let mut expected = self.runtime_names.clone();
        expected.sort_unstable();
        assert_eq!(names, expected, "必须使用实际Local的原登记名");
        let mut ids = points.iter().map(|point| point.attributes["pi_tcp.pool.id"].clone()).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), points.len(), "同一核心的多个池也不得覆盖");
    }

    /// 只调用原close并观察owner线程自然退出；不代补生产清理，也不承诺原连接资源回收。
    /// Linux测试进程中同步最多等待3秒；外层监控负责进程/全部残留线程的最终截止。
    pub fn finish(mut self) {
        self.close.take().expect("重复测试关闭")();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let found = std::fs::read_dir("/proc/self/task").expect("读取测试线程失败")
                .filter_map(Result::ok).filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
                .any(|name| self.owner_names.iter().any(|owner| owner == name.trim()));
            if !found { break; }
            assert!(Instant::now() < deadline, "原close后连接池owner未退出");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) { if let Some(close) = self.close.take() { close(); } }
}

/// 真实固定帧客户；一次最多一个请求，逐字节校验连接编号、序号和内容。
pub struct Client {
    stream: TcpStream,
    connection: u64,
    sequence: u64,
}

impl Client {
    /// 3秒连接/读/写截止，禁止无界网络等待；调用在外部客户线程，不在Local回调。
    pub fn connect(port: u16, connection: u64) -> Self {
        let stream = TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_secs(3)).expect("连接真实TCP失败");
        stream.set_nodelay(true).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        Self { stream, connection, sequence: 0 }
    }

    /// 同步真实往返；发生网络错误/跨连接混淆立即失败，不把提交成功计作收到。
    pub fn round_trip(&mut self) {
        let mut frame = [0u8; FRAME_BYTES];
        frame[..8].copy_from_slice(&self.connection.to_le_bytes());
        frame[8..16].copy_from_slice(&self.sequence.to_le_bytes());
        for (index, byte) in frame[16..].iter_mut().enumerate() { *byte = (index as u64 ^ self.connection ^ self.sequence) as u8; }
        self.stream.write_all(&frame).expect("真实TCP请求写入失败");
        let mut reply = [0; FRAME_BYTES];
        self.stream.read_exact(&mut reply).expect("真实TCP回包读取失败");
        assert_eq!(reply, frame, "完整帧必须保序且不能跨连接混淆");
        self.sequence = self.sequence.checked_add(1).expect("客户序号溢出");
    }
}
