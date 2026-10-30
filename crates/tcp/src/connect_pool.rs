//! TCP连接池及可选发送队列长度观测。
//!
//! 原`runtime_metrics`仅控制`run`首个send前的外部Local冷登记，不负责采集/导出。
//! 默认关闭的`write_queue_metrics`则每15秒尽力记录本池共享发送通道的请求项数。
//! 宿主必须在绑定监听器前安装同一份OpenTelemetry API对应的Provider，并负责采集、
//! 导出和数据过期；本模块不安装Provider、创建采集任务/线程或修改原关闭流程。
//!
//! 启用方式是在本库依赖声明中添加`features = ["write_queue_metrics"]`；Cargo会合并
//! 同一依赖构建中的feature，不能按连接动态切换。仪表名为`pi_tcp.write_queue.messages`，
//! 单位`{message}`，来源名为`pi_tcp.connection_pool`。`process.pid`、`rt.name`、
//! `pi_tcp.pool.id`和`target`是普通数据点标签；同一来源/版本下各池共用同名仪表，
//! 后端可按`target`过滤各池，不要求把来源属性转换成查询标签。
//! 运行时名沿用本库登记时的原名，不是OS线程名，也不追随上游合并别名的变化。
//!
//! 数量不包含已取走批次、Socket/TLS/内核缓冲或字节数，不是周期峰值、送达确认或
//! 运行时任务数。到期只在原循环有机会时采一次并重新计时，不补采、不多采、不保证
//! 准点；停止后可能仍显示SDK保留的历史值，不能据此判断池存活或伪记为零。
//!
//! 未启用时不编译新增状态/分支；启用后正常轮仅本地比较，不增加读时钟、锁或分配，
//! 实际采样另有原通道原子重试、SDK标签哈希/同步锁及首次标签建项成本；并非严格
//! 非阻塞，也不保证1毫秒上界。共享仪表的标签容量和旧池数据过期由宿主SDK管理，
//! 宿主须为累计池数配置足够容量。冷准备、任意自定义Provider/View及系统调度也不受
//! 固定耗时保证。新增观测不访问业务Context或增加unsafe，
//! 不扩展原池的线程使用合同，也不修订原有收发/关闭语义。
//!
//! 原登记测试：`batch_runtime_metrics_tests`；新增状态单元：`write_queue_metrics_tests`；
//! 真实TCP/SDK专项：`tests/write_queue_metrics_network.rs`；实际OTLP及记录耗时专项：
//! `tests/write_queue_metrics_otlp.rs`；性能：`benches/write_queue_metrics.rs`。

use std::rc::Rc;
use std::sync::Arc;
use std::cell::UnsafeCell;
use std::time::{Duration, Instant};
use std::net::{Shutdown, SocketAddr};
use std::io::{ErrorKind, Result, Error};

use futures::future::{FutureExt, LocalBoxFuture};
use mio::{Events, Poll, Token, Interest};
use crossbeam_channel::{Sender, Receiver, unbounded};
use bytes::BufMut;
use slotmap::{Key as SlotMapKey, KeyData as SlotMapKeyData};
use spin_sleep_util::{self, Interval};
use log::{warn, error};

#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
use std::sync::{OnceLock, atomic::{AtomicUsize, Ordering}};
#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
use opentelemetry::{global, InstrumentationScope, KeyValue, metrics::Gauge};

use pi_async_rt::{lock::spin_lock::SpinLock,
                  rt::{serial::AsyncValue,
                       serial_local_thread::LocalTaskRuntime}};
use pi_hash::XHashMap;
use pi_cancel_timer::Timer;
use slotmap::{Key, DefaultKey, KeyData, SlotMap};

use crate::{Socket, Stream, SocketAdapter, SocketConfig, SocketEvent, SocketDriver,
            utils::register_close_sender};

// 内部设计，不进入API docs。WQM-B02/F02/WQM-G01～03：冷准备/本地状态/SDK记录单向协作。
// run先准备Gauge，首任务真正被owner推进时才建时钟；之后状态只随原pool移动。
// handle_timer复用原时钟只置due，下轮写队列排空前才len/record并重计时。无补采任务、
// 业务回调、接收器Clone、运行时持有或Drop处理；Socket/Context/Token不进入本状态。
// PID/编号只在冷路径访问；固定Scope复用SDK仪表，四个不可变身份标签随本池状态移动。
// 记录伪代码：未due直接返回；否则原Receiver.len→Gauge.record(缓存标签)→now重置。
// 标签不含业务资源，SDK保留数据点不代表pool仍存活；不另建全局Gauge或共享状态表。
#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
const WRITE_QUEUE_METRICS_INTERVAL: Duration = Duration::from_secs(15);
#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
static WRITE_QUEUE_METRICS_PID: OnceLock<u32> = OnceLock::new();
#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
static NEXT_WRITE_QUEUE_POOL_ID: AtomicUsize = AtomicUsize::new(1);

// 冷编号只用于区分同一运行时的不同池，不是连接/任务ID。Relaxed只协调唯一数字，
// 不发布业务内存；checked_add拒绝回绕。单次CAS可因其它冷构造重试，耗尽返回None，
// 不panic或复用旧编号。参数只在本私有职责内借用，测试使用独立计数器而不重置全局。
// 非纯、非幂等，额外空间O(1)，成功时间O(CAS尝试数)，无同步/异步锁和await。
// 验证：write_queue_metrics_tests::test_pool_id_unique_and_exhaustion。
#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
fn next_write_queue_pool_id(sequence: &AtomicUsize) -> Option<usize> {
    sequence.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
        next.checked_add(1)
    }).ok()
}

