//! 异步WebSocket客户端；外部运行时只在原客户端启动成功/返回前冷登记。
//! 高级用法可在冷初始化调用prepare_runtime_metrics准备另一个普通运行时核心；
//! 连接、消息、辅助Future、Clone/Drop/关闭不增加指标操作，Actix本身不接入。
//! NET-MET-BATCH-DESIGN-001/B-WSC；本次参数/Ok专项见batch_runtime_metrics_tests。

#[macro_use]
extern crate lazy_static;

use std::thread;
use std::pin::Pin;
use std::sync::Arc;
use std::str::FromStr;
use std::future::Future;
use std::fmt::{self, Debug};
use std::task::{Context, Poll};
use std::result::Result as GenResult;
use std::cell::{UnsafeCell, RefCell};
use std::time::{Duration, SystemTime};
use std::io::{Error, Result, ErrorKind};

use parking_lot::RwLock;
use futures::future::FutureExt;
use futures_util::{sink::SinkExt, stream::StreamExt};
use flume::{Sender, Receiver, bounded};
use url::Url;
use webpki;
use rustls::{ClientConfig, RootCertStore, Certificate, ServerName, Error as TLSError,
             client::{ServerCertVerifier, ServerCertVerified}};
use actix_rt::{self, System};
use actix_codec::Framed;
use actix_http::ws::{Codec, Item};
use awc::{Client, Connector, BoxedSocket, ClientResponse,
          ws::{self, Frame, Message, CloseCode, CloseReason}};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use bytestring::ByteString;
use log::{info, warn, error};

use pi_async_rt::rt::{TaskId, AsyncTaskPool, AsyncTaskPoolExt, AsyncRuntime, AsyncWaitResult};
use async_lock::Mutex;
use pi_rand::{SecureRng, xor_unencrypt_clarity};

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod batch_runtime_metrics_tests {
    //! 验证本库prepare_runtime_metrics及自动冷入口实际调用参数/原Ok。
    //! 普通Single/Multi/Worker均使用真实公开装配，不采集、不安装SDK、不测试上游算法。
    //! 测试专用TLS不持核心；实际成功ID取自本次调用，断言前显式注销测试自有登记。

    use super::*;

    /// 仅观测本次真实调用并核对原编号/完整名称/原Ok；不是手动预登记或生产查询。
    /// expected_id和kind是测试独立期望，O(L)命名核对，无await/锁借用/采样或生命周期推断。
    fn assert_registration(expected_id: usize, kind: &str) {
        let observed = WSC_RUNTIME_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (actual_id, actual_name, registration) = observed.expect("原冷入口必须实际登记");
        assert_eq!(actual_id, expected_id, "必须使用原注入核心");
        assert_eq!(actual_name, format!("pi_async_wsc-{}-{}", kind, expected_id));
        assert!(registration.is_ok(), "真实登记必须成功: {:?}", registration);
    }

    /// 被测生产：prepare_runtime_metrics的三种安全Any识别，保留各自原Pool和编号。
    /// 只创建一个空Multi worker；Single/Worker使用真实Runner，不创建采集运行时。
    /// 无网络/采集/业务任务，不重复容量/别名/SDK测试；原Multi worker由测试进程退出回收。
    #[test]
    fn test_prepare_runtime_metrics_supported_types() {
        let _clock = pi_async_rt::rt::startup_global_time_loop(10);
        let single_runner = pi_async_rt::rt::single_thread::SingleTaskRunner::<()>::default();
        let single = single_runner.startup().expect("真实Single启动");
        prepare_runtime_metrics(&single);
        assert_registration(single.get_id(), "single");

        let multi = pi_async_rt::rt::AsyncRuntimeBuilder::default_multi_thread(
            Some("cold-metrics-unit"), None, Some(1), Some(1),
        );
        prepare_runtime_metrics(&multi);
        assert_registration(multi.get_id(), "multi");

        let worker_runner = pi_async_rt::rt::worker_thread::WorkerTaskRunner::<()>::default();
        let worker = worker_runner.get_runtime();
        prepare_runtime_metrics(&worker);
        assert_registration(worker.get_id(), "worker");
    }

    /// 被测生产入口：AsyncWebsocketClient::new原握手成功后的自动冷登记。
    /// 真实Actix初始化但不建立网络连接；原close由测试所属Single执行一次用于清理，
    /// 不声称测试了网络/退出join。命令另有600秒硬截止防止原同步握手永久等待。
    #[test]
    fn test_wsc_client_registers_runtime_metrics() {
        let _clock = pi_async_rt::rt::startup_global_time_loop(10);
        let runner = pi_async_rt::rt::single_thread::SingleTaskRunner::<()>::default();
        let runtime = runner.startup().expect("真实Single启动");
        let client = AsyncWebsocketClient::new(runtime.clone(), "metrics-unit".to_string(), 32)
            .expect("真实客户端原启动握手");
        let closed = client.close();
        let dispatched = runner.run_once();
        assert_registration(runtime.get_id(), "single");
        closed.expect("原客户端关闭指令应能投递");
        dispatched.expect("原Single推进关闭任务");
    }
}

// 仅单元测试观测真实调用，纯参数/原Result不持核心，普通生产完全移除。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static WSC_RUNTIME_METRICS_TEST_RESULT: std::cell::RefCell<Option<(usize, String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>)>> = const { std::cell::RefCell::new(None) };
}

