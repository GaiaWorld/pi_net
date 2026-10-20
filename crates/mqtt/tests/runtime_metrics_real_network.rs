#![cfg(not(target_arch = "wasm32"))]

//! MQTT最小运行时登记的真实网络/专项验证，对应冻结D15及NET-MET-V01/V02/V05/V08。
//!
//! 被测入口是broker.rs及quic_broker.rs的原单例初始化块；只通过公开协议工厂触发。
//! 每个场景独立子进程，真实TCP/WebSocket/MQTT交互后再由宿主同步采集并真实OTLP导出。
//! 不执行test.rs，不手动登记MQTT核心，不安装生产Provider或替生产补控制/清理能力。
//! 默认/关闭feature使用完全相同网络断言；容量边界仅用无OS线程的普通Single及公开登记API。
//! QUIC仅验证协议构造触发的第二私有Worker登记，不宣称已测试QUIC/TLS网络协议。
//! 所有case串行，整例60秒截止；不是压力、基准或整个指标交付的最终验收。
//! 执行时须显式传入--features pi-async-rt/trace及--ignored，普通构建不隐式开启上报。

#[path = "support/runtime_metrics_transport.rs"]
mod transport;

use mqtt311::{Connect, Connack, ConnectReturnCode, MqttRead, MqttWrite, Packet,
    PacketIdentifier, Protocol, Publish, QoS, Suback, Subscribe, SubscribeReturnCodes, SubscribeTopic};
use pi_async_rt::{rt::{single_thread::SingleTaskRunner, serial::AsyncRuntimeBuilder}, runtime_metrics::*};
use pi_mqtt::{quic_v311::QuicMqtt311, tls_v311::WssMqtt311, v311::WsMqtt311,
    server::{get_broker, MqttBrokerProtocol, WsMqttBrokerFactory}};
use opentelemetry_proto::tonic::{collector::metrics::v1::ExportMetricsServiceRequest,
    common::v1::any_value, metrics::v1::{metric, number_data_point}};
use std::{collections::{BTreeMap, BTreeSet}, io::{Cursor, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio}, sync::{Arc, Mutex}, thread, time::{Duration, Instant}};
use tcp::{connect::TcpSocket, server::{PortsAdapterFactory, SocketListener}, SocketConfig};
use transport::OtlpHost;
use ws::{frame::WsHead, server::WebsocketListener};

const CHILD_ENV: &str = "PI_MQTT_RUNTIME_METRICS_CHILD";
const PROCESS: &str = "pi_mqtt-回环专项宿主";
const IO_LIMIT: Duration = Duration::from_secs(3);