// 只由run在首send前调用：runtime_id取外部Local原get_id，不把它当指标登记ID。
// 冷缓存PID，分配池编号，准备无池属性的固定来源/共享仪表及四个普通标签。
// 返回Gauge及独占标签数组，不保存业务对象/登记结果；重复构造不申请新的指标名。
// 耗尽仅告警降级None，不改变run返回；SDK API无任意Noop检测/导出确认，不伪称成功。
// 名称构造O(L)时间/空间，SDK另有冷锁/分配/缓存成本；可能同步等待，无await/异步锁。
// 非纯、非幂等；生产必须先安装宿主Provider，不捕获自定义Provider的panic。
// 验证：test_shared_gauge_identity_and_labels、真实TCP网络及OTLP专项。
#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
fn prepare_write_queue_gauge(runtime_id: usize) -> Option<(Gauge<u64>, [KeyValue; 4])> {
    let Some(pool_id) = next_write_queue_pool_id(&NEXT_WRITE_QUEUE_POOL_ID) else {
        warn!("TCP发送队列指标池编号已耗尽，本池不启用长度观测，运行时原ID: {}", runtime_id);
        return None;
    };
    let pid = *WRITE_QUEUE_METRICS_PID.get_or_init(std::process::id);
    let runtime_name = format!("pi_tcp-local-{}", runtime_id);
    let target = format!("{pid}:{runtime_name}:write-queue-{pool_id}");
    let attributes = [
        KeyValue::new("process.pid", i64::from(pid)),
        KeyValue::new("rt.name", runtime_name),
        KeyValue::new("pi_tcp.pool.id", pool_id.to_string()),
        KeyValue::new("target", target),
    ];
    let scope = InstrumentationScope::builder("pi_tcp.connection_pool")
        .with_version(env!("CARGO_PKG_VERSION"))
        .build();
    let gauge = global::meter_with_scope(scope).u64_gauge("pi_tcp.write_queue.messages")
        .with_unit("{message}")
        .with_description("本连接池发送通道中尚未取走的请求项数量")
        .build();
    Some((gauge, attributes))
}

// 每池唯一的私有采样状态，不是登记结果包装/任务表。gauge是SDK记录能力；
// F03：attributes用本池唯一所有的Box缓存四个只读普通标签；冷分配一次，
// 后续状态移动不搬动标签数组，常规记录只借用，不Clone/格式化，不加共享计数。
// last_sample是首推进或上次记录调用返回后的时刻；due只承接到下轮排空前的一次尝试。
// 本地独占&mut访问，无Clone/自定义Drop/裸指针或业务资源；仅开启feature时存在。
#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
struct WriteQueueMetrics {
    gauge: Gauge<u64>,
    attributes: Box<[KeyValue; 4]>,
    last_sample: Instant,
    due: bool,
}

#[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
impl WriteQueueMetrics {
    // gauge/attributes按值接收，started由首任务提供真实Instant；不record虚构零值。
    // 固定四项为O(1)时间/空间，新增一次冷Box分配；分配器可能同步等待，内存不足
    // 按全局分配器策略处理，不新增可恢复业务错误。无读时钟/业务锁/await；随原状态释放。
    // 验证：test_cached_labels_survive_state_move及原到期/无初值单元。
    fn new(gauge: Gauge<u64>, attributes: [KeyValue; 4], started: Instant) -> Self {
        Self { gauge, attributes: Box::new(attributes), last_sample: started, due: false }
    }

    // now仅复用原定时器位置读数；未到期不变，满15秒置due且不累计，不回调/取长。
    // 时间倒退按饱和差处理；同步O(1)，无读时钟/锁/分配/await。重复标记幂等。
    // 验证：test_due_boundaries_and_reset、test_late_mark_has_no_catch_up。
    fn mark_due(&mut self, now: Instant) {
        if !self.due && now.saturating_duration_since(self.last_sample) >= WRITE_QUEUE_METRICS_INTERVAL {
            self.due = true;
        }
    }

    // receiver只是本池原通道短借用，不Clone/保存/取走消息；未due不调用len或SDK。
    // due时len→缓存标签record→低频now重计时→清due，无await或新增业务锁。
    // 时间O(1)未到期，采样O(R+V)取决于通道重试和SDK出口；本地额外空间O(1)，
    // 不主动创建容器/字符串或复制消息；SDK的非空标签表可能同步等待，首次建项可分配。
    // SDK共享标签容量由宿主配置，不保证固定阻塞上界，不在本函数捕获Provider panic。
    // 非纯、非幂等；返回仅说明本地尝试结束，不代表导出/送达。不重入同一份&mut状态，
    // 不增加取消/关闭/Drop操作。验证：test_records_without_consuming_queue及上述时间测试。
    fn record_if_due(&mut self, receiver: &Receiver<(Token, Vec<u8>)>) {
        if !self.due { return; }
        let count = receiver.len() as u64;
        self.gauge.record(count, self.attributes.as_ref());
        self.last_sample = Instant::now();
        self.due = false;
    }
}

// 测试专用纯观测，不持核心/锁/回调，不进入普通生产构建。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static TCP_METRICS_TEST_RESULT: std::cell::RefCell<Option<(usize, String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod batch_runtime_metrics_tests {
    //! 仅验证TcpSocketPool::run真实首投递前的登记参数/Ok，不打开网络或执行池循环。
    //! 使用真实原生Local及真实空池/驱动；原首任务已排队，局部单元不宣称网络或关闭验收。
    //! 原Local没有公开取消排队任务接口，未推进的首任务由独立测试进程退出回收，不新增
    //! 生产清理入口或测试侧伪造排空；登记自有ID在参数断言前显式注销。

    use super::*;
    use crate::{connect::TcpSocket, server::PortsAdapter};

    /// 被测生产：TcpSocketPool::run，不手动预登记，不将run的Ok冒充登记Ok。
    /// 只分配真实mio轮询器，不绑定端口；不跨所属线程调用本地队列方法，无采集/SDK。
    /// 命名核对O(L)，测试入口严格串行/有界，不保护原事件循环或资源析构的全部语义。
    #[test]
    fn test_tcp_pool_registers_runtime_metrics() {
        TCP_METRICS_TEST_RESULT.with(|value| { value.borrow_mut().take(); });
        let runner = pi_async_rt::rt::serial_local_thread::LocalTaskRunner::<()>::new();
        let runtime = runner.get_runtime();
        let expected_id = runtime.get_id();
        let (_sender, receiver) = crossbeam_channel::unbounded::<TcpSocket>();
        let pool = TcpSocketPool::<TcpSocket, PortsAdapter<TcpSocket>>::new(
            254, "metrics-unit".to_string(), receiver, SocketConfig::default(),
        ).expect("真实空TCP池构造");
        let driver = SocketDriver::<TcpSocket, PortsAdapter<TcpSocket>>::new(&[]);
        pool.run(runtime, driver, 1, Some(0)).expect("原首任务投递");
        let observed = TCP_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (actual_id, actual_name, registration) = observed.expect("原run必须实际登记");
        assert_eq!(actual_id, expected_id);
        assert_eq!(actual_name, format!("pi_tcp-local-{}", expected_id));
        assert!(registration.is_ok(), "真实登记必须成功: {:?}", registration);
    }
}