/// 在服务冷初始化中为WebSocket所用的外部普通运行时指定名称并登记。
///
/// runtime仅借用，不保存、不创建或接管核心；返回()不是登记成功回执。
/// 自动冷构造/配置也复用此函数；直接使用另一核心时，在逐连接/消息调用以前准备：
/// ```no_run
/// fn prepare(runtime: &pi_async_rt::rt::multi_thread::MultiTaskRuntime<()>) {
///     pi_async_wsc::prepare_runtime_metrics(runtime);
/// }
/// ```
///
/// 安全识别普通Single/Multi/Worker及原R::Pool，按pi_async_wsc-类型-原get_id命名；
/// 不增加原AsyncRuntime的公开约束，不强转、读私有槽或改变调度。不支持的自定义实现
/// 以及上游登记错误只中文告警，原业务继续；关闭本crate功能/wasm条件时无操作。
///
/// 此函数非纯：上游登记会强保活，重复同名幂等归上游；不保存生产ID/结果，不在
/// Drop/业务失败/关闭时自动注销。宿主负责采集、Provider、共享核心生命周期，
/// 不应在热路径反复调用或从这里恢复被宿主注销的核心；不安装时钟/采集器。
///
/// 本层名称处理O(L)时间/空间；上游登记另含O(C+A*L+M+L)时间和O(C+A+M+L)
/// 分配上界，C为历史核心数，A为名称数，M为旧合并名长度。同步短锁/分配器和
/// 宿主logger可能等待或panic，不保证硬实时/严格非阻塞；无await/异步锁/业务任务，
/// 不在持业务锁时调用。普通运行时原Send/Sync合同不变，不新增unsafe或本地线程权限。
/// 关联单元：batch_runtime_metrics_tests::test_prepare_runtime_metrics_supported_types。
pub fn prepare_runtime_metrics<R: AsyncRuntime<()>>(runtime: &R) {
    #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
    {
        use std::any::Any;
        use pi_async_rt::rt::{single_thread::SingleTaskRuntime,
            multi_thread::MultiTaskRuntime, worker_thread::WorkerRuntime};
        use pi_async_rt::runtime_metrics::register_runtime_metrics;

        let any = runtime as &dyn Any;
        let (metric_name, registration) = if let Some(rt) = any.downcast_ref::<SingleTaskRuntime<(), R::Pool>>() {
            let name = format!("pi_async_wsc-single-{}", rt.get_id());
            let result = register_runtime_metrics(rt, &name);
            (name, result)
        } else if let Some(rt) = any.downcast_ref::<MultiTaskRuntime<(), R::Pool>>() {
            let name = format!("pi_async_wsc-multi-{}", rt.get_id());
            let result = register_runtime_metrics(rt, &name);
            (name, result)
        } else if let Some(rt) = any.downcast_ref::<WorkerRuntime<(), R::Pool>>() {
            let name = format!("pi_async_wsc-worker-{}", rt.get_id());
            let result = register_runtime_metrics(rt, &name);
            (name, result)
        } else {
            log::warn!("WebSocket 运行时指标未注册, 不支持的外部类型: {}", std::any::type_name::<R>());
            return;
        };
        #[cfg(test)]
        WSC_RUNTIME_METRICS_TEST_RESULT.with(|value| {
            *value.borrow_mut() = Some((runtime.get_id(), metric_name.clone(), registration));
        });
        if let Err(error) = registration {
            log::warn!("WebSocket 运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
        }
    }
    #[cfg(not(all(feature = "runtime_metrics", not(target_arch = "wasm32"))))]
    let _ = runtime;
}

// 用于加密随机数种子的密钥
// 注意如需修改，则必须同时修改客户端
const SAFE_SEED_KEY: u64 = 0xffabcdef0fedcba0;

/*
* Websocket连接
*/
thread_local! {
    static ASYNC_WEBSOCKET_CONNECTION: Arc<UnsafeCell<Option<Framed<BoxedSocket, Codec>>>> = Arc::new(UnsafeCell::new(None));
}

/*
* 异步Websocket客户端，客户端同时只允许存在一个Websocket连接，创建新连接时会关闭旧连接
*/
pub struct AsyncWebsocketClient<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> {
    rt:     RT,                                 //外部异步运行时
    sender: Sender<AsyncWebsocketCmd<P, RT>>,   //客户端所在异步运行时的指令发送器
}

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Send for AsyncWebsocketClient<P, RT> {}
unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Sync for AsyncWebsocketClient<P, RT> {}

/*
* 异步Websocket客户端同步方法
*/
impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> AsyncWebsocketClient<P, RT> {
    /// 构建异步WebSocket客户端并在原启动握手成功后冷登记外部运行时。
    ///
    /// rt按原逻辑保存，name为客户端日志名称；queue_len不在32..=65535时仍按原规则取32。
    /// 返回原Result，原握手错误不登记；指标失败/未知泛型只warn，不新增业务错误/字段。
    /// 构造仍按原逻辑建Actix线程并同步等握手，不是非阻塞API；登记不代表业务任务完成。
    /// 命名O(L)时间/空间，加上游短同步锁/分配/logger成本；不新增线程/await/异步锁。
    /// 使用已准备的普通Single/Multi/Worker可自动登记，原自定义AsyncRuntime签名仍兼容。
    /// 仅冷初始化调用；成功强保活不转移外部核心所有权，客户端关闭/Drop不自动注销。
    /// 示例：AsyncWebsocketClient::new(runtime, "client".to_string(), 32)?；
    /// 对应专项batch_runtime_metrics_tests::test_wsc_client_registers_runtime_metrics，非网络验收。
    pub fn new(rt: RT,
               name: String,
               mut queue_len: usize) -> Result<AsyncWebsocketClient<P, RT>> {
        if queue_len < 32 || queue_len > 65535 {
            queue_len = 32;
        }
        let (sender, receiver) = bounded(queue_len);

        //打开新的线程，来运行异步Websocket客户端
        let sender_copy = sender.clone();
        let (send, recv) = bounded(1);
        thread::spawn(move || {
            let mut runner = System::new();
            send.send(());
            info!("Websocket client {:?} running...", name);
            runner.block_on(async move {
                event_loop(receiver, sender_copy).await;
            });
            warn!("Websocket client {:?} stoped", name);
        });

        match recv.recv() {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Build websocket client failed, reason: {:?}", e)))
            },
            Ok(_) => {
                prepare_runtime_metrics(&rt);
                Ok(AsyncWebsocketClient {
                    rt,
                    sender,
                })
            },
        }
    }

    //构建异步Websocket连接
    pub fn build(&self,
                 url: &str,
                 protocols: Vec<String>,
                 handler: AsyncWebsocketHandler) -> Result<AsyncWebsocket<P, RT>> {
        match Url::from_str(url) {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("Open websocket failed, url: {}, reason: {:?}", url, e)))
            },
            Ok(url) => {
                let mut ws = AsyncWebsocket::new(self.rt.clone(),
                                                 self.sender.clone(),
                                                 url,
                                                 protocols,
                                                 handler);
                ws.set_task_id(self.rt.alloc::<()>());

                Ok(ws)
            },
        }
    }

    //关闭客户端，同时关闭连接
    pub fn close(&self) -> Result<()> {
        let sender = self.sender.clone();

        self.rt.spawn_by_id(self.rt.alloc::<()>(), async move {
            if let Err(e) = sender.send_async(AsyncWebsocketCmd::Stop(None)).await {
                error!("Close websocket client failed, reason: {:?}", e);
            }
        })
    }
}

