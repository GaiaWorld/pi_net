//! MQTT指标专项的真实宿主运输：官方SDK -> OTLP/gRPC -> 127.0.0.1接收器。
//!
//! 参考上游0.5.17真实OTLP装配，仅保存官方已解码报文，不登记被测运行时、不合成指标。
//! Tokio只服务测试运输，不冒充pi_async_rt可观测实例。恒定一个运输worker及一个SDK读取线程。
//! 全部同步控制方法只供隔离子进程的外部测试线程调用；父进程60秒截止约束SDK等待。

use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
    metrics_service_server::{MetricsService, MetricsServiceServer},
};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use std::{net::{Ipv4Addr, TcpListener}, sync::mpsc::{self, Receiver, SyncSender}, time::Duration};
use tokio::{runtime::{Builder, Runtime}, sync::oneshot, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, transport::Server};

const IO_LIMIT: Duration = Duration::from_secs(3);

/// 只接收真实官方报文；满队列或关闭必须返回错误，不能回执成功后丢弃。
#[derive(Debug)]
struct ReceiverService(SyncSender<ExportMetricsServiceRequest>);

#[tonic::async_trait]
impl MetricsService for ReceiverService {
    async fn export(&self, request: Request<ExportMetricsServiceRequest>)
        -> Result<Response<ExportMetricsServiceResponse>, Status>
    {
        self.0.try_send(request.into_inner())
            .map_err(|_| Status::resource_exhausted("测试接收队列已满或关闭"))?;
        Ok(Response::new(ExportMetricsServiceResponse { partial_success: None }))
    }
}

/// 独占SDK、回环listener及运输运行时的测试所有者；不拥有MQTT私有Worker的关闭权限。
/// 构造/刷新/关闭有线程、网络和全局Provider副作用，不幂等；只允许在独立子进程使用。
/// 无新增unsafe、业务同步锁或跨await持有的测试锁；不能以此宣称生产远端可用性。
pub(super) struct OtlpHost {
    runtime: Option<Runtime>,
    provider: Option<SdkMeterProvider>,
    stop: Option<oneshot::Sender<()>>,
    stopped: Receiver<Result<(), tonic::transport::Error>>,
    task: Option<JoinHandle<()>>,
    requests: Receiver<ExportMetricsServiceRequest>,
}

impl OtlpHost {
    /// 绑定随机回环端口并直接转交同一listener，避免端口释放/重绑定竞争。
    /// 在首次采集初始化前安装有效官方Provider；只创建测试设施，不调用生产登记API。
    /// SDK/OS失败使测试失败，O(1)设施空间及SDK实现成本；必须由父进程限制整例时间。
    pub(super) fn new() -> Self {
        let runtime = Builder::new_multi_thread().worker_threads(1)
            .thread_name("mqtt-metrics-transport").enable_all().build().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::sync_channel(4);
        let (stop, stop_rx) = oneshot::channel();
        let (done, stopped) = mpsc::sync_channel(1);
        let task = {
            let _entered = runtime.enter();
            let incoming = TcpListenerStream::new(tokio::net::TcpListener::from_std(listener).unwrap());
            runtime.spawn(async move {
                let service = MetricsServiceServer::new(ReceiverService(tx))
                    .max_decoding_message_size(256 * 1024);
                let result = Server::builder().serve_with_incoming_shutdown(
                    service, incoming, async { let _ = stop_rx.await; }
                ).await;
                let _ = done.try_send(result);
            })
        };
        // 先取得清理所有权，SDK构建异常时也能停止已启动的接收端。
        let mut host = Self { runtime: Some(runtime), provider: None, stop: Some(stop),
            stopped, task: Some(task), requests };
        let provider = {
            let _entered = host.runtime.as_ref().unwrap().enter();
            let exporter = MetricExporter::builder().with_tonic().with_endpoint(endpoint)
                .with_timeout(IO_LIMIT).build().unwrap();
            let reader = PeriodicReader::builder(exporter)
                .with_interval(Duration::from_secs(3600)).build();
            SdkMeterProvider::builder().with_reader(reader).build()
        };
        opentelemetry::global::set_meter_provider(provider.clone());
        host.provider = Some(provider);
        host
    }

    /// 刷新已有真实采集结果，返回恰好一份实际网络接收的官方报文；不生成或补采样。
    /// SDK刷新并非硬实时，父进程60秒兜底；接收有3秒截止，重复请求也使断言失败。
    pub(super) fn flush(&self) -> ExportMetricsServiceRequest {
        self.provider.as_ref().unwrap().force_flush().unwrap();
        let request = self.requests.recv_timeout(IO_LIMIT).expect("OTLP报文未到达回环接收端");
        assert!(self.requests.try_recv().is_err(), "单次刷新不得出现额外OTLP报文");
        request
    }

    /// 正常关闭测试SDK、接收器和运输运行时；失败不得被Drop的尽力清理掩盖。
    /// 调用方须已停止新采集；不注销或关闭MQTT进程级私有核心，不代表它们已析构。
    pub(super) fn shutdown(mut self) {
        self.provider.take().unwrap().shutdown().unwrap();
        self.stop_transport().expect("OTLP接收端关闭失败");
    }

    /// 有界回收仅属于本夹具的运输资源，不读取或修补MQTT任务/队列。
    fn stop_transport(&mut self) -> Result<(), String> {
        if self.runtime.is_none() { return Ok(()); }
        if let Some(stop) = self.stop.take() { let _ = stop.send(()); }
        let result = self.stopped.recv_timeout(IO_LIMIT)
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
        if let Some(task) = self.task.take() {
            if result.is_err() { task.abort(); }
        }
        if let Some(runtime) = self.runtime.take() { runtime.shutdown_timeout(Duration::from_secs(2)); }
        result
    }
}

impl Drop for OtlpHost {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            if let Err(error) = provider.shutdown() { eprintln!("测试SDK异常清理失败：{}", error); }
        }
        if let Err(error) = self.stop_transport() { eprintln!("测试运输异常清理失败：{}", error); }
    }
}