/// TCP连接池；`runtime_metrics`冷登记及可选`write_queue_metrics`的前提和成本见模块说明。
/// 新观测只借用本池共享发送通道，不改变收发/关闭合同或增加公开状态查询。
pub struct TcpSocketPool<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    uid:            u8,                                                             //Tcp连接池唯一id
    name:           String,                                                         //Tcp连接池名称
    config:         SocketConfig,                                                   //Tcp连接配置
    poll:           Rc<UnsafeCell<Poll>>,                                           //Socket事件轮询器
    sockets:        Arc<SpinLock<SlotMap<DefaultKey, Option<Arc<UnsafeCell<S>>>>>>, //Socket连接表
    map:            XHashMap<SocketAddr, Token>,                                    //Socket映射表
    driver:         Option<SocketDriver<S, A>>,                                     //Socket驱动
    socket_recv:    Receiver<S>,                                                    //Socket接收器
    write_sent:     Sender<(Token, Vec<u8>)>,                                       //写事件的发送器
    write_recv:     Receiver<(Token, Vec<u8>)>,                                     //写事件的接收器
    close_sent:     Sender<(Token, Result<()>)>,                                    //关闭事件的发送器
    close_recv:     Receiver<(Token, Result<()>)>,                                  //关闭事件的接收器
    duration:       Instant,                                                        //定时器持续时间
    timer:          Timer<(Token, SocketEvent), 100, 60, 3>,                        //定时器
    timer_sent:     Sender<(Token, Option<(usize, SocketEvent)>)>,                  //定时器设置事件的发送器
    timer_recv:     Receiver<(Token, Option<(usize, SocketEvent)>)>,                //定时器设置事件的接收器
    #[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
    write_queue_metrics: Option<WriteQueueMetrics>,                               //仅新feature的本池状态
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for TcpSocketPool<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for TcpSocketPool<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> TcpSocketPool<S, A> {
    /// 构建一个Tcp连接池
    pub fn new(uid: u8,
               name: String,
               receiver: Receiver<S>,
               config: SocketConfig,
    ) -> Result<Self> {
        Self::with_capacity(uid, name, receiver, config, 10)
    }

    /// 构建一个指定初始大小的Tcp连接池
    pub fn with_capacity(uid: u8,
                         name: String,
                         receiver: Receiver<S>,
                         config: SocketConfig,
                         size: usize) -> Result<Self> {
        let contexts = Arc::new(SpinLock::new(SlotMap::with_capacity(size)));
        let map = XHashMap::default();

        let poll = Rc::new(UnsafeCell::new(Poll::new()?));

        let (write_sent, write_recv) = unbounded();
        let (close_sent, close_recv) = unbounded();
        let (timer_sent, timer_recv) = unbounded();
        register_close_sender(uid, close_sent.clone()); //注册全局关闭事件发送器
        let duration = Instant::now();

        Ok(TcpSocketPool {
            uid,
            name,
            config,
            poll,
            sockets: contexts,
            map,
            driver: None,
            socket_recv: receiver,
            write_sent,
            write_recv,
            close_sent,
            close_recv,
            duration,
            timer: Timer::<(Token, SocketEvent), 100, 60, 3>::default(),
            timer_sent,
            timer_recv,
            #[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
            write_queue_metrics: None,
        })
    }

    /// 运行Tcp连接池，并设置Socket驱动
    ///
    /// rt是外部已准备的原生Local，driver移动到原池，event_size/timeout保持原事件容量/
    /// 轮询时间单位及合法范围；成功返回Ok仅表示原首任务已投递，不表示任务或采样完成。
    /// 默认冷登记pi_tcp-local-原get_id，失败只告警继续，不增加业务错误、ID字段或注销。
    /// 名称O(L)时间/空间，另有上游登记/原send及logger成本；同步锁可能等待，无新增await/
    /// 异步锁/线程。只在冷启动使用，原send跨线程边界不变，不能从外部线程改用spawn。
    /// 登记由上游强保活，不转移外部核心所有权；原关闭/Drop不因本次增量自动注销。
    /// 生产参数/Ok：batch_runtime_metrics_tests::test_tcp_pool_registers_runtime_metrics；
    /// 原真实网络启动边界另由tests/connect_pool_startup.rs保护，不属本单元结论。
    /// 开启write_queue_metrics时，额外在首send前冷准备仪表；宿主须在本调用前安装
    /// 同副本Provider。首任务被owner推进时才开始15秒计时，返回Ok仍不证明采样/导出。
    /// 冷编号耗尽只告警并跳过本池观测，不增加业务错误。新增字段/循环成本仅该feature
    /// 存在，精确口径/线程/历史值边界见模块说明；真实装配见write_queue_metrics_network。
    pub fn run(self,
               rt: LocalTaskRuntime<()>,
               driver: SocketDriver<S, A>,
               event_size: usize,
               timeout: Option<usize>) -> Result<()> {
        let mut pool = self;
        pool.driver = Some(driver);
        #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
        {
            let metric_name = format!("pi_tcp-local-{}", rt.get_id());
            let registration = pi_async_rt::runtime_metrics::register_runtime_metrics(&rt, &metric_name);
            #[cfg(test)]
            TCP_METRICS_TEST_RESULT.with(|value| {
                *value.borrow_mut() = Some((rt.get_id(), metric_name.clone(), registration));
            });
            if let Err(error) = registration {
                warn!("TCP 运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
            }
        }
        #[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
        let write_queue_gauge = prepare_write_queue_gauge(rt.get_id());
        let rt_copy = rt.clone();
        // `run` 由监听器启动流程调用，不保证运行在本地运行时所属线程上。
        // 因此首个连接池事件循环必须通过线程安全的外部队列投递；
        // 当该异步任务已经在所属线程上被轮询后，`event_loop` 内部仍可继续使用
        // 本地 `spawn` 完成自调度。
        rt.send(async move {
            #[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
            {
                pool.write_queue_metrics = write_queue_gauge.map(|(gauge, attributes)| {
                    WriteQueueMetrics::new(gauge, attributes, Instant::now())
                });
            }
            //启动Tcp连接事件循环
            let (helper, poll_timeout) = if let Some(poll_timeout) = timeout {
                if cfg!(windows) && poll_timeout < 15000 {
                    (spin_sleep_util::interval(Duration::from_micros(poll_timeout as u64)),
                     Some(Duration::from_micros(0)))
                } else {
                    (spin_sleep_util::interval(Duration::from_micros(poll_timeout as u64)),
                     Some(Duration::from_micros(poll_timeout as u64)))
                }
            } else {
                (spin_sleep_util::interval(Duration::from_micros(100)),
                 Some(Duration::from_millis(0)))
            };
            event_loop(rt_copy,
                       pool,
                       event_size,
                       helper,
                       poll_timeout).await;
        });

        Ok(())
    }
}

// Tcp连接事件循环，当连接同时关注可读和可写事件时，轮询将不会返回任何事件，轮询超时时长会影响到最快响应时间，最快响应时间是实际处理时间加上轮询超时时长
#[inline]
fn event_loop<S, A>(rt: LocalTaskRuntime<()>,
                    mut pool: TcpSocketPool<S, A>,
                    event_size: usize,
                    mut helper: Interval,
                    poll_timeout: Option<Duration>) -> LocalBoxFuture<'static, ()>
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    async move {
        let mut events = Events::with_capacity(event_size);
        handle_accepted(&rt, &mut pool).await;

        handle_write_event(&mut pool).await;

        unsafe {
            let mut is_interrupted = false;
            if let Err(e) = (&mut *pool
                .poll
                .get())
                .poll(&mut events, poll_timeout.clone()) {
                if e.kind() != ErrorKind::Interrupted {
                    //轮询连接事件错误，则立即退出Tcp连接事件循环
                    error!("Tcp socket pool poll failed, timeout: {:?}, ports: {:?}, reason: {:?}",
                        poll_timeout,
                        pool.name,
                        e);
                    return;
                }

                is_interrupted = true;
            } else {
                is_interrupted = false;
            }

            if is_interrupted {
                //轮询连接事件被中断，则继续异步调用Tcp连接事件循环
                let event_loop = event_loop(rt.clone(),
                                            pool,
                                            event_size,
                                            helper,
                                            poll_timeout);
                rt.spawn(event_loop);
                return;
            }
        }

        handle_poll_events(&rt, &mut pool, &events).await;

        handle_close_event(&rt, &mut pool).await;

        handle_timer(&mut pool).await; //必须在关闭处理完成后执行

        //继续异步调用Tcp连接事件循环
        if cfg!(windows) || poll_timeout.is_some_and(|time| time.is_zero()) {
            helper.tick_no_spin(); //开始休眠
        }
        let event_loop = event_loop(rt.clone(),
                                    pool,
                                    event_size,
                                    helper,
                                    poll_timeout);
        rt.spawn(event_loop);
    }.boxed_local()
}