// 异步Websocket客户端事件循环
async fn event_loop<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
>(receiver: Receiver<AsyncWebsocketCmd<P, RT>>,
  sender: Sender<AsyncWebsocketCmd<P, RT>>) {
    let mut current_connection: Option<Framed<BoxedSocket, Codec>> = None; //Websocket连接
    loop {
        match receiver.recv_async().await {
            Err(e) => {
                //接收指令错误，则立即关闭当前连接，并关闭客户端
                error!("Websocket client error, reason: {:?}", e);

                if let Some(ws_con) = current_connection.as_mut() {
                    ws_con.send(ws::Message::Close(Some(CloseReason::from(CloseCode::Other(500))))).await;
                }

                break;
            },
            Ok(cmd) => {
                match cmd {
                    AsyncWebsocketCmd::Open(ws,
                                            task_id,
                                            is_strict,
                                            is_sync_seed,
                                            timeout) => {
                        //建立指定url的连接
                        let url = ws.0.status.read().get_url().clone();
                        let protocols = ws.0.status.read().get_protocols().to_vec();

                        let connector = if is_strict {
                            //严格模式
                            Connector::new()
                                .timeout(Duration::from_millis(timeout))
                        } else {
                            //非严格模式
                            let mut config = ClientConfig::builder()
                                .with_safe_default_cipher_suites()
                                .with_safe_default_kx_groups()
                                .with_safe_default_protocol_versions()
                                .expect("Build ClientConfig failed")
                                .with_root_certificates(RootCertStore::empty())
                                .with_no_client_auth();

                            let protos = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
                            config.alpn_protocols = protos;

                            config
                                .dangerous()
                                .set_certificate_verifier(Arc::new(NoCertificateVerification));

                            Connector::new()
                                .rustls(Arc::new(config))
                                .timeout(Duration::from_millis(timeout))
                        };
                        let mut client = Client::builder()
                            .connector(connector)
                            .timeout(Duration::from_millis(timeout))
                            .finish();
                        let mut ws_req = client
                            .ws(url.as_str())
                            .protocols(protocols.clone());

                        if let Some(origin) = &*ws.0.origin.read() {
                            //设置了Origin
                            ws_req = ws_req.origin(origin.as_str());
                        }
                        if let Some(size) = &*ws.0.recv_size.read() {
                            //设置了最大接收帧大小
                            ws_req = ws_req.max_frame_size(*size);
                        }
                        if *ws.0.masking_strict.read() {
                            //设置了严格的掩码处理
                            ws_req = ws_req.server_mode();
                        }

                        match ws_req.connect().await {
                            Err(e) => {
                                let reason = format!("Open websocket failed, url: {}, protocols: {:?}, reason: {:?}", url, protocols, e);
                                *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                error!("{}", reason);
                            },
                            Ok((resp, connection)) => {
                                //打开Websocket连接成功
                                if let Some(mut ws_con) = current_connection.take() {
                                    //立即关闭旧连接
                                    ws_con.send(ws::Message::Close(None)).await;
                                }
                                current_connection = Some(connection); //更新当前连接

                                if is_sync_seed {
                                    //当前连接需要同步种子，则设置连接状态为同步种子中
                                    *ws.0.status.write() = AsyncWebsocketStatus::SyncingSeed(url.clone(), protocols.clone());

                                    let ws_copy = ws.clone();
                                    sender.send_async(AsyncWebsocketCmd::ReceiveSeed(ws_copy,
                                                                                     task_id,
                                                                                     format!("{:?}", resp),
                                                                                     None,
                                                                                     5000)).await;
                                } else {
                                    //当前连接不需要同步种子，则设置连接状态为已连接
                                    *ws.0.status.write() = AsyncWebsocketStatus::Connected(url.clone(), protocols.clone());
                                    ws.0.handler.on_open(); //通知处理器连接成功
                                    ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                    info!("Open websocket ok, url: {}, protocols: {:?}, resp: {:?}", url, protocols, resp);
                                }
                            }
                        }
                    },
                    AsyncWebsocketCmd::ReceiveSeed(ws,
                                                   task_id,
                                                   resp,
                                                   mut received_message,
                                                   receive_timeout) => {
                        //需要和服务器端同步种子，则接收服务器端发送的种子
                        if let Some(ws_con) = current_connection.as_mut() {
                            //当前客户端有连接，则立即接收消息，如果当前缓冲区为空，则会挂起接收的异步任务
                            if let Ok(r) = actix_rt::time::timeout(
                                Duration::from_millis(receive_timeout),
                                ws_con.next()
                            ).await {
                                if let Some(respone) = r {
                                    match respone {
                                        Err(e) => {
                                            //接收消息帧失败，则立即退出本次消息接收
                                            let url = ws.0.status.read().get_url().clone();
                                            let protocols = ws.0.status.read().get_protocols().to_vec();
                                            let reason = format!("Websocket receive seed failed, url: {}, reason: {:?}", url, e);
                                            *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                            ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                            ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                            error!("{}", reason);
                                        },
                                        Ok(frame) => {
                                            //接收消息帧成功
                                            received_message = Some(receive_frames(received_message, frame));
                                            if let Some(ReceivedMessage::Discomplete(_, _)) = &received_message {
                                                //接收消息不完整，则通过消息队列投递继续接收种子的任务，继续接收种子的后续帧，并立即结束当前接收，避免接收长时间独占Websocket连接的运行时
                                                let ws_copy = ws.clone();
                                                sender.send_async(AsyncWebsocketCmd::ReceiveSeed(
                                                    ws_copy,
                                                    task_id,
                                                    resp,
                                                    received_message,
                                                    receive_timeout)).await;
                                            } else {
                                                match received_message {
                                                    Some(ReceivedMessage::Err(e)) => {
                                                        //接收消息帧失败，则立即退出本次消息接收
                                                        let url = ws.0.status.read().get_url().clone();
                                                        let protocols = ws.0.status.read().get_protocols().to_vec();
                                                        let reason = format!("Websocket receive seed failed, url: {}, reason: {:?}", url, e);
                                                        *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                                        ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                        ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                                        error!("{}", reason);
                                                    },
                                                    Some(ReceivedMessage::Completed(msg)) => {
                                                        //接收消息完整
                                                        match msg {
                                                            Message::Close(None) => {
                                                                //接收到关闭消息
                                                                ws.0.handler.on_close(CloseCode::Normal, "");
                                                            },
                                                            Message::Close(Some(reason)) => {
                                                                //接收到关闭消息
                                                                let url = ws.0.status.read().get_url().clone();
                                                                let protocols = ws.0.status.read().get_protocols().to_vec();
                                                                *ws.0.status.write() = AsyncWebsocketStatus::Closed(url, protocols, Some(reason.code)); //设置连接状态
                                                                if let Some(desc) = reason.description {
                                                                    //有关闭的原因
                                                                    ws.0.handler.on_close(reason.code, desc.as_str());
                                                                } else {
                                                                    ws.0.handler.on_close(reason.code, "");
                                                                }
                                                            },
                                                            Message::Binary(bin) => {
                                                                //接收到种子
                                                                if bin.len() == 4 {
                                                                    //无效的种子
                                                                    let url = ws.0.status.read().get_url().clone();
                                                                    let protocols = ws.0.status.read().get_protocols().to_vec();
                                                                    let reason = format!("Websocket receive seed failed, url: {}, reason: invalid seed length", url);
                                                                    *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                                                    ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                                    ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                                                    error!("{}", reason);
                                                                    continue;
                                                                }

                                                                match xor_unencrypt_clarity(bin, SAFE_SEED_KEY.to_le_bytes()) {
                                                                    Err(e) => {
                                                                        //无效的种子
                                                                        let url = ws.0.status.read().get_url().clone();
                                                                        let protocols = ws.0.status.read().get_protocols().to_vec();
                                                                        let reason = format!("Websocket receive seed failed, url: {}, reason: {:?}", url, e);
                                                                        *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                                                        ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                                        ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                                                        error!("{}", reason);
                                                                    },
                                                                    Ok(vec) => {
                                                                        let seed = u64::from_le_bytes(vec.as_slice().try_into().unwrap());
                                                                        let rng = SecureRng::with_seed(seed);
                                                                        *ws.0.rng.lock().await = Some(rng); //设置随机数生成器

                                                                        let url = ws.0.status.read().get_url().clone();
                                                                        let protocols = ws.0.status.read().get_protocols().to_vec();
                                                                        *ws.0.status.write() = AsyncWebsocketStatus::Connected(url.clone(), protocols.clone());
                                                                        ws.0.handler.on_open(); //通知处理器连接成功
                                                                        ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                                                        info!("Open websocket ok, url: {}, protocols: {:?}, resp: {:?}", url, protocols, resp);
                                                                    },
                                                                }
                                                            },
                                                            msg => {
                                                                //接收到其它非法消息
                                                                let url = ws.0.status.read().get_url().clone();
                                                                let protocols = ws.0.status.read().get_protocols().to_vec();
                                                                let reason = format!("Websocket receive seed failed, url: {}, msg: {:?}, reason: invalid msg", url, msg);
                                                                *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                                                ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                                                error!("{}", reason);
                                                            },
                                                        }
                                                    },
                                                    _ => {
                                                        //接收消息帧失败，不应该进入此分支，则立即退出本次种子接收
                                                        let url = ws.0.status.read().get_url().clone();
                                                        let protocols = ws.0.status.read().get_protocols().to_vec();
                                                        let reason = format!("Websocket receive seed failed, url: {}, reason: unknow", url);
                                                        *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                                        ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                        ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                                        error!("{}", reason);
                                                    },
                                                }
                                            }
                                        },
                                    }
                                } else {
                                    //有本次接收的最大消息数限制或无本次接收的最大消息限制，当连接流结束时立即退出本次消息接收
                                    let url = ws.0.status.read().get_url().clone();
                                    let protocols = ws.0.status.read().get_protocols().to_vec();
                                    let reason = format!("Websocket receive seed failed, url: {}, reason: receive EOF", url);
                                    *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                    ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                    ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                                }
                            } else {
                                //当接收超时，则立即退出本次消息接收
                                let url = ws.0.status.read().get_url().clone();
                                let protocols = ws.0.status.read().get_protocols().to_vec();
                                let reason = format!("Websocket receive seed failed, url: {}, reason: Timeout", url);
                                *ws.0.status.write() = AsyncWebsocketStatus::Error(url, protocols, None, Some(Error::new(ErrorKind::ConnectionAborted, reason.clone()))); //设置连接状态
                                ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步打开连接的任务
                            }
                        }
                    },
                    AsyncWebsocketCmd::Send(ws,
                                            task_id,
                                            msg,
                                            result) => {
                        //向服务器端发送消息
                        if let Some(ws_con) = current_connection.as_mut() {
                            //当前客户端有连接，则立即发送消息
                            let mut is_sended = true;
                            for msg in send_frames(msg, *ws.0.send_size.read()) {
                                //发送单帧或多帧消息
                                if let Err(e) = ws_con.send(msg).await {
                                    //发送消息失败
                                    let url = ws.0.status.read().get_url().clone();
                                    let reason = format!("Websocket send failed, url: {}, reason: {:?}", url, e);
                                    let error = Error::new(ErrorKind::Other, reason.clone());
                                    *result.0.borrow_mut() = Some(Err(error)); //设置发送消息的结果
                                    ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                    ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步发送消息的任务
                                    is_sended = false; //设置状态为发送失败
                                    break;
                                }

                                if *ws.0.is_nodelay.read() {
                                    //TODO 立即刷新连接...
                                }
                            }

                            if is_sended {
                                //发送消息成功
                                *result.0.borrow_mut() = Some(Ok(())); //设置发送消息的结果
                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步发送消息的任务
                            }
                        }
                    },
                    AsyncWebsocketCmd::Receive(ws,
                                               task_id,
                                               require_count,
                                               received_count,
                                               mut received_message,
                                               receive_timeout,
                                               result) => {
                        //接收服务器端发送的消息
                        if let Some(ws_con) = current_connection.as_mut() {
                            //当前客户端有连接，则立即接收消息，如果当前缓冲区为空，则会挂起接收的异步任务
                            if let Some(0) = require_count {
                                //有本次接收的最大消息数限制，且已接收指定数量的消息，则退出本次消息接收
                                *result.0.borrow_mut() = Some(Ok(())); //设置接收消息的结果
                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步接收消息的任务
                            } else {
                                //已接收至少一条消息，检查缓冲区是否还有未接收的消息
                                if received_count > 0 && ws_con.is_read_buf_empty() {
                                    //已接收至少一条消息，且连接的读缓冲区为空
                                    if require_count.is_some() {
                                        //有本次接收的最大消息数限制，且当前缓冲区没有可接收的消息帧，但还有未接收的指定数量的消息，则继续异步接收剩余消息，并立即结束当前接收
                                        //通过消息队列投递继续接收的任务，避免接收长时间独占Websocket连接的运行时
                                        let ws_copy = ws.clone();
                                        sender.send_async(AsyncWebsocketCmd::Receive(
                                            ws_copy,
                                            task_id,
                                            require_count,
                                            received_count,
                                            received_message,
                                            receive_timeout,
                                            result)).await;
                                    } else {
                                        //无本次接收的最大消息数限制，且当前缓冲区没有可接收的消息帧，则立即退出本次消息接收
                                        *result.0.borrow_mut() = Some(Ok(())); //设置接收消息的结果
                                        ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步接收消息的任务
                                    }
                                } else {
                                    //还未接收至少一条消息，或连接的缓冲区不为空
                                    if let Ok(r) = actix_rt::time::timeout(
                                        Duration::from_millis(receive_timeout),
                                        ws_con.next()
                                    ).await {
                                        if let Some(respone) = r {
                                            match respone {
                                                Err(e) => {
                                                    //接收消息帧失败，则立即退出本次消息接收
                                                    let url = ws.0.status.read().get_url().clone();
                                                    let reason = format!("Websocket receive failed, url: {}, reason: {:?}", url, e);
                                                    let error = Error::new(ErrorKind::Other, reason.clone());
                                                    *result.0.borrow_mut() = Some(Err(error)); //设置接收消息的结果
                                                    ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                    ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步接收消息的任务
                                                    error!("{}", reason);
                                                },
                                                Ok(frame) => {
                                                    //接收消息帧成功
                                                    received_message = Some(receive_frames(received_message, frame));
                                                    if let Some(ReceivedMessage::Discomplete(_, _)) = &received_message {
                                                        //接收消息不完整，则通过消息队列投递继续接收的任务，继续接收消息的后续帧，并立即结束当前接收，避免接收长时间独占Websocket连接的运行时
                                                        let ws_copy = ws.clone();
                                                        sender.send_async(AsyncWebsocketCmd::Receive(
                                                            ws_copy,
                                                            task_id,
                                                            require_count,
                                                            received_count,
                                                            received_message,
                                                            receive_timeout,
                                                            result)).await;
                                                    } else {
                                                        match received_message {
                                                            Some(ReceivedMessage::Err(e)) => {
                                                                //接收消息帧失败，则立即退出本次消息接收
                                                                let url = ws.0.status.read().get_url().clone();
                                                                let reason = format!("Websocket receive failed, url: {}, reason: {:?}", url, e);
                                                                let error = Error::new(ErrorKind::Other, reason.clone());
                                                                *result.0.borrow_mut() = Some(Err(error)); //设置接收消息的结果
                                                                ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步接收消息的任务
                                                                error!("{}", reason);
                                                            },
                                                            Some(ReceivedMessage::Completed(msg)) => {
                                                                //接收消息完整
                                                                match msg {
                                                                    Message::Close(None) => {
                                                                        //接收到关闭消息
                                                                        ws.0.handler.on_close(CloseCode::Normal, "");
                                                                    },
                                                                    Message::Close(Some(reason)) => {
                                                                        //接收到关闭消息
                                                                        let url = ws.0.status.read().get_url().clone();
                                                                        let protocols = ws.0.status.read().get_protocols().to_vec();
                                                                        *ws.0.status.write() = AsyncWebsocketStatus::Closed(url, protocols, Some(reason.code)); //设置连接状态
                                                                        if let Some(desc) = reason.description {
                                                                            //有关闭的原因
                                                                            ws.0.handler.on_close(reason.code, desc.as_str());
                                                                        } else {
                                                                            ws.0.handler.on_close(reason.code, "");
                                                                        }
                                                                    },
                                                                    msg => {
                                                                        //接收到其它消息
                                                                        ws.0.handler.on_message(msg);
                                                                    },
                                                                }

                                                                //接收到一条消息，则继续异步接收剩余消息，并立即结束当前接收
                                                                let ws_copy = ws.clone();
                                                                if let Some(count) = require_count {
                                                                    //通过消息队列投递继续接收的任务，避免接收长时间独占Websocket连接的运行时
                                                                    sender.send_async(AsyncWebsocketCmd::Receive(
                                                                        ws_copy,
                                                                        task_id,
                                                                        Some(count - 1),
                                                                        received_count + 1,
                                                                        None,
                                                                        receive_timeout,
                                                                        result)).await;
                                                                } else {
                                                                    sender.send_async(AsyncWebsocketCmd::Receive(
                                                                        ws_copy,
                                                                        task_id,
                                                                        None,
                                                                        received_count + 1,
                                                                        None,
                                                                        receive_timeout,
                                                                        result)).await;
                                                                }
                                                            },
                                                            _ => {
                                                                //接收消息帧失败，不应该进入此分支，则立即退出本次消息接收
                                                                let url = ws.0.status.read().get_url().clone();
                                                                let reason = format!("Websocket receive failed, url: {}, reason: unknow", url);
                                                                let error = Error::new(ErrorKind::Other, reason.clone());
                                                                *result.0.borrow_mut() = Some(Err(error)); //设置接收消息的结果
                                                                ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步接收消息的任务
                                                                error!("{}", reason);
                                                            },
                                                        }
                                                    }
                                                },
                                            }
                                        } else {
                                            //有本次接收的最大消息数限制或无本次接收的最大消息限制，当连接流结束时立即退出本次消息接收
                                            *result.0.borrow_mut() = Some(Ok(())); //设置接收消息的结果
                                            ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步接收消息的任务
                                        }
                                    } else {
                                        //当接收超时，则立即退出本次消息接收
                                        *result.0.borrow_mut() = Some(Ok(())); //设置接收消息的结果
                                        ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步接收消息的任务
                                    }
                                }
                            }
                        }
                    },
                    AsyncWebsocketCmd::Close(ws,
                                             task_id,
                                             close_code,
                                             result) => {
                        //关闭当前连接
                        if let Some(ws_con) = current_connection.as_mut() {
                            //当前客户端有连接，则立即关闭连接
                            let close_reason = if let Some(code) = &close_code {
                                Some(CloseReason::from(code.clone()))
                            } else {
                                None
                            };

                            if let Err(e) = ws_con.send(ws::Message::Close(close_reason)).await {
                                //发送关闭消息失败
                                let url = ws.0.status.read().get_url().clone();
                                let reason = format!("Close websocket failed, url: {}, reason: {:?}", url, e);
                                let error = Error::new(ErrorKind::Other, reason.clone());
                                *result.0.borrow_mut() = Some(Err(error)); //设置关闭连接的结果
                                ws.0.handler.on_error(reason.clone()); //通知处理器连接错误
                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步关闭连接的任务
                                error!("{}", reason);
                            } else {
                                //发送关闭消息成功
                                let url = ws.0.status.read().get_url().clone();
                                let protocols = ws.0.status.read().get_protocols().to_vec();
                                *ws.0.status.write() = AsyncWebsocketStatus::Closing(url, protocols, close_code); //设置连接状态
                                *result.0.borrow_mut() = Some(Ok(())); //设置关闭连接的结果
                                ws.0.rt.wakeup::<()>(&task_id); //唤醒外部运行时的异步关闭连接的任务
                            }
                        }
                    },
                    AsyncWebsocketCmd::Stop(close_reason) => {
                        //停止客户端运行
                        if let Some(ws_con) = current_connection.as_mut() {
                            ws_con.send(ws::Message::Close(close_reason)).await;
                        }

                        break;
                    },
                }
            },
        }
    }
}