/// 独占子进程身份；不按裸PID操作其它进程。正常/异常均只回收本测试创建的子进程。
struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) { return; }
        let _ = self.0.kill();
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// 父进程只执行指定独立test，子进程独占注册表/Provider/时钟/端口；不改父进程环境。
/// 返回true表示当前为对应子进程，调用方随后执行真实场景；其它情况等待有界退出。
/// 非纯、非幂等，O(1)进程设施；子进程失败或迟到都使父测试失败，不能采纳迟到成功。
fn is_child(test: &str) -> bool {
    if let Some(value) = std::env::var_os(CHILD_ENV) {
        assert_eq!(value.as_os_str(), std::ffi::OsStr::new(test));
        return true;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test, "--ignored", "--test-threads=1", "--nocapture"])
        .env(CHILD_ENV, test).stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    // 只移除子进程环境中的OTEL配置，避免外部定时器/属性设置改变专项证据。
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("OTEL_") { command.env_remove(key); }
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut child = OwnedChild(command.spawn().unwrap());
    loop {
        assert!(Instant::now() < deadline, "MQTT专项子进程超过60秒截止");
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(Instant::now() < deadline && status.success(), "MQTT专项失败：{}", status);
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// 真实MQTT网络所有者。Drop仅关闭本测试socket/listener，不能替生产登记或注销。
/// listener没有绑定0后返回端口的接口，短暂预约端口后重绑有竞争风险，绑定失败必须失败。
struct MqttNetwork {
    listener: Option<SocketListener<TcpSocket, PortsAdapterFactory<TcpSocket>>>,
    client: Option<TcpStream>,
    broker: String,
}

impl MqttNetwork {
    /// 通过原公开工厂创建代理并启动真实WebSocket/MQTT服务，触发原公共Worker初始化。
    /// 无入参，返回独占清理句柄；有网络/线程/原过期循环副作用，不在测试侧补Ready或重投。
    fn new() -> Self {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let broker = "runtime-metrics-broker".to_owned();
        let factory = WsMqttBrokerFactory::new("mqtt", &broker, port, false);
        let mut adapters = PortsAdapterFactory::<TcpSocket>::new();
        adapters.bind(port, Box::new(WebsocketListener::with_protocol(factory.new_child_protocol(false))));
        let mut config = SocketConfig::new("127.0.0.1", &[port]);
        config.set_option(16384, 16384, 16384, 16);
        let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
        let listener = SocketListener::try_bind(vec![rt], adapters, config,
            16, 2 * 1024 * 1024, 64, 16, 4096, 4096, Some(10)).expect("绑定MQTT监听器失败");
        let mut network = Self { listener: Some(listener), client: None, broker };
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let client = TcpStream::connect_timeout(&addr, IO_LIMIT).expect("连接MQTT回环服务失败");
        client.set_read_timeout(Some(IO_LIMIT)).unwrap();
        client.set_write_timeout(Some(IO_LIMIT)).unwrap();
        network.client = Some(client);
        let client = network.client.as_mut().unwrap();
        client.write_all(format!("GET /mqtt HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: mqtt\r\n\r\n", port).as_bytes()).unwrap();
        let mut bytes = Vec::new();
        let deadline = Instant::now() + IO_LIMIT;
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 8192 && Instant::now() < deadline, "握手头超长或超时");
            let mut byte = [0];
            client.read_exact(&mut byte).expect("读取WebSocket握手失败");
            bytes.push(byte[0]);
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut response = httparse::Response::new(&mut headers);
        assert!(response.parse(&bytes).unwrap().is_complete());
        assert_eq!(response.code, Some(101));
        let accept = response.headers.iter().find(|header| header.name.eq_ignore_ascii_case("sec-websocket-accept"));
        assert_eq!(accept.map(|header| header.value), Some(&b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="[..]));
        network
    }

    /// 使用mqtt311原编码和WsHead原帧头编码发送短二进制帧，掩码固定以保持测试可复现。
    /// 仅支持本专项的小消息；O(n)编码/掩码及IO，3秒写截止，不承担通用WebSocket客户端职责。
    fn send(&mut self, packet: &Packet) {
        let mut payload = Cursor::new(Vec::new());
        payload.write_packet(packet).unwrap();
        let mut payload = payload.into_inner();
        assert!(payload.len() <= 4096);
        let key = [0x29, 0x6c, 0x10, 0x73];
        let mut head = WsHead::default();
        head.set_finish(true);
        head.set_binary();
        head.set_len(payload.len());
        head.set_key(Some(key.to_vec()));
        for (index, byte) in payload.iter_mut().enumerate() { *byte ^= key[index % 4]; }
        let client = self.client.as_mut().unwrap();
        client.write_all(&Vec::from(head)).unwrap();
        client.write_all(&payload).unwrap();
    }

    /// 只接受本专项短的、未掩码、未压缩完整二进制响应；MQTT仍由原mqtt311解码。
    /// WsHead::from只面向服务端六字节带掩码上行头，不能当作通用下行解析器使用。
    /// 此处固定帧形状独立检查两字节头，拒绝扩展长度/分片/额外报文，不实现通用协议引擎。
    /// O(n)空间/时间，n<126，socket有3秒截止，整个子进程另有60秒上限。
    fn receive(&mut self) -> Packet {
        let client = self.client.as_mut().unwrap();
        let mut bytes = [0; 2];
        client.read_exact(&mut bytes).expect("未收到真实MQTT响应");
        assert_eq!(bytes[0], 0x82, "必须为无扩展的完整二进制帧");
        assert!(bytes[1] < 126, "服务端不得掩码或使用专项不支持的扩展长度");
        let mut payload = vec![0; bytes[1] as usize];
        client.read_exact(&mut payload).unwrap();
        let mut input = payload.as_slice();
        let packet = input.read_packet().expect("MQTT报文解码失败");
        assert!(input.is_empty(), "单次响应必须只有一个完整MQTT报文");
        packet
    }

    /// 真实CONNECT/CONNACK、SUBSCRIBE/SUBACK、QoS0发布回显、PING闭环。
    /// 精确验证响应内容及权威会话表，不以connect/派发返回Ok充当业务成功。
    fn exercise(&mut self) {
        self.send(&Packet::Connect(Connect { protocol: Protocol::MQTT(4), keep_alive: 60,
            client_id: "metrics-client".into(), clean_session: true,
            last_will: None, username: None, password: None }));
        assert_eq!(self.receive(), Packet::Connack(Connack {
            session_present: false, code: ConnectReturnCode::Accepted }));
        let broker = match get_broker(&self.broker).unwrap() {
            MqttBrokerProtocol::WsMqtt311(broker) => broker,
            _ => panic!("必须使用真实WS MQTT代理"),
        };
        assert_eq!(broker.get_broker().session_size(), 1);
        assert!(broker.get_broker().is_exist_session(&"metrics-client".to_owned()));
        self.send(&Packet::Subscribe(Subscribe { pkid: PacketIdentifier(7), topics: vec![SubscribeTopic {
            topic_path: "metrics/loopback".into(), qos: QoS::AtMostOnce }] }));
        assert_eq!(self.receive(), Packet::Suback(Suback { pkid: PacketIdentifier(7),
            return_codes: vec![SubscribeReturnCodes::Success(QoS::AtMostOnce)] }));
        let publish = Packet::Publish(Publish { dup: false, qos: QoS::AtMostOnce, retain: false,
            topic_name: "metrics/loopback".into(), pkid: None, payload: Arc::new("真实MQTT指标装配".as_bytes().to_vec()) });
        self.send(&publish);
        assert_eq!(self.receive(), publish);
        self.send(&Packet::Pingreq);
        assert_eq!(self.receive(), Packet::Pingresp);
    }
}

impl Drop for MqttNetwork {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() { let _ = client.shutdown(Shutdown::Both); }
        if let Some(listener) = self.listener.take() { listener.close(Ok(())); }
    }
}

/// 从允许同步等待且不推进目标的宿主测试线程采集，不新增生产采集循环。
fn collect() -> RuntimeMetricCollectionReport {
    collect_and_report_runtime_metrics(RuntimeMetricCollectionOptions::new(
        Duration::from_secs(1), Duration::from_secs(3)).unwrap()).expect("宿主采集失败")
}

/// 只读核对官方网络报文的完整三Gauge、三属性、唯一ID、类型、进程标记及值。
/// SDK运输值与同轮报告比较只证明同表及传输不失真；原getter数值语义由私有单元独立核验。
/// O(P log P)时间/O(P)临时空间，P是点数；不持锁、不登记、不反解合并名称。
fn assert_otlp(request: &ExportMetricsServiceRequest, report: &RuntimeMetricCollectionReport) {
    let mut expected = BTreeMap::new();
    for entry in report.instances() {
        assert_eq!(entry.status(), RuntimeMetricInstanceStatus::RecordInvoked);
        let (len, wait) = entry.counts().expect("活动Worker/Single必须有原读数");
        let name = entry.names().collect::<Vec<_>>().join("-");
        expected.insert(("pi_async_rt.runtime.len".to_owned(), entry.id().as_u64()), (name.clone(), len as i64));
        expected.insert(("pi_async_rt.runtime.wait_len".to_owned(), entry.id().as_u64()),
            (name.clone(), wait.expect("普通Worker/Single具有wait_len") as i64));
        expected.insert(("pi_async_rt.runtime.sample_state".to_owned(), entry.id().as_u64()), (name, 1));
    }
    let process = format!("{}-{}", PROCESS, std::process::id());
    let mut actual = BTreeMap::new();
    let mut instruments = BTreeSet::new();
    assert_eq!(request.resource_metrics.len(), 1, "只能有本测试宿主的一个SDK资源");
    for resource in &request.resource_metrics {
        assert_eq!(resource.scope_metrics.len(), 1, "只能有上游指标作用域");
        for scope in &resource.scope_metrics {
            let instrument_scope = scope.scope.as_ref().unwrap();
            assert_eq!(instrument_scope.name, "pi_async_rt");
            assert_eq!(instrument_scope.version, "0.5.17");
            for metric in &scope.metrics {
                assert!(instruments.insert(metric.name.clone()), "仪器不得重复");
                let (unit, description) = match metric.name.as_str() {
                    "pi_async_rt.runtime.len" => ("{task}", "已登记运行时原任务池待轮询数量"),
                    "pi_async_rt.runtime.wait_len" => ("{task}", "已登记运行时原等待读数"),
                    "pi_async_rt.runtime.sample_state" => ("1", "运行时指标采样及记录状态码"),
                    _ => panic!("不得出现未冻结的指标"),
                };
                assert_eq!(metric.unit, unit);
                assert_eq!(metric.description, description);
                let gauge = match metric.data.as_ref() {
                    Some(metric::Data::Gauge(gauge)) => gauge,
                    _ => panic!("只能上报Gauge"),
                };
                for point in &gauge.data_points {
                    assert_eq!(point.attributes.len(), 3);
                    let mut attributes = BTreeMap::new();
                    for attribute in &point.attributes {
                        assert!(attributes.insert(attribute.key.as_str(), attribute.value.as_ref()
                            .and_then(|value| value.value.as_ref()).unwrap()).is_none());
                    }
                    let id = match attributes.get("rt.id") {
                        Some(any_value::Value::IntValue(id)) if *id > 0 => *id as u64,
                        _ => panic!("指标ID必须为正整数"),
                    };
                    let name = match attributes.get("rt.name") {
                        Some(any_value::Value::StringValue(name)) => name.to_string(),
                        _ => panic!("必须保留完整上报名称"),
                    };
                    assert_eq!(attributes.get("rt.process"), Some(&&any_value::Value::StringValue(process.clone())));
                    assert!(point.time_unix_nano > 0);
                    assert_eq!(point.flags, 0);
                    let value = match point.value {
                        Some(number_data_point::Value::AsInt(value)) if value >= 0 => value,
                        _ => panic!("必须保留无损非负整数"),
                    };
                    assert!(actual.insert((metric.name.clone(), id), (name, value)).is_none(),
                        "同一核心每个Gauge只允许一个点");
                }
            }
        }
    }
    assert_eq!(instruments.len(), 3, "恰好三个共享Gauge");
    assert_eq!(actual, expected, "报告与真实SDK/OTLP必须同表、无丢失或额外数据");
}

/// 成功/默认关闭同一真实装配：先仅TCP/TLS公共Worker，再按需创建独立QUIC Worker。
/// 再构造同类协议和Clone不得增加核心或原名；不提前初始化未使用的QUIC单例。
#[test]
#[ignore = "需显式开启pi-async-rt/trace及真实回环网络权限，串行执行"]
fn test_runtime_metrics_real_network_and_otlp() {
    if !is_child("test_runtime_metrics_real_network_and_otlp") { return; }
    let _clock = pi_async_rt::rt::startup_global_time_loop(1).expect("仅子进程初始化一次时钟");
    let host = OtlpHost::new();
    init_runtime_metrics_reporting(PROCESS).unwrap();
    assert!(collect().instances().is_empty(), "构造前没有本例登记");
    let mut network = MqttNetwork::new();
    network.exercise();
    let first = collect();
    let enabled = usize::from(cfg!(feature = "runtime_metrics"));
    assert_eq!(first.instances().len(), enabled, "仅MQTT公共Worker应被库内自动登记");
    let first_id = first.instances().first().map(RuntimeMetricInstanceReport::id);
    let quic = QuicMqtt311::with_name("metrics-quic", 0);
    let _ = quic.clone();
    let _ = WsMqtt311::with_name("mqtt", "metrics-ws-second", 0, false);
    let _ = WssMqtt311::with_name("mqtt", "metrics-tls-shared", 0, false);
    let _ = QuicMqtt311::with_name("metrics-quic-second", 0);
    let final_report = collect();
    assert_eq!(final_report.instances().len(), 2 * enabled);
    assert_eq!(final_report.active_count(), 2 * enabled);
    let ids: BTreeSet<_> = final_report.instances().iter().map(|entry| entry.id().as_u64()).collect();
    assert_eq!(ids.len(), 2 * enabled);
    if let Some(id) = first_id { assert!(ids.contains(&id.as_u64())); }
    for entry in final_report.instances() {
        assert_eq!(entry.kind(), RuntimeMetricKind::Worker);
        assert_eq!(entry.names().collect::<Vec<_>>(), vec![entry.name()]);
        let suffix = entry.name().strip_prefix("pi_mqtt-worker-").expect("必须为冻结三段名");
        assert!(suffix.parse::<usize>().unwrap() > 0);
    }
    if enabled != 0 { assert_otlp(&host.flush(), &final_report); }
    network.send(&Packet::Pingreq);
    assert_eq!(network.receive(), Packet::Pingresp, "采集/登记后业务必须继续正常");
    drop(network);
    host.shutdown();
}

/// 仅捕获本库两处新增失败日志；不回调业务、不重入lazy初始化，不持锁跨await。
struct FailureLog(Mutex<Vec<String>>);
static FAILURE_LOG: FailureLog = FailureLog(Mutex::new(Vec::new()));

impl log::Log for FailureLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() == log::Level::Error && metadata.target().starts_with("pi_mqtt::")
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) { self.0.lock().unwrap().push(record.args().to_string()); }
    }
    fn flush(&self) {}
}