// 处理已接受的Tcp连接
#[inline]
async fn handle_accepted<S, A>(rt: &LocalTaskRuntime<()>,
                               pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    let socket_opts = pool.config.option();

    let mut sockets = pool.socket_recv.try_iter().collect::<Vec<S>>();
    if sockets.len() == 0 {
        match pool.socket_recv.try_recv() {
            Err(e) if e.is_disconnected() => {
                //接受器的发送器已失效，则立即退出当前连接运行时
                let _ = rt.clone().close();
                error!("Handle accepted failed, reason: {:?}", e);
                return;
            },
            Err(_) => (),
            Ok(socket) => {
                sockets.push(socket);
            },
        }
    }

    for mut socket in sockets {
        //接受的新的Tcp连接
        let id = pool
            .sockets
            .lock()
            .insert(None);
        let token = Token(id.data().as_ffi() as usize);

        //注册指定连接的轮询事件，暂时不关注读写事件，等待上层通知后，开始关注读写事件
        let ready = socket
            .get_interest()
            .expect(format!("Handle accepted falied, token: {:?}, reason: invalid inited interest",
                    token).as_str());
        let remote = socket.get_remote().clone();
        pool.map.insert(remote, token);
        unsafe {
            if let Err(e) = (&mut *pool
                .poll
                .get())
                .registry()
                .register(socket.get_stream_mut(), token, ready) {
                //连接注册失败
                warn!("Tcp socket poll register error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                token,
                socket.get_remote(),
                socket.get_local(),
                e);

                //立即关闭未注册的连接
                if let Err(e) = socket.close(Err(Error::new(ErrorKind::Other, "register socket failed"))) {
                    warn!("Tcp socket close error, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                    token,
                    socket.get_remote(),
                    socket.get_local(),
                    e);
                }
            } else {
                //连接注册成功
                socket.set_write_listener(Some(pool.write_sent.clone())); //为注册成功的连接绑定写事件监听器
                socket.set_close_listener(Some(pool.close_sent.clone())); //为注册成功的连接绑定关闭事件监听器
                socket.set_timer_listener(Some(pool.timer_sent.clone())); //为注册成功的连接绑定定时事件监听器
                socket.set_runtime(rt.clone()); //为注册成功的连接绑定运行时
                socket.set_token(Some(token)); //为注册成功的连接绑定新的令牌
                socket.set_uid(((pool.uid as usize) << 56) | token.0); //为注册成功的连接设置唯一id
                socket.set_poll(pool.poll.clone()); //为注册成功的连接设置轮询器
                let socket_arc = Arc::new(UnsafeCell::new(socket));
                let handle = {
                    unsafe {
                        (&mut *socket_arc.get()).set_handle(&socket_arc); //设置连接句柄
                        (&mut *socket_arc.get()).get_handle()
                    }
                };
                // D-LOCAL-04-SHARED：先结束S借用（borrow），再在实际所属线程（owner thread）
                // 准备同一绑定（binding）；成功后才发布活动项（active entry）及建连回调（connected callback）。
                // 关联context_sequence、BIND-U05/U06及BL-V01～V06；不引入整体停止截断（admission cutoff）。
                if let Err(error) = handle.get_context_binding().prepare_context() {
                    warn!("Tcp context preparation rejected, token: {:?}, reason: {:?}", token, error);
                    // 只撤销本次完整键（full key）占位及仍属于本次令牌（token）的映射（mapping）。
                    // 原注册失败分支（registration failure branch）不因本次适配而重构。
                    let reserved = { pool.sockets.lock().remove(id) };
                    if pool.map.get(&remote) == Some(&token) {
                        pool.map.remove(&remote);
                    }
                    let internal_handle = {
                        let current = &mut *socket_arc.get();
                        if let Err(error) = (&mut *pool.poll.get()).registry().deregister(current.get_stream_mut()) {
                            warn!("Tcp unpublished socket deregistration failed, token: {:?}, reason: {:?}", token, error);
                        }
                        if let Err(error) = current.get_stream_ref().shutdown(Shutdown::Both) {
                            warn!("Tcp unpublished socket shutdown failed, token: {:?}, reason: {:?}", token, error);
                        }
                        current.remove_handle()
                    };
                    // 所有表锁（table lock）和连接借用（socket borrow）结束后归还本地持有。
                    // 已发布却未解除的其它绑定（binding）沿原保留合同，不在这里跨线程（thread）触碰Rc。
                    drop(reserved);
                    drop(internal_handle);
                    drop(handle);
                    drop(socket_arc);
                    continue;
                }
                pool.sockets.lock()[id] = Some(socket_arc); //加入连接池上下文

                //异步调用连接回调任务
                let connected = pool
                    .driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .connected(Ok(handle));
                rt.spawn(connected);
            }
        }
    }
}