//异步Websocket指令
enum AsyncWebsocketCmd<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> {
    Open(AsyncWebsocket<P, RT>, TaskId, bool, bool, u64),
    ReceiveSeed(AsyncWebsocket<P, RT>, TaskId, String, Option<ReceivedMessage>, u64),
    Send(AsyncWebsocket<P, RT>, TaskId, Message, AsyncWaitResult<()>),
    Receive(AsyncWebsocket<P, RT>, TaskId, Option<usize>, usize, Option<ReceivedMessage>, u64, AsyncWaitResult<()>),
    Close(AsyncWebsocket<P, RT>, TaskId, Option<CloseCode>, AsyncWaitResult<()>),
    Stop(Option<CloseReason>),
}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Debug for AsyncWebsocketCmd<P, RT> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AsyncWebsocketCmd::Open(_, _, _, _, _) => write!(f, "AsyncWebsocketCmd::Open"),
            AsyncWebsocketCmd::ReceiveSeed(_, _, _, _, _) => write!(f, "AsyncWebsocketCmd::ReceiveSeed"),
            AsyncWebsocketCmd::Send(_, _, _, _) => write!(f, "AsyncWebsocketCmd::Send"),
            AsyncWebsocketCmd::Receive(_, _, _, _, _, _, _) => write!(f, "AsyncWebsocketCmd::Receive"),
            AsyncWebsocketCmd::Close(_, _, _, _) => write!(f, "AsyncWebsocketCmd::Close"),
            AsyncWebsocketCmd::Stop(_) => write!(f, "AsyncWebsocketCmd::Stop"),
        }
    }
}