/// 隔离、真实容量边界：13个不创建OS线程的Single核心共1024原名，均短名且合并<=256字节。
/// 经公开API确认下一新登记真为CapacityExceeded，再走真实MQTT网络与两原初始化块。
/// 精确验证失败日志数量/类别、本库未假登记、全部宿主核心仍活动且读数为0；不伪造ID。
#[test]
#[ignore = "独占注册表容量、需pi-async-rt/trace及真实回环网络权限，串行执行"]
fn test_runtime_metrics_capacity_degrades_without_breaking_network() {
    if !is_child("test_runtime_metrics_capacity_degrades_without_breaking_network") { return; }
    let _clock = pi_async_rt::rt::startup_global_time_loop(1).expect("仅子进程初始化一次时钟");
    log::set_logger(&FAILURE_LOG).unwrap();
    log::set_max_level(log::LevelFilter::Error);
    let host = OtlpHost::new();
    init_runtime_metrics_reporting(PROCESS).unwrap();
    let mut runners = Vec::new();
    let mut expected = BTreeMap::new();
    let mut remaining: usize = 1024;
    while remaining > 0 {
        let runner = SingleTaskRunner::<()>::default();
        let runtime = runner.startup().unwrap();
        let count = remaining.min(85);
        let mut names = Vec::new();
        let mut id = None;
        for alias in 0..count {
            let name = format!("{:02x}", alias);
            let registered = register_runtime_metrics(&runtime, &name).unwrap();
            if let Some(id) = id { assert_eq!(registered, id); }
            id = Some(registered);
            names.push(name);
        }
        assert!(names.join("-").len() <= 256);
        assert!(expected.insert(id.unwrap(), names).is_none());
        runners.push(runner);
        remaining -= count;
    }
    assert_eq!(expected.len(), 13);
    let probe = SingleTaskRunner::<()>::default();
    assert_eq!(register_runtime_metrics(&probe.startup().unwrap(), "overflow"),
        Err(RuntimeMetricRegistrationError::CapacityExceeded), "必须真实耗尽预算而非名称非法");
    let mut network = MqttNetwork::new();
    network.exercise();
    let _ = QuicMqtt311::with_name("metrics-capacity-quic", 0);
    let report = collect();
    assert_eq!(report.instances().len(), 13);
    assert_eq!(report.active_count(), 13, "生产失败不能注销其它宿主核心");
    assert_eq!(report.sampled_count(), 13);
    for entry in report.instances() {
        assert_eq!(entry.kind(), RuntimeMetricKind::Single);
        assert_eq!(entry.names().collect::<Vec<_>>(), expected[&entry.id()].iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(entry.counts(), Some((0, Some(0))), "独立空Single的原读数必须保持0");
    }
    let logs = FAILURE_LOG.0.lock().unwrap();
    assert_eq!(logs.len(), 2 * usize::from(cfg!(feature = "runtime_metrics")));
    for message in logs.iter() {
        assert!(message.contains("运行时指标注册失败"));
        assert!(message.contains("名称: pi_mqtt-worker-"));
        assert!(message.contains("运行时指标累计系列容量已满"));
    }
    drop(logs);
    assert_otlp(&host.flush(), &report);
    network.send(&Packet::Pingreq);
    assert_eq!(network.receive(), Packet::Pingresp);
    drop(network);
    // 只撤销本测试宿主实际拥有的空Single登记；不能替生产注销MQTT私有单例。
    for id in expected.keys() { assert!(unregister_runtime_metrics(*id)); }
    drop(runners);
    host.shutdown();
}