//处理Tcp连接的写事件
#[inline]
async fn handle_write_event<S, A>(pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    #[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
    if let Some(metrics) = pool.write_queue_metrics.as_mut() {
        // 原通道取走前观测，不把len预先求值放到每轮热路径。
        metrics.record_if_due(&pool.write_recv);
    }
    for (token, buf) in pool
        .write_recv
        .try_iter()
        .collect::<Vec<(Token, Vec<u8>)>>() {
        //当前连接池有写事件
        if let Some(Some(socket)) = pool
            .sockets
            .lock()
            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
            //写事件指定的连接存在
            let interest = if let Some(interest) = unsafe { (&*socket.get()).get_interest() } {
                //当前连接有感兴趣的事件
                interest
            } else {
                //当前连接没有感兴趣的事件
                Interest::READABLE
            };

            //强制重置连接当前感兴趣的事件
            if let Err(e) = unsafe { (&mut *pool
                .poll
                .get())
                .registry()
                .reregister(unsafe { (&mut *socket.get()).get_stream_mut() }, token,
                            interest.add(Interest::WRITABLE)) } {
                //重新注册关注的事件失败，则关闭出错的Tcp连接
                if let Err(e) = unsafe { (&mut *socket.get()).close(Err(e)) } {
                    //关闭指定连接失败
                    unsafe {
                        warn!("Tcp socket close failed by handle write event, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    (&*socket.get()).get_token(),
                                    (&*socket.get()).get_remote(),
                                    (&*socket.get()).get_local(),
                                    e);
                    }
                }
            }

            unsafe {
                if let Some(write_buf) = (&mut *socket.get()).get_write_buffer() {
                    write_buf.put_slice(buf.as_ref());
                }
            }
        }
    }
}

// 处理Tcp连接的轮询事件
#[inline]
async fn handle_poll_events<S, A>(rt: &LocalTaskRuntime<()>,
                                  pool: &mut TcpSocketPool<S, A>,
                                  events: &Events)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    for event in events {
        let token = event.token(); //当前事件的令牌

        if let Some(Some(socket)) = pool
            .sockets
            .lock()
            .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64)))
        {
            let poll = pool.poll.clone();
            let socket_copy = socket.clone();
            let adapter = pool
                .driver
                .as_ref()
                .unwrap()
                .clone_adapter();
            let sockets = pool.sockets.clone();

            if event.is_readable() {
                //可读事件，表示读就绪，则异步处理读事件
                rt.spawn(async move {
                    let mut close_reason = None;
                    {
                        let s = unsafe { (&mut *socket_copy.get()) };
                        match s.recv() {
                            Ok(len) => {
                                //按需接收完成，则重新注册当前Tcp连接关注的事件，并执行已读回调
                                if let Some(interest) = s.get_interest() {
                                    //需要修改当前连接感兴趣的事件类型
                                    if let Err(e) = unsafe { (&mut *poll
                                        .get())
                                        .registry()
                                        .reregister(s.get_stream_mut(),
                                                    token,
                                                    interest) } {
                                        //重新注册关注的事件失败
                                        close_reason = Some(Err(e));
                                    }
                                }

                                if s.is_wait_wakeup_read_ready() {
                                    //当前连接有需要唤醒的异步准备读取器，则唤醒当前的异步准备读取器
                                    s.wakeup_read_ready();
                                } else {
                                    if s.is_hibernated() {
                                        //当前连接已休眠，则将本次读任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                                        let handle = s.get_handle();
                                        s.push_hibernated_task(adapter.readed(Ok(handle)));
                                    } else {
                                        //当前连接没有需要唤醒的异步准备读取器，则调用接收回调
                                        let handle = s.get_handle();
                                        drop(s); //在继续调用前释放锁
                                        adapter.readed(Ok(handle)).await;
                                    }
                                }
                            },
                            Err(e) => {
                                //按需接收失败，准备关闭当前连接
                                close_reason = Some(Err(e));
                            },
                        }
                    }

                    //关闭轮询时出错的Tcp连接
                    if let Some(reason) = close_reason.take() {
                        if let Some(Some(socket)) = sockets
                            .lock()
                            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                            let result = unsafe { (&mut *socket.get()).close(reason) }; //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                unsafe {
                                    warn!("Tcp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    (&*socket.get()).get_token(),
                                    (&*socket.get()).get_remote(),
                                    (&*socket.get()).get_local(),
                                    e);
                                }
                            }
                        }
                    }
                });
            } else if event.is_writable() {
                //可写事件，表示写就绪，则异步处理写事件
                rt.spawn(async move {
                    let mut close_reason = None;
                    {
                        let s = unsafe { (&mut *socket_copy.get()) };
                        match s.send() {
                            Ok(len) => {
                                //发送完成，并执行已写回调
                                if let Some(interest) = s.get_interest() {
                                    // 需要修改当前连接感兴趣的事件类型
                                    unsafe {
                                        if let Err(e) = (&mut *poll
                                            .get())
                                            .registry()
                                            .reregister(s.get_stream_mut(), token, interest) {
                                            //重新注册关注的事件失败
                                            close_reason = Some(Err(e));
                                        }
                                    }
                                }

                                if s.is_hibernated() {
                                    //当前连接已休眠，则将本次写任务加入当前连接的休眠任务队列，等待连接被唤醒后再继续处理
                                    let handle = s.get_handle();
                                    s.push_hibernated_task(adapter.writed(Ok(handle)));
                                } else {
                                    //调用发送回调
                                    let handle = s.get_handle();
                                    drop(s); //在继续调用前释放锁
                                    let writed = adapter.writed(Ok(handle)).await;
                                }
                            },
                            Err(e) => {
                                //发送失败，准备关闭当前连接
                                close_reason = Some(Err(e));
                            },
                        }
                    }

                    //关闭轮询时出错的Tcp连接
                    if let Some(reason) = close_reason.take() {
                        if let Some(Some(socket)) = sockets
                            .lock()
                            .get(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                            let result = unsafe { (&mut *socket.get()).close(reason) }; //保证归还借用的可写引用
                            if let Err(e) = result {
                                //关闭指定连接失败
                                unsafe {
                                    warn!("Tcp socket close failed, token: {:?}, remote: {:?}, local: {:?}, reason: {:?}",
                                    (&*socket.get()).get_token(),
                                    (&*socket.get()).get_remote(),
                                    (&*socket.get()).get_local(),
                                    e);
                                }
                            }
                        }
                    }
                });
            }
        }
    }
}