/*
* 异步Websocket连接状态
*/
#[derive(Debug)]
pub enum AsyncWebsocketStatus {
    Connecting(Url, Vec<String>),                               //连接中
    Connected(Url, Vec<String>),                                //已连接
    SyncingSeed(Url, Vec<String>),                              //同步种子中
    Closing(Url, Vec<String>, Option<CloseCode>),               //关闭中
    Closed(Url, Vec<String>, Option<CloseCode>),                //已关闭
    Error(Url, Vec<String>, Option<CloseCode>, Option<Error>),  //错误
}

unsafe impl Send for AsyncWebsocketStatus {}
unsafe impl Sync for AsyncWebsocketStatus {}

impl AsyncWebsocketStatus {
    //是否正在连接中
    pub fn is_connecting(&self) -> bool {
        if let AsyncWebsocketStatus::Connecting(_, _) = self {
            true
        } else {
            false
        }
    }

    //是否已连接
    pub fn is_connected(&self) -> bool {
        if let AsyncWebsocketStatus::Connected(_, _) = self {
            true
        } else {
            false
        }
    }

    //是否正在关闭中
    pub fn is_closing(&self) -> bool {
        if let AsyncWebsocketStatus::Closing(_, _, _) = self {
            true
        } else {
            false
        }
    }

    //是否已关闭
    pub fn is_closed(&self) -> bool {
        if let AsyncWebsocketStatus::Closed(_, _, _) = self {
            true
        } else {
            false
        }
    }

    //是否错误
    pub fn is_error(&self) -> bool {
        if let AsyncWebsocketStatus::Error(_, _, _, error) = self {
            true
        } else {
            false
        }
    }

    //获取错误
    pub fn get_error(&mut self) -> Option<Error> {
        if let AsyncWebsocketStatus::Error(_, _, _, error) = self {
            error.take()
        } else {
            None
        }
    }

    //获取连接的url
    pub fn get_url(&self) -> &Url {
        match self {
            AsyncWebsocketStatus::Connecting(url, _) => url,
            AsyncWebsocketStatus::SyncingSeed(url, _) => url,
            AsyncWebsocketStatus::Connected(url, _) => url,
            AsyncWebsocketStatus::Closing(url, _, _) => url,
            AsyncWebsocketStatus::Closed(url, _, _) => url,
            AsyncWebsocketStatus::Error(url, _, _, _) => url,
        }
    }

    //获取连接的子协议
    pub fn get_protocols(&self) -> &[String] {
        match self {
            AsyncWebsocketStatus::Connecting(_, protocols) => protocols.as_slice(),
            AsyncWebsocketStatus::SyncingSeed(_, protocols) => protocols.as_slice(),
            AsyncWebsocketStatus::Connected(_, protocols) => protocols.as_slice(),
            AsyncWebsocketStatus::Closing(_, protocols, _) => protocols.as_slice(),
            AsyncWebsocketStatus::Closed(_, protocols, _) => protocols.as_slice(),
            AsyncWebsocketStatus::Error(_, protocols, _, _) => protocols.as_slice(),
        }
    }

    //将状态转换为数值
    pub fn to_number(&self) -> isize {
        match self {
            AsyncWebsocketStatus::Connecting(_, _) => 0,
            AsyncWebsocketStatus::Connected(_, _) => 1,
            AsyncWebsocketStatus::Closing(_, _, _) => 2,
            AsyncWebsocketStatus::Closed(_, _, _) => 3,
            AsyncWebsocketStatus::SyncingSeed(_, _) => 10,
            AsyncWebsocketStatus::Error(_, _, _, _) => -1,
        }
    }
}

/*
* 异步Websocket连接
*/
pub struct AsyncWebsocket<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
>(Arc<InnerWebsocket<P, RT>>);

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Send for AsyncWebsocket<P, RT> {}
unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Sync for AsyncWebsocket<P, RT> {}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Clone for AsyncWebsocket<P, RT> {
    fn clone(&self) -> Self {
        AsyncWebsocket(self.0.clone())
    }
}

/*
* 异步Websocket连接同步方法
*/
impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> AsyncWebsocket<P, RT> {
    //构建异步Websocket连接
    fn new(rt: RT,
           sender: Sender<AsyncWebsocketCmd<P, RT>>,
           url: Url,
           protocols: Vec<String>,
           handler: AsyncWebsocketHandler) -> Self {
        let inner = InnerWebsocket {
            rt,
            sender,
            task_id: RwLock::new(None),
            handler,
            status: RwLock::new(AsyncWebsocketStatus::Connecting(url, protocols)),
            origin: RwLock::new(None),
            send_size: RwLock::new(None),
            recv_size: RwLock::new(None),
            masking_strict: RwLock::new(false),
            is_nodelay: RwLock::new(false),
            rng: Mutex::new(None),
        };

        AsyncWebsocket(Arc::new(inner))
    }

    //设置Websocket握手时的Origin，默认没有Origin
    pub fn set_origin(&self, origin: String) {
        *self.0.origin.write() = Some(origin);
    }

    //设置发送帧的最大长度，超过长度的消息在发送时将被自动分帧，单位字节，最小3B，默认64KB
    pub fn set_send_frame_limit(&self, mut size: usize) {
        if size < 3 {
            //发送帧过小
            size = 3;
        }

        //减去Websocket帧头长度
        let real_size = if size < 126 {
            size - 2
        } else if size <= 65535 {
            size - 4
        } else {
            size - 10
        };
        *self.0.send_size.write() = Some(real_size);
    }

    //设置发送帧的最大长度，超过长度的消息在发送时将被自动分帧，单位字节，最小7B，默认64KB，且允许严格的掩码处理，默认不允许
    pub fn set_send_frame_limit_and_enable_strict_masking(&self, mut size: usize) {
        if size < 7 {
            //发送帧过小
            size = 7;
        }

        //减去Websocket帧头长度，包括了掩码长度
        let real_size = if size < 126 {
            size - 6
        } else if size <= 65535 {
            size - 10
        } else {
            size - 14
        };
        *self.0.send_size.write() = Some(real_size);
    }

    //设置接收帧的最大长度，超过长度的消息在接收时将返回错误，单位字节，最小127B，默认64KB
    pub fn set_receive_frame_limit(&self, mut size: usize) {
        if size < 127 {
            //接收帧过小
            size = 127;
        }

        *self.0.recv_size.write() = Some(size);
    }

    //允许立即刷新连接缓冲区，不允许则会由缓冲区自动刷新，默认不允许
    pub fn enable_nodelay(&self) {
        *self.0.is_nodelay.write() = true;
    }

    //设置异步任务id
    pub fn set_task_id(&self, task_id: TaskId) {
        *self.0.task_id.write() = Some(task_id);
    }

    //获取异步Websocket连接的状态
    pub fn get_status(&self) -> isize {
        self.0.status.read().to_number()
    }

    //获取异步Websocket连接的处理器
    pub fn get_handler(&self) -> AsyncWebsocketHandler {
        self.0.handler.clone()
    }
}

/*
* 异步Websocket连接异步方法
*/
impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> AsyncWebsocket<P, RT> {
    //打开异步Websocket连接
    pub async fn open(&self,
                      is_strict: bool,
                      is_sync_seed: bool,
                      timeout: u64) -> Result<()> {
        let rt = self.0.rt.clone();
        let url = self.0.status.read().get_url().clone();
        let protocols = self.0.status.read().get_protocols().to_vec();
        let task_id = self.0.task_id.write().take().unwrap();
        let ws = self.clone();

        let wait_any = rt.clone().wait_any(2);
        wait_any.spawn(rt.clone(),
                       AsyncOpenWebsocket::new(rt.clone(),
                                               task_id,
                                               ws,
                                               is_strict,
                                               is_sync_seed,
                                               timeout).boxed());
        wait_any.spawn(rt.clone(),
                       async move {
                           rt.timeout(timeout as usize).await;
                           Err(Error::new(ErrorKind::TimedOut, format!("Open websocket failed, url: {}, protocols: {:?}, reason: connect timeout", url, protocols)))
                       }.boxed());
        wait_any.wait_result().await
    }

    //发送消息
    pub async fn send(&self, msg: AsyncWebsocketMessage) -> Result<()> {
        let rt = self.0.rt.clone();
        let ws = self.clone();
        let task_id = rt.alloc::<()>();

        let message = match &msg {
            Message::Text(bin) => {
                if let Some(rng) = &mut *self.0.rng.lock().await {
                    //设置了随机数生成器，则分配一个消息id
                    let id = rng.get_u32();
                    let mut buf = BytesMut::with_capacity(bin.len() + 4);
                    buf.put_u32_le(id);
                    let bytes = bin.as_bytes();
                    buf.put_slice(&bytes);
                    unsafe { Message::Text(ByteString::from_bytes_unchecked(buf.freeze())) }
                }
                else {
                    msg
                }
            },
            Message::Binary(bin) => {
                if let Some(rng) = &mut *self.0.rng.lock().await {
                    //设置了随机数生成器，则分配一个消息id
                    let id = rng.get_u32();
                    let mut buf = BytesMut::with_capacity(bin.len() + 4);
                    buf.put_u32_le(id);
                    buf.put_slice(bin);
                    Message::Binary(buf.freeze())
                } else {
                    msg
                }
            },
            _ => msg,
        };

        AsyncWebsocketSend::new(rt, task_id, ws, message).await
    }

    //接收一次消息，可以限制一次最多接收多少消息，None表示将接收缓冲区中所有的消息
    pub async fn receive_once(&self,
                              mut limit: Option<usize>,
                              receive_timeout: u64) -> Result<()> {
        limit = match limit {
            Some(0) => {
                //至少需要接收一个消息
                Some(1)
            },
            any => any,
        };

        let rt = self.0.rt.clone();
        let ws = self.clone();
        let task_id = rt.alloc::<()>();

        AsyncWebsocketReceive::new(rt, task_id, ws, limit, receive_timeout).await
    }

    //关闭异步WebSocket连接
    pub async fn close(&self, code: AsyncWebsocketCloseCode) -> Result<()> {
        let rt = self.0.rt.clone();
        let ws = self.clone();
        let task_id = rt.alloc::<()>();

        AsyncCloseWebsocket::new(rt, task_id, ws, Some(code)).await
    }

    //发送指定指令到客户端所在异步运行时
    async fn send_to_client(&self, cmd: AsyncWebsocketCmd<P, RT>) -> Result<()> {
        if let Err(e) = self.0.sender.send_async(cmd).await {
            return Err(Error::new(ErrorKind::Other, format!("Send to client failed, reason: {:?}", e)));
        }

        Ok(())
    }
}

//内部Websocket连接
struct InnerWebsocket<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> {
    rt:             RT,                                 //外部异步运行时
    sender:         Sender<AsyncWebsocketCmd<P, RT>>,   //连接所在异步运行时的指令发送器
    task_id:        RwLock<Option<TaskId>>,             //异步任务id
    handler:        AsyncWebsocketHandler,              //处理器
    status:         RwLock<AsyncWebsocketStatus>,       //连接状态
    origin:         RwLock<Option<String>>,             //设置握手时的Origin
    send_size:      RwLock<Option<usize>>,              //最大发送帧大小，超过则分帧，单位字节
    recv_size:      RwLock<Option<usize>>,              //最大接收帧大小，超过则错误，单位字节
    masking_strict: RwLock<bool>,                       //是否严格的掩码处理
    is_nodelay:     RwLock<bool>,                       //是否立即刷新连接
    rng:            Mutex<Option<SecureRng>>,           //随机数生成器
}

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Send for InnerWebsocket<P, RT> {}
unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Sync for InnerWebsocket<P, RT> {}

//发送消息帧
#[inline]
fn send_frames(msg: Message, fragment_size: Option<usize>) -> Vec<Message> {
    let mut msgs = Vec::new();
    match msg {
        Message::Text(str) => {
            //文本消息
            if let Some(size) = fragment_size {
                //指定了帧最大长度
                if str.as_bytes().len() > size {
                    //需要分帧
                    let mut is_first_frame = true;
                    let parts = split_parts(Bytes::from(str.into_bytes()), size);
                    let mut parts_len = parts.len();
                    for part in parts {
                        if is_first_frame {
                            //首帧
                            msgs.push(Message::Continuation(Item::FirstText(part)));
                            is_first_frame = false; //已处理首帧
                            parts_len -= 1;
                            continue;
                        }

                        if parts_len == 1 {
                            //尾帧
                            msgs.push(Message::Continuation(Item::Last(part)));
                            break;
                        }

                        //后续帧
                        msgs.push(Message::Continuation(Item::Continue(part)));
                        parts_len -= 1;
                    }

                    return msgs;
                }
            }

            //不需要分帧
            msgs.push(Message::Text(str));
            msgs
        },
        Message::Binary(bin) => {
            //二进制消息
            if let Some(size) = fragment_size {
                //指定了帧最大长度
                if bin.len() > size {
                    //需要分帧
                    let mut is_first_frame = true;
                    let mut msgs = Vec::new();
                    let parts = split_parts(bin, size);
                    let mut parts_len = parts.len();
                    for part in parts {
                        if is_first_frame {
                            //首帧
                            msgs.push(Message::Continuation(Item::FirstBinary(part)));
                            is_first_frame = false; //已处理首帧
                            parts_len -= 1;
                            continue;
                        }

                        if parts_len == 1 {
                            //尾帧
                            msgs.push(Message::Continuation(Item::Last(part)));
                            break;
                        }

                        //后续帧
                        msgs.push(Message::Continuation(Item::Continue(part)));
                        parts_len -= 1;
                    }

                    return msgs
                }
            }

            //不需要分帧
            msgs.push(Message::Binary(bin));
            msgs
        },
        msg => {
            //单帧消息
            msgs.push(msg);
            msgs
        }
    }
}