// 批量处理Tcp连接的关闭事件
// 关闭指定的Tcp连接，并清理上下文
async fn handle_close_event<S, A>(rt: &LocalTaskRuntime<()>,
                                  pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    for (token, reason) in pool
        .close_recv
        .try_iter()
        .collect::<Vec<(Token, Result<()>)>>() {
        //从连接表中移除被关闭的Tcp连接
        if !pool
            .sockets
            .lock()
            .contains_key(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
            // 当前完整键（full key）已移出，只跳过本事件（event），保留同批后续连接（connection）。
            // 关联context_close_sequence：A/A/B及旧完整令牌（full token）之后的新连接（connection）。
            continue;
        }
        let socket = pool
            .sockets
            .lock()
            .remove(DefaultKey::from(KeyData::from_ffi(token.0 as u64)))
            .unwrap()
            .unwrap();

        //从映射表中移除被关闭Tcp连接的信息
        pool.map.remove(unsafe { (&*socket.get()).get_remote() });

        //从轮询器中注销Tcp连接
        let r = unsafe {
            (&mut *pool
                .poll
                .get())
                .registry()
                .deregister(unsafe { (&mut *socket.get()).get_stream_mut() })
        };

        //关闭流
        unsafe {
            (&*socket.get())
                .get_stream_ref()
                .shutdown(Shutdown::Both);
        }

        //移除定时器
        if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
            let _ = pool.timer.cancel(SlotMapKeyData::from_ffi(timer as u64).into());
        }

        //异步执行已关闭回调
        let handle = unsafe { (&*socket.get()).get_handle() };
        // 保留原对象（object）的绑定（binding），不在回调（callback）完成后按旧令牌（token）重新查表。
        let context_binding = handle.get_context_binding();
        let closed = if let Err(e) = reason {
            //因为内部错误，关闭Tcp连接
            pool.driver
                .as_ref()
                .unwrap()
                .get_adapter()
                .closed(Err((handle, e)))
        } else {
            if let Err(e) = r {
                //注销时错误，关闭Tcp连接
                pool.driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .closed(Err((handle, e)))
            } else {
                //正常关闭Tcp连接
                pool.driver
                    .as_ref()
                    .unwrap()
                    .get_adapter()
                    .closed(Ok(handle))
            }
        };
        rt.spawn(async move {
            //执行关闭回调
            closed.await;

            // D-LOCAL-04-LAST-RELEASE：正常回调（callback）完整结束后先取出Context Rc，
            // 再取出原S内部句柄（SocketHandle）；两个内部借用（borrow）均已结束才归还持有。
            // 正常准备与本任务（task）均在同一所属线程（owner thread）；重复解除允许为空。
            // 未完成回调（callback）沿原资源保留（resource retention），不增加取消或补投任务（task）。
            // 关联BL-V01/V02/V04及context_sequence；用户析构（Drop）不得发生在内部借用（borrow）中。
            let detached_context = context_binding.detach_context()
                .expect("Pool context must remain on its owner thread");
            let internal_handle = unsafe { (&mut *socket.get()).remove_handle() };
            drop(internal_handle);
            drop(detached_context);
        });
    }
}

// 处理Tcp连接的定时器
#[inline]
async fn handle_timer<S, A>(pool: &mut TcpSocketPool<S, A>)
    where S: Socket + Stream,
          A: SocketAdapter<Connect = S> {
    //设置或取消指定Tcp连接的定时器
    for (token, opt) in pool.timer_recv.try_iter().collect::<Vec<(Token, Option<(usize, SocketEvent)>)>>() {
        if let Some((timeout, event)) = opt {
            //为指定令牌的连接设置指定的定时器
            if let Some(Some(socket)) = pool
                .sockets
                .lock()
                .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                if unsafe { (&*socket.get()).is_closed() } {
                    //连接已关闭，则忽略
                    continue;
                }

                if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
                    //连接已设置定时器，则先移除指定句柄的定时器
                    let _ = pool.timer.cancel(SlotMapKeyData::from_ffi(timer as u64).into());
                }

                //设置指定事件的定时器，并在连接上设置定时器句柄
                let current_time = pool.duration.elapsed().as_millis() as u64;
                let timer = pool.timer.push_time(current_time + timeout as u64, (token, event));
                unsafe { (&mut *socket.get()).set_timer_handle(timer.data().as_ffi() as usize); }
            }
        } else {
            //为指定令牌的连接取消指定的定时器
            if let Some(Some(socket)) = pool
                .sockets
                .lock()
                .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                if unsafe { (&*socket.get()).is_closed() } {
                    //连接已关闭，则忽略
                    continue;
                }

                //移除连接上的定时器句柄，并移除指定句柄的定时器
                if let Some(timer) = unsafe { (&mut *socket.get()).unset_timer_handle() } {
                    let _ = pool.timer.cancel(SlotMapKeyData::from_ffi(timer as u64).into());
                }
            }
        }
    }

    //轮询所有超时的定时器，执行已超时回调
    #[cfg(not(all(feature = "write_queue_metrics", not(target_arch = "wasm32"))))]
    let current_time = pool.duration.elapsed().as_millis() as u64;
    #[cfg(all(feature = "write_queue_metrics", not(target_arch = "wasm32")))]
    let current_time = {
        // 与原elapsed相同位置只读一次；业务时间和采样判断消费同一个单调读数。
        let now = Instant::now();
        if let Some(metrics) = pool.write_queue_metrics.as_mut() {
            metrics.mark_due(now);
        }
        now.saturating_duration_since(pool.duration).as_millis() as u64
    };
    while let Some((_key, item)) = pool.timer.pop_kv(current_time) {
        //存在超时的回调
        let (token, event) = item;
        if let Some(Some(socket)) = pool
            .sockets
            .lock()
            .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
            if unsafe { (&*socket.get()).is_closed() } {
                //连接已关闭，则忽略
                continue;
            }

            //移除连接上的定时器句柄
            unsafe { (&mut *socket.get()).unset_timer_handle(); }

            //连接已超时
            let handle = unsafe { (&*socket.get()).get_handle() };
            pool.driver
                .as_ref()
                .unwrap()
                .get_adapter()
                .timeouted(handle, event)
                .await;
        }
    }
}