//分帧
fn split_parts(bin: Bytes, fragment_size: usize) -> Vec<Bytes> {
    let mut parts = Vec::new();

    let mut offset = 0;
    let mut len = offset + fragment_size;
    let mut top = bin.len(); //缓冲区当前长度
    loop {
        top -= (len - offset); //更新缓冲区当前长度
        let part = bin.slice(offset..len);
        if top == 0 {
            //分帧完成，则立即返回
            parts.push(part);
            return parts;
        }

        //加入分帧列表，并更新偏移
        parts.push(part);
        offset = len;
        if top <= fragment_size {
            //剩余的缓冲区当前长度不足，则设置为剩余长度
            len = offset + top;
        } else {
            //剩余的缓冲区当前长度足够，则设置为单帧最大长度
            len = offset + fragment_size;
        }
    }
}

//已接收的消息
enum ReceivedMessage {
    Discomplete(bool, BytesMut),    //不完整的消息，标记是否为二进制消息
    Completed(Message),             //完整的消息
    Err(Error),                     //错误
}

//接收消息帧
#[inline]
fn receive_frames(received: Option<ReceivedMessage>,
                  frame: Frame) -> ReceivedMessage {
    match frame {
        Frame::Continuation(item) => {
            //接收到消息的部分帧
            match item {
                Item::Continue(bytes) => {
                    //接收到消息的后续帧
                    if let Some(ReceivedMessage::Discomplete(is_binary, mut buf)) = received {
                        //已接收到前继帧，则可以继续接收后续帧，并返回合并后的不完整的已接收消息
                        buf.put(bytes);
                        ReceivedMessage::Discomplete(is_binary, buf)
                    } else {
                        //未接收到前继帧，则忽略接收，并立即返回错误
                        ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket next frame failed, reason: require received message")))
                    }
                },
                Item::FirstText(bytes) => {
                    //接收到文本消息的头帧
                    let mut buf = BytesMut::new();
                    buf.put(bytes);
                    ReceivedMessage::Discomplete(false, buf)
                },
                Item::FirstBinary(bytes) => {
                    //接收到二进制消息的头帧
                    let mut buf = BytesMut::new();
                    buf.put(bytes);
                    ReceivedMessage::Discomplete(true, buf)
                },
                Item::Last(bytes) => {
                    //接收到消息的尾帧
                    if let Some(ReceivedMessage::Discomplete(is_binary, mut buf)) = received {
                        //已接收到前继帧，则可以继续接收尾帧，并返回合并后的完整的已接收消息
                        buf.put(bytes);
                        if is_binary {
                            //二进制消息
                            ReceivedMessage::Completed(Message::Binary(Bytes::from(buf.to_vec())))
                        } else {
                            //文本消息
                            match String::from_utf8(buf.to_vec()) {
                                Err(e) => {
                                    //消息不符合utf8编码，则立即返回错误
                                    ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket tail frame failed, reason: {:?}", e)))
                                },
                                Ok(str) => {
                                    ReceivedMessage::Completed(Message::Text(ByteString::from(str)))
                                }
                            }
                        }
                    } else {
                        //未接收到前继帧，则忽略接收，并立即返回错误
                        ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket tail frame failed, reason: require received message")))
                    }
                },
            }
        },
        Frame::Text(bytes) => {
            //接收到完整的文本消息
            match String::from_utf8(bytes.to_vec()) {
                Err(e) => {
                    //消息不符合utf8编码，则立即返回错误
                    ReceivedMessage::Err(Error::new(ErrorKind::InvalidData, format!("Receive websocket single text frame failed, reason: {:?}", e)))
                },
                Ok(str) => {
                    ReceivedMessage::Completed(Message::Text(ByteString::from(str)))
                }
            }
        },
        Frame::Binary(bytes) => {
            //接收到完整的二进制消息
            ReceivedMessage::Completed(Message::Binary(bytes))
        },
        Frame::Ping(bytes) => {
            //接收到服务器发送的Ping消息，客户端暂时不处理由服务器发送的Ping消息
            ReceivedMessage::Completed(Message::Ping(bytes))
        },
        Frame::Pong(bytes) => {
            //接收到Pong消息
            ReceivedMessage::Completed(Message::Pong(bytes))
        },
        Frame::Close(reason) => {
            //接收到服务器发送的关闭消息
            ReceivedMessage::Completed(Message::Close(reason))
        },
    }
}

//异步打开Websocket连接
pub struct AsyncOpenWebsocket<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> {
    rt:             RT,                     //异步运行时
    task_id:        TaskId,                 //异步任务id
    ws:             AsyncWebsocket<P, RT>,  //连接
    is_strict:      bool,                   //是否严格模式
    is_sync_seed:   bool,                   //是否同步种子
    timeout:        u64,                    //连接超时时长
}

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Send for AsyncOpenWebsocket<P, RT> {}
unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Sync for AsyncOpenWebsocket<P, RT> {}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Future for AsyncOpenWebsocket<P, RT> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.ws.0.status.read().is_connected() {
            //已打开Websocket连接，则返回
            return Poll::Ready(Ok(()));
        } else if self.ws.0.status.read().is_error() {
            //打开Websocket连接错误，则返回
            if let Some(e) = self.ws.0.status.write().get_error() {
                return Poll::Ready(Err(e));
            }
        }

        let task_id = self.task_id.clone();
        let ws = self.ws.clone();
        let is_strict = self.is_strict;
        let is_sync_seed = self.is_sync_seed;
        let timeout = self.timeout;
        let cmd = AsyncWebsocketCmd::Open(ws.clone(),
                                          task_id,
                                          is_strict,
                                          is_sync_seed,
                                          timeout);

        //挂起打开连接的异步任务
        let result = self.ws.0.rt.pending(&self.task_id, cx.waker().clone());

        //异步打开Websocket连接
        self.rt.spawn(async move {
            if let Err(e) = ws.send_to_client(cmd).await {
                error!("Open websocket failed, reason: {:?}", e);
            }
        });

        result
    }
}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> AsyncOpenWebsocket<P, RT> {
    //创建异步打开Websocket连接
    pub fn new(rt: RT,
               task_id: TaskId,
               ws: AsyncWebsocket<P, RT>,
               is_strict: bool,
               is_sync_seed: bool,
               timeout: u64) -> Self {
        AsyncOpenWebsocket {
            rt,
            task_id,
            ws,
            is_strict,
            is_sync_seed,
            timeout,
        }
    }
}

//Websocket异步发送消息
pub struct AsyncWebsocketSend<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> {
    rt:         RT,                             //异步运行时
    task_id:    TaskId,                         //异步任务id
    ws:         AsyncWebsocket<P, RT>,          //连接
    msg:        UnsafeCell<Option<Message>>,    //消息
    result:     AsyncWaitResult<()>,            //异步发送消息的结果
}

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Send for AsyncWebsocketSend<P, RT> {}
unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Sync for AsyncWebsocketSend<P, RT> {}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Future for AsyncWebsocketSend<P, RT> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.result.0.borrow_mut().take() {
            //消息已发送，则返回
            return Poll::Ready(result);
        }

        let msg = unsafe { (*self.msg.get()).take().unwrap() };
        let task_id = self.task_id.clone();
        let ws = self.ws.clone();
        let result = self.result.clone();
        let cmd = AsyncWebsocketCmd::Send(ws.clone(),
                                          task_id,
                                          msg,
                                          result);

        //挂起发送消息的异步任务
        let result = self.ws.0.rt.pending(&self.task_id, cx.waker().clone());

        //异步发送消息
        self.rt.spawn(async move {
            if let Err(e) = ws.send_to_client(cmd).await {
                error!("Send websocket failed, reason: {:?}", e);
            }
        });

        result
    }
}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> AsyncWebsocketSend<P, RT> {
    //创建异步打开Websocket连接
    pub fn new(rt: RT,
               task_id: TaskId,
               ws: AsyncWebsocket<P, RT>,
               msg: Message) -> Self {
        AsyncWebsocketSend {
            rt,
            task_id,
            ws,
            msg: UnsafeCell::new(Some(msg)),
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        }
    }
}

//Websocket异步接收消息
pub struct AsyncWebsocketReceive<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> {
    rt:                 RT,                     //异步运行时
    task_id:            TaskId,                 //异步任务id
    ws:                 AsyncWebsocket<P, RT>,  //连接
    limit:              Option<usize>,          //一次最多可以接收多少消息
    receive_timeout:    u64,                    //接收超时时长
    result:             AsyncWaitResult<()>,    //异步接收消息的结果
}

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Send for AsyncWebsocketReceive<P, RT> {}
unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Sync for AsyncWebsocketReceive<P, RT> {}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Future for AsyncWebsocketReceive<P, RT> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.result.0.borrow_mut().take() {
            //消息已接收，则返回
            return Poll::Ready(result);
        }

        let task_id = self.task_id.clone();
        let ws = self.ws.clone();
        let limit = self.limit.clone();
        let receive_timeout = self.receive_timeout;
        let result = self.result.clone();
        let cmd = AsyncWebsocketCmd::Receive(ws.clone(), task_id, limit, 0, None, receive_timeout, result);

        //挂起接收消息的异步任务
        let result = self.ws.0.rt.pending(&self.task_id, cx.waker().clone());

        //异步接收消息
        self.rt.spawn(async move {
            if let Err(e) = ws.send_to_client(cmd).await {
                error!("Receive websocket failed, reason: {:?}", e);
            }
        });

        result
    }
}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> AsyncWebsocketReceive<P, RT> {
    //创建异步打开Websocket连接
    pub fn new(rt: RT,
               task_id: TaskId,
               ws: AsyncWebsocket<P, RT>,
               limit: Option<usize>,
               mut receive_timeout: u64) -> Self {
        if receive_timeout < 1 || receive_timeout > 10 {
            receive_timeout = 1;
        }

        AsyncWebsocketReceive {
            rt,
            task_id,
            ws,
            limit,
            receive_timeout,
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        }
    }
}

//异步关闭Websocket连接
pub struct AsyncCloseWebsocket<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> {
    rt:         RT,                             //异步运行时
    task_id:    TaskId,                         //异步任务id
    ws:         AsyncWebsocket<P, RT>,          //连接
    close_code: UnsafeCell<Option<CloseCode>>,  //关闭状态码
    result:     AsyncWaitResult<()>,            //异步关闭连接的结果
}

unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Send for AsyncCloseWebsocket<P, RT> {}
unsafe impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Sync for AsyncCloseWebsocket<P, RT> {}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> Future for AsyncCloseWebsocket<P, RT> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.result.0.borrow_mut().take() {
            //消息已接收，则返回
            return Poll::Ready(result);
        }

        let close_code = unsafe { (*self.close_code.get()).take().unwrap() };
        let task_id = self.task_id.clone();
        let ws = self.ws.clone();
        let result = self.result.clone();
        let cmd = AsyncWebsocketCmd::Close(ws.clone(),
                                           task_id,
                                           Some(close_code),
                                           result);

        //挂起关闭连接的异步任务
        let result = self.ws.0.rt.pending(&self.task_id, cx.waker().clone());

        //异步关闭Websocket连接
        self.rt.spawn(async move {
            if let Err(e) = ws.send_to_client(cmd).await {
                error!("Close websocket failed, reason: {:?}", e);
            }
        });

        result
    }
}

impl<
    P: AsyncTaskPoolExt<()> + AsyncTaskPool<(), Pool = P>,
    RT: AsyncRuntime<(), Pool = P>,
> AsyncCloseWebsocket<P, RT> {
    //创建异步打开Websocket连接
    pub fn new(rt: RT,
               task_id: TaskId,
               ws: AsyncWebsocket<P, RT>,
               close_code: Option<CloseCode>) -> Self {
        AsyncCloseWebsocket {
            rt,
            task_id,
            ws,
            close_code: UnsafeCell::new(close_code),
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        }
    }
}

/*
* 异步Websocket消息
*/
pub type AsyncWebsocketMessage = Message;

/*
* 异步Websocket关闭状态码
*/
pub type AsyncWebsocketCloseCode = CloseCode;

/*
* 异步Websocket连接处理器
*/
#[derive(Clone)]
pub struct AsyncWebsocketHandler(Arc<RwLock<InnerHandler>>);

unsafe impl Send for AsyncWebsocketHandler {}
unsafe impl Sync for AsyncWebsocketHandler {}

impl Default for AsyncWebsocketHandler {
    fn default() -> Self {
        let inner = InnerHandler {
            on_open_handler: None,
            on_message_handler: None,
            on_close_handler: None,
            on_error_handler: None,
        };

        AsyncWebsocketHandler(Arc::new(RwLock::new(inner)))
    }
}

impl AsyncWebsocketHandler {
    //设置握手成功的回调函数
    pub fn set_on_open(&self, callback: Arc<dyn Fn()>) {
        self.0.write().on_open_handler = Some(callback);
    }

    //设置接收到消息的回调函数
    pub fn set_on_message(&self, callback: Arc<dyn Fn(AsyncWebsocketMessage)>) {
        self.0.write().on_message_handler = Some(callback);
    }

    //设置开始关闭的回调函数
    pub fn set_on_close(&self, callback: Arc<dyn Fn(u16, String)>) {
        self.0.write().on_close_handler = Some(callback);
    }

    //设置错误的回调函数
    pub fn set_on_error(&self, callback: Arc<dyn Fn(String)>) {
        self.0.write().on_error_handler = Some(callback);
    }

    //握手成功的回调
    pub fn on_open(&self) {
        if let Some(callback) = &self.0.read().on_open_handler {
            callback();
        }
    }

    //接收到消息的回调
    fn on_message(&self, msg: Message) {
        if let Some(callback) = &self.0.read().on_message_handler {
            callback(msg);
        }
    }

    //连接关闭的回调
    fn on_close(&self, code: CloseCode, reason: &str) {
        if let Some(callback) = &self.0.read().on_close_handler {
            callback(code.into(), reason.to_string());
        }
    }

    //错误的回调
    fn on_error(&self, e: String) {
        if let Some(callback) = &self.0.read().on_error_handler {
            callback(e);
        }
    }
}

//内部连接处理器
struct InnerHandler {
    on_open_handler:    Option<Arc<dyn Fn()>>,                      //握手成功的回调函数
    on_message_handler: Option<Arc<dyn Fn(AsyncWebsocketMessage)>>, //接收到消息的回调函数
    on_close_handler:   Option<Arc<dyn Fn(u16, String)>>,           //关闭的回调函数
    on_error_handler:   Option<Arc<dyn Fn(String)>>,                //错误的回调函数
}

//不验证证书
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: SystemTime,
    ) -> GenResult<ServerCertVerified, TLSError> {
        Ok(ServerCertVerified::assertion())
    }
}