#[cfg(all(test, feature = "write_queue_metrics", not(target_arch = "wasm32")))]
mod write_queue_metrics_tests {
    //! WQM-B02局部状态/身份单元，不冒充真实网络或线上后端验证。
    //! 使用官方SDK真实Gauge；只有时间输入/局部原子编号由单元控制，不修改生产全局编号。
    //! 本测试进程中的Provider由本组独占，必须--test-threads=1；退出恢复Noop并关停SDK。
    //! 真实原TCP装配另见write_queue_metrics_network，不在这里复制SDK聚合/登记算法。

    use super::*;
    use std::collections::BTreeMap;
    use opentelemetry::metrics::{MeterProvider, NoopMeterProvider};
    use opentelemetry_sdk::metrics::{SdkMeterProvider, PeriodicReader, InMemoryMetricExporter};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};

    /// 单元中的进程宿主；Provider/Reader全部来自官方SDK，不增加生产观测出口。
    /// 只在测试冷路径安装，读取时主动flush；无网络，仅验证局部单元，串行使用。
    struct Host {
        provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
    }

    impl Host {
        /// 初始化本测试拥有的唯一Provider；小时级SDK周期避免测试后台自动重复导出。
        fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let reader = PeriodicReader::builder(exporter.clone())
                .with_interval(Duration::from_secs(3600)).build();
            let provider = SdkMeterProvider::builder().with_reader(reader).build();
            global::set_meter_provider(provider.clone());
            Self { provider, exporter }
        }

        /// 原官方SDK的真实报告；先清空测试导出缓存，不能把旧报告计为新采样。
        fn collect(&self) -> Vec<ResourceMetrics> {
            self.exporter.reset();
            self.provider.force_flush().expect("单元SDK采集失败");
            self.exporter.get_finished_metrics().expect("单元SDK报告读取失败")
        }

        /// 只提取本单元的u64 Gauge值；不是生产状态查询API。
        fn values(&self) -> Vec<u64> {
            let mut values = Vec::new();
            for report in self.collect() {
                for scope in report.scope_metrics() {
                    for metric in scope.metrics().filter(|metric| metric.name() == "wqm.unit") {
                        match metric.data() {
                            AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                                for point in gauge.data_points() {
                                    let actual = point.attributes().map(|kv| {
                                        (kv.key.as_str().to_owned(), kv.value.as_str().into_owned())
                                    }).collect::<BTreeMap<_, _>>();
                                    let expected = attributes().into_iter().map(|kv| {
                                        (kv.key.as_str().to_owned(), kv.value.as_str().into_owned())
                                    }).collect::<BTreeMap<_, _>>();
                                    assert_eq!(actual, expected);
                                    values.push(point.value());
                                }
                            }
                            other => panic!("单元仪表类型不符: {other:?}"),
                        }
                    }
                }
            }
            values
        }

        /// 本单元的独立普通Gauge，不手工实现Measure/Reader回调替代生产能力。
        fn gauge(&self) -> Gauge<u64> {
            self.provider.meter("wqm.unit").u64_gauge("wqm.unit").build()
        }
    }

    /// 单元专用固定普通标签；不申请生产编号、不访问池或运行时，避免污染身份测试。
    fn attributes() -> [KeyValue; 4] {
        [KeyValue::new("process.pid", i64::from(std::process::id())),
            KeyValue::new("rt.name", "unit-local"), KeyValue::new("pi_tcp.pool.id", "unit"),
            KeyValue::new("target", "unit-target")]
    }

    impl Drop for Host {
        fn drop(&mut self) {
            global::set_meter_provider(NoopMeterProvider::new());
            if let Err(error) = self.provider.shutdown() {
                if !std::thread::panicking() { panic!("单元SDK关停失败: {error}"); }
            }
        }
    }

    /// 被测next_write_queue_pool_id：冷并发唯一和极值耗尽；不污染/重置全局编号。
    /// 四个线程只共享测试原子量，不传递Local/Socket/Context，数据量固定64项。
    #[test]
    fn test_pool_id_unique_and_exhaustion() {
        let sequence = Arc::new(AtomicUsize::new(1));
        let workers = (0..4).map(|_| {
            let sequence = sequence.clone();
            std::thread::spawn(move || (0..16).map(|_| {
                next_write_queue_pool_id(&sequence).expect("冷编号不应耗尽")
            }).collect::<Vec<_>>())
        }).collect::<Vec<_>>();
        let joined = workers.into_iter().map(|worker| worker.join()).collect::<Vec<_>>();
        let mut ids = joined.into_iter().flat_map(|result| result.expect("冷编号线程失败")).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, (1..=64).collect::<Vec<_>>());
        let exhausted = AtomicUsize::new(usize::MAX - 1);
        assert_eq!(next_write_queue_pool_id(&exhausted), Some(usize::MAX - 1));
        assert_eq!(next_write_queue_pool_id(&exhausted), None);
        assert_eq!(next_write_queue_pool_id(&exhausted), None);
        assert_eq!(exhausted.load(Ordering::Relaxed), usize::MAX);
    }

    /// 被测new/mark_due/record_if_due：到期前、恰好15秒、记录后重计时及不消费消息。
    /// 合成now仅作为纯时间状态单元输入；生产循环和真实网络专项均不加速时钟。
    #[test]
    fn test_due_boundaries_and_reset() {
        let host = Host::new();
        let start = Instant::now();
        let mut metrics = WriteQueueMetrics::new(host.gauge(), attributes(), start);
        let (_sender, receiver) = unbounded();
        metrics.mark_due(start);
        metrics.mark_due(start + Duration::from_millis(14_999));
        assert!(!metrics.due);
        metrics.record_if_due(&receiver);
        assert_eq!(metrics.last_sample, start);
        assert!(host.values().is_empty(), "到期前不得记录");
        metrics.mark_due(start + WRITE_QUEUE_METRICS_INTERVAL);
        assert!(metrics.due);
        let before = Instant::now();
        metrics.record_if_due(&receiver);
        assert!(!metrics.due);
        assert!(metrics.last_sample >= before && metrics.last_sample <= Instant::now());
        metrics.mark_due(metrics.last_sample + Duration::from_millis(14_999));
        assert!(!metrics.due);
        metrics.mark_due(metrics.last_sample + WRITE_QUEUE_METRICS_INTERVAL);
        assert!(metrics.due);
    }

    /// 被测F03的new/record：强制把整个状态移到另一个堆对象后，缓存标签地址不变，
    /// 状态体积小于内联数组，真实SDK仍保留全部标签及数量。只比较地址，不解引用
    /// 旧地址，不恢复对象；外层Box仅为单元强制移动，不进入生产或代补业务功能。
    #[test]
    fn test_cached_labels_survive_state_move() {
        let host = Host::new();
        let start = Instant::now();
        let metrics = WriteQueueMetrics::new(host.gauge(), attributes(), start);
        let address = metrics.attributes.as_ptr();
        let mut moved = Box::new(metrics);
        assert_eq!(address, moved.attributes.as_ptr());
        assert!(std::mem::size_of::<WriteQueueMetrics>() < std::mem::size_of::<[KeyValue; 4]>());
        let (sender, receiver) = unbounded();
        sender.send((Token(1), vec![7; 256])).unwrap();
        moved.mark_due(start + WRITE_QUEUE_METRICS_INTERVAL);
        moved.record_if_due(&receiver);
        assert_eq!(host.values(), [1]);
        assert_eq!(receiver.len(), 1, "缓存布局不能消费生产通道");
    }

    /// 被测延迟/重复标记：迟到十个周期也仅保留一次due，记录后重新等完整间隔。
    #[test]
    fn test_late_mark_has_no_catch_up() {
        let host = Host::new();
        let start = Instant::now();
        let mut metrics = WriteQueueMetrics::new(host.gauge(), attributes(), start);
        let (_sender, receiver) = unbounded();
        metrics.mark_due(start + Duration::from_secs(150));
        metrics.mark_due(start + Duration::from_secs(300));
        assert!(metrics.due);
        metrics.record_if_due(&receiver);
        let reset = metrics.last_sample;
        metrics.record_if_due(&receiver);
        assert_eq!(metrics.last_sample, reset, "不得立即追赶下一周期");
        metrics.mark_due(reset + Duration::from_secs(14));
        assert!(!metrics.due);
        metrics.mark_due(start.checked_sub(Duration::from_secs(1)).unwrap());
        assert!(!metrics.due, "倒退时间不能制造到期");
    }

    /// 被测record_if_due：数量为请求项而非字节，不取走/重排/修改原队列内容。
    #[test]
    fn test_records_without_consuming_queue() {
        let host = Host::new();
        let start = Instant::now();
        let mut metrics = WriteQueueMetrics::new(host.gauge(), attributes(), start);
        let (sender, receiver) = unbounded();
        for index in 0..3 { sender.send((Token(index), vec![index as u8; 256])).unwrap(); }
        metrics.mark_due(start + WRITE_QUEUE_METRICS_INTERVAL);
        metrics.record_if_due(&receiver);
        assert_eq!(host.values(), [3]);
        assert_eq!(receiver.len(), 3);
        for index in 0..3 {
            let (token, bytes) = receiver.try_recv().unwrap();
            assert_eq!(token, Token(index));
            assert_eq!(bytes, vec![index as u8; 256]);
        }
        assert!(receiver.is_empty());
    }

    /// 被测初始化/空及断开队列：未采样无虚构零；到期真空为零，不延长Sender生命。
    #[test]
    fn test_empty_queue_and_no_initial_sample() {
        let host = Host::new();
        let start = Instant::now();
        let mut metrics = WriteQueueMetrics::new(host.gauge(), attributes(), start);
        assert!(host.values().is_empty());
        let (sender, receiver) = unbounded();
        drop(sender);
        metrics.mark_due(start + WRITE_QUEUE_METRICS_INTERVAL);
        metrics.record_if_due(&receiver);
        assert_eq!(host.values(), [0]);
        assert!(matches!(receiver.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
    }

    /// 被测冷准备的真实SDK身份：同一来源/仪表中两池原名相同，普通标签区分且互不覆盖。
    /// 这里只向私有冷入口提供固定逻辑ID；真实Local的get_id对齐另由网络专项验证。
    #[test]
    fn test_shared_gauge_identity_and_labels() {
        let host = Host::new();
        let (first, first_attrs) = prepare_write_queue_gauge(123).expect("冷准备不应耗尽");
        let (second, second_attrs) = prepare_write_queue_gauge(123).expect("第二池冷准备不应耗尽");
        first.record(5, &first_attrs);
        second.record(11, &second_attrs);
        first.record(7, &first_attrs);
        let mut pools = BTreeMap::new();
        let mut scopes = 0;
        for report in host.collect() {
            for scope in report.scope_metrics().filter(|scope| scope.scope().name() == "pi_tcp.connection_pool") {
                scopes += 1;
                assert_eq!(scope.scope().version(), Some(env!("CARGO_PKG_VERSION")));
                assert_eq!(scope.scope().attributes().count(), 0);
                let metrics = scope.metrics().filter(|metric| metric.name() == "pi_tcp.write_queue.messages")
                    .collect::<Vec<_>>();
                assert_eq!(metrics.len(), 1, "各池必须使用同一仪表");
                let metric = metrics[0];
                assert_eq!(metric.unit(), "{message}");
                let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data() else {
                    panic!("必须是u64 Gauge")
                };
                let points = gauge.data_points().collect::<Vec<_>>();
                assert_eq!(points.len(), 2);
                for point in points {
                    let attrs = point.attributes().map(|value| {
                        (value.key.as_str().to_owned(), value.value.as_str().into_owned())
                    }).collect::<BTreeMap<_, _>>();
                    assert_eq!(attrs.len(), 4);
                    assert_eq!(attrs["process.pid"], std::process::id().to_string());
                    assert_eq!(attrs["rt.name"], "pi_tcp-local-123");
                    assert_eq!(attrs["target"], format!("{}:pi_tcp-local-123:write-queue-{}",
                        std::process::id(), attrs["pi_tcp.pool.id"]));
                    assert!(pools.insert(attrs["pi_tcp.pool.id"].clone(), point.value()).is_none());
                }
            }
        }
        assert_eq!(scopes, 1, "各池必须复用固定来源");
        let mut values = pools.into_values().collect::<Vec<_>>();
        values.sort_unstable();
        assert_eq!(values, [7, 11]);
    }
}
