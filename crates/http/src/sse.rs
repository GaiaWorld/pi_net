//! Server-Sent Events support for `pi_http`.
//!
//! 本模块为 `pi_http` 提供 HTTP/1.1 SSE 支持。SSE 连接必须由客户端通过
//! `EventSource` 或等价 HTTP GET 请求主动建立；服务端不能把任意普通 HTTP
//! keep-alive 连接透明转换为 SSE 连接。
//!
//! # Example
//!
//! ```ignore
//! use pi_http::sse::{SseConfig, SseEvent, SseHub, SseResponse};
//!
//! let (resp, sender) = SseResponse::builder(&req)
//!     .config(SseConfig::default())
//!     .build_with_heartbeat(runtime.clone())?;
//!
//! hub.register(user_id, sender.clone())?;
//! sender.send(SseEvent::data("connected")).await?;
//! ```
//!
//! # Public API model
//!
//! - `SseResponse::builder(&req)` 是从普通 HTTP GET 请求进入 SSE 的唯一入口；
//!   它返回 `HttpResponse` 与每连接一个的 `SseSender`。
//! - `SseMiddleware` 是面向外部 `pi_http` 使用方的默认路由中间件；它封装响应构建、
//!   Hub 注册、打开回调和 stream 响应旁路。
//! - `SseSender` 是轻量 clone 的连接发送句柄，可跨线程和异步任务使用。
//! - `SseHub<K, S>` 是可选的活动连接注册表，按业务 key 或透明
//!   `SseConnectionId` 定向发送。
//! - `SseEvent` 负责标准 SSE 文本帧编码，支持 `event`、`data`、`id`、
//!   `retry` 和 comment。
//! - `try_*` API 全部为同步非阻塞接口，不会 `block_on`、不会启动运行时，也不会等待队列容量。
//!
//! # Boundaries
//!
//! 本模块只提供 HTTP/1.1 `text/event-stream` 长连接发送能力。它不提供事件历史重放、
//! 断线补偿存储、HTTP/2 server push、TLS 类型擦除或普通 HTTP 连接透明升级。`TcpSocket`
//! 与 `TlsSocket` 应分别使用各自的 `SseHub<K, S>` 类型。
//!
//! # Safety and performance
//!
//! `SseSender` and `SseHub` are `Clone + Send + Sync` as long as the underlying
//! `Socket` type satisfies `pi_tcp`'s `Socket` contract. Sending an event is `O(n)`
//! time and `O(n)` space where `n` is the encoded event size. `try_*` methods are
//! synchronous and non-blocking; they never start a runtime and never wait for queue
//! capacity. Async methods use the same bounded response channel and provide
//! asynchronous backpressure. For one `SseSender`, successful send/finish
//! operations are serialized by one gate: sequential calls from the same thread
//! are enqueued in call order, while concurrent calls from different threads or
//! tasks are observed in the order they successfully enter the response queue.
//!
//! # Test entries
//!
//! 生产侧静态逻辑由 `sse_event_builder_and_encoding`、
//! `sse_config_builder_rejects_invalid_values`、`sse_sender_try_send_reports_queue_full`、
//! `sse_hub_register_get_unregister`、`sse_hub_try_send_to_id_reports_result` 等单元测试覆盖。
//! 真实网络路径由 `crates/http/tests/sse_real_network.rs` 中的
//! `sse_real_network_get_stream_receives_chunked_event`、
//! `sse_real_network_same_thread_order_matches_call_order`、
//! `sse_real_network_cross_thread_controlled_enqueue_order_matches_output_order` 和
//! `sse_real_network_acceptor_can_reject_open_request` 覆盖。

use std::error::Error as StdError;
use std::fmt::{self, Display, Formatter};
use std::hash::Hash;
use std::io::{Error as IoError, ErrorKind};
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use dashmap::{mapref::entry::Entry, DashMap};
use futures::{
    future::{FutureExt, LocalBoxFuture},
    lock::Mutex,
};
use https::{
    header::{
        HeaderName, ACCEPT, CACHE_CONTROL, CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH,
        CONTENT_TYPE, TRANSFER_ENCODING,
    },
    Method, StatusCode,
};
use pi_async_rt::rt::AsyncRuntime;
use tcp::{Socket, SocketHandle};
use wyhash::WyHasherBuilder;

use crate::gateway::GatewayContext;
use crate::middleware::{Middleware, MiddlewareResult};
use crate::request::HttpRequest;
use crate::response::{HttpResponse, ResponseHandler};

const DEFAULT_CHANNEL_SIZE: usize = 16;
const DEFAULT_MAX_EVENT_BYTES: usize = 64 * 1024;
const DEFAULT_HEARTBEAT_INTERVAL_MS: usize = 15_000;
const SSE_CONTENT_TYPE: &str = "text/event-stream; charset=utf-8";
const SSE_CACHE_CONTROL: &str = "no-cache, no-transform";
const SSE_KEEP_ALIVE: &str = "keep-alive";
const SSE_TRANSFER_ENCODING: &str = "chunked";
const SSE_ACCEL_BUFFERING_HEADER: &str = "x-accel-buffering";
const SSE_ACCEL_BUFFERING_DISABLED: &str = "no";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

const SENDER_STATE_OPEN: u8 = 0;
const SENDER_STATE_FINISHING: u8 = 1;
const SENDER_STATE_CLOSED: u8 = 2;

static SSE_CONNECTION_ID_ALLOCATOR: AtomicU32 = AtomicU32::new(1);

/// SSE 结果类型。
///
/// 所有公开 SSE API 均使用该别名返回 `SseError`。该别名无运行时成本，不分配内存，
/// 不阻塞，也没有副作用。
pub type SseResult<T> = Result<T, SseError>;

/// SSE 错误。
///
/// 该错误覆盖配置、协议编码、非阻塞发送、连接生命周期和底层 I/O 错误。
/// 大多数变体构造成本为 `O(1)`；带 `String` 或 `Io` 的变体会持有对应错误上下文。
#[derive(Debug)]
pub enum SseError {
    /// 配置非法，例如 channel size 或最大事件大小为 0。
    InvalidConfig(String),
    /// 请求方法非法；SSE 第一版只接受 GET。
    InvalidMethod(String),
    /// 事件字段非法，例如 `event`、`id` 中含 CR/LF/NUL。
    InvalidEvent(String),
    /// 编码后的单事件超过配置上限。
    EventTooLarge {
        /// 实际编码字节数。
        len: usize,
        /// 配置允许的最大字节数。
        limit: usize,
    },
    /// 同步非阻塞发送时响应队列已满。
    QueueFull,
    /// 同步非阻塞发送时当前 sender 已被其它发送或关闭操作占用。
    Busy,
    /// SSE 连接已关闭或正在关闭。
    Closed,
    /// `u32` 连接 ID 极端情况下耗尽或回绕撞上活动连接。
    ConnectionIdExhausted,
    /// 按 ID 查找 SSE 连接失败。
    ConnectionNotFound,
    /// 底层 HTTP/TCP I/O 错误。
    Io(IoError),
}

impl Display for SseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SseError::InvalidConfig(reason) => write!(f, "invalid SSE config: {}", reason),
            SseError::InvalidMethod(method) => write!(f, "invalid SSE method: {}", method),
            SseError::InvalidEvent(reason) => write!(f, "invalid SSE event: {}", reason),
            SseError::EventTooLarge { len, limit } => {
                write!(f, "SSE event too large: {}, limit: {}", len, limit)
            }
            SseError::QueueFull => write!(f, "SSE queue is full"),
            SseError::Busy => write!(f, "SSE sender is busy"),
            SseError::Closed => write!(f, "SSE connection is closed"),
            SseError::ConnectionIdExhausted => write!(f, "SSE connection id exhausted"),
            SseError::ConnectionNotFound => write!(f, "SSE connection not found"),
            SseError::Io(error) => write!(f, "SSE io error: {}", error),
        }
    }
}

impl StdError for SseError {}

impl From<IoError> for SseError {
    fn from(value: IoError) -> Self {
        match value.kind() {
            ErrorKind::WouldBlock => SseError::QueueFull,
            ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset => {
                SseError::Closed
            }
            _ => SseError::Io(value),
        }
    }
}

/// SSE 配置。
///
/// 功能说明：
/// - 控制每个 SSE 响应体队列容量、单事件最大编码字节数、心跳间隔、初始 comment、
///   cache 头和代理缓冲控制头。
/// - 该类型只保存纯配置值，不持有连接、不持有锁、不执行 I/O。
///
/// 入参/出参：
/// - 只能通过 `SseConfig::builder` 或 `Default` 创建。
/// - getter 返回构建时的配置快照，不暴露内部可变引用。
///
/// 边界与注意事项：
/// - `channel_size` 和 `max_event_bytes` 必须大于 0。
/// - `heartbeat_interval_ms == 0` 表示不自动启动心跳。
/// - 禁忌：不要把超大的 `channel_size` 当作无限缓冲；慢客户端仍应由业务层限流或关闭。
///
/// 性能与安全：
/// - `Clone`、getter、默认构造均为 `O(1)` 时间和空间。
/// - 不阻塞、无 I/O、副作用仅限 Builder 生成新值。
/// - 纯数据类型，线程安全和异步安全；内部无同步锁或异步锁。
///
/// 测试入口：
/// - 单元测试 `sse_config_builder_rejects_invalid_values` 覆盖非法配置。
/// - 真实网络测试 `sse_real_network_get_stream_receives_chunked_event` 覆盖配置在响应构建中的使用。
#[derive(Clone, Debug)]
pub struct SseConfig {
    channel_size: usize,
    max_event_bytes: usize,
    heartbeat_interval_ms: usize,
    send_initial_comment: bool,
    no_cache: bool,
    disable_proxy_buffering: bool,
}

impl Default for SseConfig {
    fn default() -> Self {
        SseConfig {
            channel_size: DEFAULT_CHANNEL_SIZE,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            send_initial_comment: true,
            no_cache: true,
            disable_proxy_buffering: true,
        }
    }
}

impl SseConfig {
    /// 构建 SSE 配置 Builder。
    ///
    /// 返回默认配置的 Builder，调用成本 `O(1)`，不阻塞且无外部副作用。
    pub fn builder() -> SseConfigBuilder {
        SseConfigBuilder {
            config: SseConfig::default(),
        }
    }

    /// 获取响应体队列容量。
    ///
    /// 返回值表示每条 SSE 连接响应体 bounded channel 的最大积压块数。
    /// 调用成本 `O(1)`，无副作用、线程安全、异步安全。
    pub fn channel_size(&self) -> usize {
        self.channel_size
    }

    /// 获取单个事件编码后的最大字节数。
    ///
    /// 返回值用于 `SseEvent::encode` 的上限检查。调用成本 `O(1)`，无副作用。
    pub fn max_event_bytes(&self) -> usize {
        self.max_event_bytes
    }

    /// 获取心跳间隔，单位毫秒；0 表示不启用心跳。
    ///
    /// 调用成本 `O(1)`，无副作用。
    pub fn heartbeat_interval_ms(&self) -> usize {
        self.heartbeat_interval_ms
    }
}

/// SSE 配置 Builder。
///
/// Builder 对非法配置返回错误而不是静默修正，避免生产环境中误配置被掩盖。
///
/// 所有 setter 都只修改 Builder 内部副本，时间复杂度 `O(1)`，不执行 I/O，不阻塞，
/// 不是幂等 API 但重复设置同一值的最终结果相同。`build` 会校验必需边界并返回
/// `SseConfig`。
#[derive(Clone, Debug)]
pub struct SseConfigBuilder {
    config: SseConfig,
}

impl SseConfigBuilder {
    /// 设置响应体队列容量；必须大于 0。
    pub fn channel_size(mut self, channel_size: usize) -> Self {
        self.config.channel_size = channel_size;
        self
    }

    /// 设置单个事件编码后的最大字节数；必须大于 0。
    pub fn max_event_bytes(mut self, max_event_bytes: usize) -> Self {
        self.config.max_event_bytes = max_event_bytes;
        self
    }

    /// 设置心跳间隔，单位毫秒；0 表示不启用心跳。
    pub fn heartbeat_interval_ms(mut self, heartbeat_interval_ms: usize) -> Self {
        self.config.heartbeat_interval_ms = heartbeat_interval_ms;
        self
    }

    /// 设置是否在构建时发送初始 comment。
    pub fn send_initial_comment(mut self, send_initial_comment: bool) -> Self {
        self.config.send_initial_comment = send_initial_comment;
        self
    }

    /// 设置是否发送 no-cache 头。
    pub fn no_cache(mut self, no_cache: bool) -> Self {
        self.config.no_cache = no_cache;
        self
    }

    /// 设置是否发送代理缓冲禁用头。
    pub fn disable_proxy_buffering(mut self, disable_proxy_buffering: bool) -> Self {
        self.config.disable_proxy_buffering = disable_proxy_buffering;
        self
    }

    /// 完成配置构建。
    pub fn build(self) -> SseResult<SseConfig> {
        if self.config.channel_size == 0 {
            return Err(SseError::InvalidConfig(
                "channel_size must be greater than 0".to_string(),
            ));
        }

        if self.config.max_event_bytes == 0 {
            return Err(SseError::InvalidConfig(
                "max_event_bytes must be greater than 0".to_string(),
            ));
        }

        Ok(self.config)
    }
}

/// SSE 事件。
///
/// 功能说明：
/// - 表示一帧标准 SSE 文本事件，支持 comment、`id`、`event`、`retry` 和多行 `data`。
/// - 编码结果满足 SSE 文本格式：字段逐行输出，并以空行结束一个事件。
///
/// 入参/出参：
/// - 字段值为 UTF-8 `String`。
/// - `event` 和 `id` 必须是单行文本，不能包含 CR/LF/NUL。
/// - `data` 和 comment 允许多行，不能包含 NUL。
/// - `encode` 返回完整事件字节，不包含 HTTP chunk framing；chunk framing 由 `HttpConnect` 负责。
///
/// 边界与禁忌：
/// - 禁忌：不要把未受控的大对象直接作为 `data`，应先设置合理 `max_event_bytes`。
/// - 本模块不解释 JSON；业务层如需 JSON，应自行序列化为字符串后放入 `data`。
///
/// 性能与安全：
/// - 构造为 `O(n)` 或 `O(1)`，取决于传入字符串转换成本。
/// - `encode` 为 `O(n)` 时间、`O(n)` 空间，`n` 为字段总字节数。
/// - 不执行 I/O，不阻塞，无同步锁或异步锁；除分配返回 buffer 外无外部副作用。
/// - 类型可 clone，clone 成本与字段总长度线性相关。
///
/// 测试入口：
/// - `sse_event_builder_and_encoding` 覆盖标准编码。
/// - `sse_event_rejects_invalid_or_large_event` 覆盖非法字段和大小限制。
/// - `sse_real_network_get_stream_receives_chunked_event` 覆盖事件在真实 HTTP 响应中的输出。
#[derive(Clone, Debug, Default)]
pub struct SseEvent {
    event: Option<String>,
    data: Vec<String>,
    id: Option<String>,
    retry: Option<u64>,
    comment: Option<String>,
}

impl SseEvent {
    /// 构建 SSE 事件 Builder。
    ///
    /// 返回空事件 Builder；调用成本 `O(1)`，不阻塞，无副作用。
    pub fn builder() -> SseEventBuilder {
        SseEventBuilder {
            event: SseEvent::default(),
        }
    }

    /// 构建默认 message 事件。
    ///
    /// `data` 会成为一段 `data:` 字段。调用会拥有传入字符串，可能分配；不校验字段，
    /// 字段合法性在 `encode` 或 Builder `build` 时检查。
    pub fn data(data: impl Into<String>) -> Self {
        SseEvent {
            data: vec![data.into()],
            ..SseEvent::default()
        }
    }

    /// 构建具名事件。
    ///
    /// `event` 必须在编码前通过单行校验；该函数本身不返回错误，适合确定输入合法的场景。
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        SseEvent {
            event: Some(event.into()),
            data: vec![data.into()],
            ..SseEvent::default()
        }
    }

    /// 构建 comment 事件，常用于心跳。
    ///
    /// comment 会编码为 `:` 行；空 comment 可作为心跳帧。
    pub fn comment(comment: impl Into<String>) -> Self {
        SseEvent {
            comment: Some(comment.into()),
            ..SseEvent::default()
        }
    }

    /// 设置事件 ID。
    ///
    /// 立即校验 `id` 是否为单行文本。成功返回更新后的事件；失败不修改外部状态。
    /// 时间复杂度 `O(n)`，`n` 为 ID 长度。
    pub fn id(mut self, id: impl Into<String>) -> SseResult<Self> {
        let id = id.into();
        validate_single_line_field("id", &id)?;
        self.id = Some(id);
        Ok(self)
    }

    /// 设置客户端重连间隔，单位毫秒。
    ///
    /// 该值只影响客户端 EventSource 的后续重连建议，不影响当前连接发送节奏。
    /// 调用成本 `O(1)`，无 I/O，无阻塞。
    pub fn retry(mut self, millis: u64) -> SseResult<Self> {
        self.retry = Some(millis);
        Ok(self)
    }

    /// 编码事件。
    ///
    /// `max_event_bytes` 必须大于 0。成功返回 UTF-8 SSE 事件字节；若字段非法或超过大小限制，
    /// 返回 `SseError`。该函数是纯编码逻辑，不写 HTTP 队列、不访问 socket。
    pub fn encode(&self, max_event_bytes: usize) -> SseResult<Vec<u8>> {
        if max_event_bytes == 0 {
            return Err(SseError::InvalidConfig(
                "max_event_bytes must be greater than 0".to_string(),
            ));
        }

        if let Some(event) = &self.event {
            validate_single_line_field("event", event)?;
        }
        if let Some(id) = &self.id {
            validate_single_line_field("id", id)?;
        }
        if let Some(comment) = &self.comment {
            validate_text_field("comment", comment)?;
        }
        for data in &self.data {
            validate_text_field("data", data)?;
        }

        let mut buf = Vec::new();
        if let Some(comment) = &self.comment {
            encode_multiline_field(&mut buf, ":", comment, false);
        }
        if let Some(id) = &self.id {
            buf.extend_from_slice(b"id: ");
            buf.extend_from_slice(id.as_bytes());
            buf.extend_from_slice(b"\n");
        }
        if let Some(event) = &self.event {
            buf.extend_from_slice(b"event: ");
            buf.extend_from_slice(event.as_bytes());
            buf.extend_from_slice(b"\n");
        }
        if let Some(retry) = self.retry {
            buf.extend_from_slice(b"retry: ");
            buf.extend_from_slice(retry.to_string().as_bytes());
            buf.extend_from_slice(b"\n");
        }
        for data in &self.data {
            encode_multiline_field(&mut buf, "data: ", data, true);
        }
        buf.extend_from_slice(b"\n");

        if buf.len() > max_event_bytes {
            return Err(SseError::EventTooLarge {
                len: buf.len(),
                limit: max_event_bytes,
            });
        }

        Ok(buf)
    }
}

/// SSE 事件 Builder。
///
/// Builder 允许分步填写事件字段，最后在 `build` 阶段统一校验。setter 调用成本为
/// `O(n)` 或 `O(1)`，取决于字符串转换成本；不阻塞，无外部副作用。`build` 会调用
/// `SseEvent::encode(usize::MAX)` 做字段合法性检查，但不保留编码结果。
#[derive(Clone, Debug, Default)]
pub struct SseEventBuilder {
    event: SseEvent,
}

impl SseEventBuilder {
    /// 设置事件名；必须为单行文本。
    pub fn event(mut self, event: impl Into<String>) -> Self {
        self.event.event = Some(event.into());
        self
    }

    /// 追加一段 data。
    pub fn data(mut self, data: impl Into<String>) -> Self {
        self.event.data.push(data.into());
        self
    }

    /// 设置事件 ID；必须为单行文本。
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.event.id = Some(id.into());
        self
    }

    /// 设置客户端重连间隔，单位毫秒。
    pub fn retry(mut self, millis: u64) -> Self {
        self.event.retry = Some(millis);
        self
    }

    /// 设置 comment；可包含多行。
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.event.comment = Some(comment.into());
        self
    }

    /// 完成事件构建，并执行字段校验。
    pub fn build(self) -> SseResult<SseEvent> {
        self.event.encode(usize::MAX)?;
        Ok(self.event)
    }
}

/// SSE 响应构建入口。
///
/// 功能说明：
/// - 该零大小类型提供从 `HttpRequest<S>` 构建 SSE `HttpResponse` 与 `SseSender<S>` 的入口。
/// - 它不保存状态，不持有连接，不执行 I/O。
///
/// 边界：
/// - 只接受 HTTP GET 请求；其它方法由 `SseResponseBuilder::build` 返回 `InvalidMethod`。
/// - 不注册 Hub，不做历史重放，不替业务层选择连接 key。
///
/// 性能与安全：
/// - `builder` 调用为 `O(1)`。
/// - 无副作用、线程安全、异步安全。
///
/// 测试入口：
/// - 真实网络测试 `sse_real_network_get_stream_receives_chunked_event` 覆盖该入口。
pub struct SseResponse;

impl SseResponse {
    /// 基于当前 HTTP 请求构建 SSE 响应 Builder。
    ///
    /// 返回的 Builder 借用请求，只能在当前请求处理生命周期内使用。调用不修改请求、
    /// 不写响应队列、不阻塞。
    pub fn builder<S: Socket>(req: &HttpRequest<S>) -> SseResponseBuilder<'_, S> {
        SseResponseBuilder {
            req,
            config: SseConfig::default(),
        }
    }
}

/// SSE 响应 Builder。
///
/// 功能说明：
/// - Builder 会创建 `HttpResponse` 与绑定当前连接的 `SseSender`。
/// - `HttpResponse` 已配置为 HTTP/1.1 stream response，设置 `Content-Type`、
///   `Transfer-Encoding: chunked`、`Connection: keep-alive`、cache 控制和可选代理缓冲控制头。
/// - `SseSender` 绑定当前 TCP/TLS socket handle 和响应体队列。
///
/// 入参/出参：
/// - `config` 接受纯配置值并覆盖默认配置。
/// - `build` 返回 `(HttpResponse, SseSender<S>)`，调用方必须把 `HttpResponse` 交回
///   `pi_http` 响应流程，否则 sender 队列会断开。
/// - `build_with_heartbeat` 还会用传入 runtime 启动心跳任务。
///
/// 边界与禁忌：
/// - 只支持 GET。
/// - 不注册 `SseHub`，调用方需要显式 `hub.register(key, sender.clone())`。
/// - 禁忌：不要在返回 `HttpResponse` 前 drop 它；否则后续 sender 发送会得到 Closed/Io。
/// - 禁忌：不要让默认压缩中间件处理 stream response；`DefaultParser` 已按生产侧约束跳过 stream。
///
/// 性能与安全：
/// - 构建时间复杂度 `O(h)`，`h` 为响应头数量；空间复杂度 `O(1)` 加响应队列容量。
/// - `build` 会同步非阻塞地写入可选初始 comment，队列满时返回错误。
/// - `build_with_heartbeat` 有启动异步任务副作用；心跳任务只持有 `SseSender` clone。
/// - Builder 不持有同步锁跨 `.await`。
///
/// 测试入口：
/// - `sse_real_network_get_stream_receives_chunked_event` 验证真实响应头和真实 chunked 输出。
pub struct SseResponseBuilder<'a, S: Socket> {
    req: &'a HttpRequest<S>,
    config: SseConfig,
}

impl<'a, S: Socket> SseResponseBuilder<'a, S> {
    /// 设置 SSE 配置。
    ///
    /// 覆盖 Builder 当前配置。调用成本 `O(1)` 加 `SseConfig` clone 成本，无 I/O、无阻塞。
    pub fn config(mut self, config: SseConfig) -> Self {
        self.config = config;
        self
    }

    /// 构建 SSE 响应和 sender，不启动心跳任务。
    ///
    /// 成功时返回的 sender 初始状态为 open。若配置要求发送初始 comment，则该 comment
    /// 已进入响应体队列，但尚不代表客户端已收到。
    pub fn build(self) -> SseResult<(HttpResponse, SseSender<S>)> {
        if self.req.method() != &Method::GET {
            return Err(SseError::InvalidMethod(
                self.req.method().as_str().to_string(),
            ));
        }

        SseConfigBuilder {
            config: self.config.clone(),
        }
        .build()?;

        let mut resp = HttpResponse::new(self.config.channel_size);
        resp.enable_stream();
        resp.insert_header(CONTENT_TYPE.as_str(), SSE_CONTENT_TYPE);
        resp.insert_header(CONNECTION.as_str(), SSE_KEEP_ALIVE);
        resp.insert_header(TRANSFER_ENCODING.as_str(), SSE_TRANSFER_ENCODING);
        resp.remove_header(CONTENT_LENGTH.as_str());
        resp.remove_header(CONTENT_ENCODING.as_str());

        if self.config.no_cache {
            resp.insert_header(CACHE_CONTROL.as_str(), SSE_CACHE_CONTROL);
        }
        if self.config.disable_proxy_buffering {
            resp.insert_header(SSE_ACCEL_BUFFERING_HEADER, SSE_ACCEL_BUFFERING_DISABLED);
        }

        let handler = resp
            .get_response_handler()
            .ok_or_else(|| SseError::InvalidConfig("missing response handler".to_string()))?;
        let last_event_id = read_last_event_id(self.req)?;
        let sender = SseSender::new(
            next_connection_id(),
            self.req.get_handle().clone(),
            handler,
            self.config.max_event_bytes,
            last_event_id,
        );

        if self.config.send_initial_comment {
            sender.try_comment("pi_http sse connected")?;
        }

        Ok((resp, sender))
    }

    /// 构建 SSE 响应和 sender，并按配置启动心跳任务。
    ///
    /// `runtime` 必须能执行 `Send + 'static` 任务。心跳间隔为 0 时不启动任务。
    /// 心跳任务在 sender 关闭或心跳发送失败后退出。该方法会 clone runtime 和 sender。
    pub fn build_with_heartbeat<R>(self, runtime: R) -> SseResult<(HttpResponse, SseSender<S>)>
    where
        R: AsyncRuntime<()>,
    {
        let interval = self.config.heartbeat_interval_ms;
        let (resp, sender) = self.build()?;
        if interval > 0 {
            sender.start_heartbeat(runtime, interval)?;
        }
        Ok((resp, sender))
    }
}

/// SSE 建连接入请求。
///
/// 功能说明：
/// - `SseMiddleware` 在路由命中 SSE GET 请求、已经构建 sender、但尚未把 stream response
///   返回给 HTTP 连接前，把该结构传给外部 acceptor。
/// - 外部可在这个时机检查请求头、URL、网关上下文、远端地址和 `Last-Event-ID`，决定是否
///   允许该 HTTP 连接打开 SSE。
///
/// 使用边界：
/// - `sender` 是安全透明句柄 clone；技术上可在 acceptor 中 clone 保存，但推荐只用于决策，
///   并在 `on_open` 中持久保存。这样可避免 Hub 注册失败时外部保存半初始化连接。
/// - 该结构只在同步回调期间有效；`context` 和 `request` 引用不能逃逸。
///
/// 性能与安全：
/// - 结构创建为 `O(1)`，不执行 I/O，不持有 Hub 锁。
/// - 回调禁止阻塞当前请求路径；需要长耗时授权时应在进入 SSE 路由前完成。
pub struct SseAccept<'a, S: Socket> {
    /// 当前网关上下文，只能在回调期间读取。
    pub context: &'a GatewayContext,
    /// 当前 HTTP 请求，只能在回调期间读取。
    pub request: &'a HttpRequest<S>,
    /// 当前待打开 SSE 连接的发送句柄 clone。
    pub sender: SseSender<S>,
}

/// SSE 建连接入决策。
///
/// 功能说明：
/// - acceptor 返回该枚举，明确允许或拒绝当前 HTTP 连接打开 SSE。
/// - 允许时携带 Hub 注册 key；拒绝时携带普通 HTTP 响应状态码和文本消息。
///
/// 边界：
/// - `Accept(K)` 不表示客户端已经收到响应，只表示中间件可以继续注册 Hub 并返回 SSE stream。
/// - `Reject` 会返回普通 HTTP 响应，不会返回 `text/event-stream`。
#[derive(Clone, Debug)]
pub enum SseAcceptDecision<K> {
    /// 允许打开 SSE，并使用该 key 注册到 Hub。
    Accept(K),
    /// 拒绝打开 SSE，并返回普通 HTTP 错误响应。
    Reject {
        /// HTTP 状态码。
        status: StatusCode,
        /// 响应文本。
        message: String,
    },
}

impl<K> SseAcceptDecision<K> {
    /// 构造允许决策。
    ///
    /// 调用成本 `O(1)` 加 key 移动成本；无副作用、不阻塞。
    pub fn accept(key: K) -> Self {
        SseAcceptDecision::Accept(key)
    }

    /// 构造拒绝决策。
    ///
    /// `message` 会被转换为 owned `String`。调用成本与消息长度线性相关；无 I/O。
    pub fn reject(status: StatusCode, message: impl Into<String>) -> Self {
        SseAcceptDecision::Reject {
            status,
            message: message.into(),
        }
    }
}

/// SSE 建连接入回调。
///
/// 功能说明：
/// - 这是外部获知“某个 HTTP 连接请求打开 SSE”的标准入口。
/// - `SseMiddleware` 每次处理 SSE 路由请求时都会在返回 stream response 前调用它。
///
/// 入参/出参：
/// - 入参 `SseAccept` 带当前请求、上下文和 sender clone。
/// - 返回 `Accept(key)` 表示允许并注册；返回 `Reject` 表示返回普通 HTTP 错误响应；
///   返回 `Err` 表示中间件按错误类型转换 HTTP 响应。
///
/// 边界与性能：
/// - 回调同步执行，禁止阻塞、禁止执行长耗时 I/O。
/// - 时间复杂度由业务实现决定；建议保持 `O(1)` 或与少量请求头/query 字段线性相关。
/// - 回调不得持有 `GatewayContext` 内部 `RefCell` borrow 后再调用可能重入的业务逻辑。
///
/// 测试入口：
/// - 单元测试 `sse_middleware_custom_acceptor_on_open_and_multi_connection_key` 覆盖自定义 acceptor。
pub type SseAcceptHandler<K, S> =
    Arc<dyn for<'a> Fn(SseAccept<'a, S>) -> SseResult<SseAcceptDecision<K>> + Send + Sync>;

/// SSE 连接打开回调。
///
/// 功能说明：
/// - `SseMiddleware` 在 sender 注册到 Hub 后调用该回调。
/// - 业务可在这里保存连接信息、发送欢迎事件、启动外部推送任务或执行轻量审计。
///
/// 入参/出参：
/// - 入参 `SseOpen<K, S>` 持有 key、透明连接 ID 和 sender clone。
/// - 返回 `Err` 会使中间件摘除刚注册的连接，并返回普通 HTTP 错误响应，不会返回半初始化
///   SSE stream。
///
/// 边界与性能：
/// - 回调同步执行，禁止阻塞当前请求路径。
/// - 若需要异步或长耗时工作，应在回调里把 `SseSender` clone 移交给外部任务后立即返回。
/// - 回调有业务副作用，不要求幂等；调用方应避免重复注册或重复发送欢迎事件。
///
/// 测试入口：
/// - 单元测试 `sse_middleware_custom_acceptor_on_open_and_multi_connection_key` 覆盖 `on_open` 发送事件。
pub type SseOpenHandler<K, S> = Arc<dyn Fn(SseOpen<K, S>) -> SseResult<()> + Send + Sync>;

type SseHeartbeatStarter<S> = Arc<dyn Fn(SseSender<S>, usize) -> SseResult<()> + Send + Sync>;

/// SSE 中间件打开连接信息。
///
/// 功能说明：
/// - 该结构只在 `SseMiddleware` 的 `on_open` 回调中传递，用于把新建 SSE 连接暴露给业务层。
///
/// 字段语义：
/// - `key` 是已注册到 Hub 的业务 key。
/// - `id` 是透明 SSE 连接 ID。
/// - `sender` 是当前连接的发送句柄 clone，可跨线程和异步任务保存。
///
/// 性能与安全：
/// - `Clone` 成本由 `K` 决定；`sender` clone 为 `O(1)`。
/// - 不持有 Hub 锁、不持有请求借用；回调返回后仍可安全移动到其它线程。
#[derive(Clone)]
pub struct SseOpen<K, S: Socket> {
    /// 已注册到 Hub 的业务 key。
    pub key: K,
    /// 当前 SSE 连接的透明 ID。
    pub id: SseConnectionId,
    /// 当前 SSE 连接的发送句柄。
    pub sender: SseSender<S>,
}

/// `pi_http` 默认 SSE 中间件。
///
/// 功能说明：
/// - 这是面向外部使用方的标准 SSE 路由处理器，可直接注册到 `HttpRoute::get` 或
///   `MiddlewareChain`。
/// - 请求阶段将当前 GET 请求转换为 SSE stream response，创建 `SseSender`，按 key 注册到
///   `SseHub`，并可调用 `on_open` 回调。
/// - 响应阶段对 stream response 返回 `Break`，使后续默认响应处理不把 SSE 当普通块响应聚合。
///
/// 入参/出参：
/// - 默认 key 类型为 `SseConnectionId`，可用 `SseMiddleware::new(hub)` 或
///   `SseMiddleware::builder(hub)` 构建。
/// - 自定义业务 key 和授权逻辑使用 `SseMiddleware::with_acceptor(hub, acceptor)` 构建。
/// - `hub()` 返回内部 Hub clone，调用方可用于推送、快照、关闭和清理连接。
///
/// 业务边界：
/// - 中间件只处理“当前路由上的 SSE 建连和注册”，不做鉴权、限流、历史重放、事件持久化、
///   多租户隔离或业务 key 正确性判断；这些应由业务层在路由前置中间件、acceptor 或
///   `on_open` 中实现。
/// - `require_accept_header` 默认关闭，以兼容非浏览器客户端；开启后会要求 `Accept` 头包含
///   `text/event-stream`。
///
/// 性能与安全：
/// - 建连路径为 `O(h + a)`，`h` 为响应头数量，`a` 为 acceptor 成本。
/// - Hub 注册均摊 `O(1)`；不持有 Hub guard 跨 `.await`。
/// - `request` 不阻塞异步运行时；业务提供的 acceptor/on_open 必须遵守非阻塞约束。
/// - 类型在 `K` 和 `S` 满足约束时 `Clone + Send + Sync`。
///
/// 测试入口：
/// - 单元测试 `sse_middleware_registers_default_connection_id_key` 覆盖默认 key 与注册。
/// - 单元测试 `sse_middleware_custom_acceptor_on_open_and_multi_connection_key` 覆盖业务 key 和打开回调。
/// - 单元测试 `sse_middleware_acceptor_can_reject_before_stream_is_returned`、`sse_middleware_can_require_accept_event_stream_header`、
///   `sse_middleware_on_open_error_unregisters_sender` 和 `sse_middleware_heartbeat_runtime_sends_comment`
///   覆盖拒绝、`Accept` 校验、回调回滚和自动心跳。
/// - 真实网络测试 `sse_real_network_get_stream_receives_chunked_event`、`sse_real_network_acceptor_can_reject_open_request`
///   和 `sse_real_network_heartbeat_runtime_emits_comment` 覆盖该中间件的 TCP 输出路径。
pub struct SseMiddleware<K, S: Socket> {
    hub: SseHub<K, S>,
    config: SseConfig,
    acceptor: SseAcceptHandler<K, S>,
    on_open: Option<SseOpenHandler<K, S>>,
    heartbeat_starter: Option<SseHeartbeatStarter<S>>,
    require_accept_header: bool,
}

impl<K, S: Socket> Clone for SseMiddleware<K, S> {
    fn clone(&self) -> Self {
        SseMiddleware {
            hub: self.hub.clone(),
            config: self.config.clone(),
            acceptor: self.acceptor.clone(),
            on_open: self.on_open.clone(),
            heartbeat_starter: self.heartbeat_starter.clone(),
            require_accept_header: self.require_accept_header,
        }
    }
}

impl<S: Socket> SseMiddleware<SseConnectionId, S> {
    /// 使用透明连接 ID 作为 key 构建默认 SSE 中间件。
    ///
    /// 调用成本 `O(1)`；不注册连接、不执行 I/O、不启动任务。返回的中间件可直接挂到 GET 路由。
    pub fn new(hub: SseHub<SseConnectionId, S>) -> Self {
        Self::builder(hub)
            .build()
            .expect("default SSE middleware config must be valid")
    }

    /// 构建默认 key 版本的 SSE 中间件 Builder。
    ///
    /// 默认 acceptor 返回 `sender.id()`，因此每条 SSE 连接天然有唯一 key。
    pub fn builder(hub: SseHub<SseConnectionId, S>) -> SseMiddlewareBuilder<SseConnectionId, S> {
        SseMiddlewareBuilder {
            hub,
            config: SseConfig::default(),
            acceptor: Arc::new(|accept| Ok(SseAcceptDecision::Accept(accept.sender.id()))),
            on_open: None,
            heartbeat_starter: None,
            require_accept_header: false,
        }
    }
}

impl<K, S> SseMiddleware<K, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    S: Socket,
{
    /// 使用业务接入决策回调构建 SSE 中间件 Builder。
    ///
    /// acceptor 在每次建连时同步执行，允许业务检查请求并决定允许或拒绝。调用本函数本身为
    /// `O(1)`，不执行 I/O。
    pub fn with_acceptor<F>(hub: SseHub<K, S>, acceptor: F) -> SseMiddlewareBuilder<K, S>
    where
        F: for<'a> Fn(SseAccept<'a, S>) -> SseResult<SseAcceptDecision<K>> + Send + Sync + 'static,
    {
        SseMiddlewareBuilder {
            hub,
            config: SseConfig::default(),
            acceptor: Arc::new(acceptor),
            on_open: None,
            heartbeat_starter: None,
            require_accept_header: false,
        }
    }

    /// 获取内部 Hub 的轻量 clone。
    ///
    /// 业务层可通过该 Hub 在请求处理路径外发送、广播、快照或关闭 SSE 连接。
    pub fn hub(&self) -> SseHub<K, S> {
        self.hub.clone()
    }

    /// 获取中间件配置快照引用。
    ///
    /// 返回值只读；调用成本 `O(1)`，无副作用。
    pub fn config(&self) -> &SseConfig {
        &self.config
    }
}

impl<K, S> Middleware<S, GatewayContext> for SseMiddleware<K, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    S: Socket,
{
    fn request<'a>(
        &'a self,
        context: &'a mut GatewayContext,
        req: HttpRequest<S>,
    ) -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        async move {
            if self.require_accept_header && !accepts_event_stream(&req) {
                return MiddlewareResult::Break(sse_error_response(
                    StatusCode::NOT_ACCEPTABLE,
                    "SSE request must accept text/event-stream",
                ));
            }

            let (resp, sender) = match SseResponse::builder(&req)
                .config(self.config.clone())
                .build()
            {
                Ok(value) => value,
                Err(error) => return MiddlewareResult::Break(sse_error_to_response(error)),
            };

            let decision = match (self.acceptor)(SseAccept {
                context,
                request: &req,
                sender: sender.clone(),
            }) {
                Ok(decision) => decision,
                Err(error) => {
                    let _ = sender.try_finish();
                    return MiddlewareResult::Break(sse_error_to_response(error));
                }
            };
            let key = match decision {
                SseAcceptDecision::Accept(key) => key,
                SseAcceptDecision::Reject { status, message } => {
                    let _ = sender.try_finish();
                    return MiddlewareResult::Break(sse_error_response(status, message));
                }
            };

            let id = match self.hub.register(key.clone(), sender.clone()) {
                Ok(id) => id,
                Err(error) => {
                    let _ = sender.try_finish();
                    return MiddlewareResult::Break(sse_error_to_response(error));
                }
            };

            if let Some(on_open) = &self.on_open {
                let open = SseOpen {
                    key,
                    id,
                    sender: sender.clone(),
                };
                if let Err(error) = on_open(open) {
                    let _ = self.hub.unregister(id);
                    let _ = sender.try_finish();
                    return MiddlewareResult::Break(sse_error_to_response(error));
                }
            }

            if let Some(starter) = &self.heartbeat_starter {
                if self.config.heartbeat_interval_ms > 0 {
                    if let Err(error) = starter(sender.clone(), self.config.heartbeat_interval_ms) {
                        let _ = self.hub.unregister(id);
                        let _ = sender.try_finish();
                        return MiddlewareResult::Break(sse_error_to_response(error));
                    }
                }
            }

            MiddlewareResult::Finish((req, resp))
        }
        .boxed_local()
    }

    fn response<'a>(
        &'a self,
        _context: &'a mut GatewayContext,
        req: HttpRequest<S>,
        resp: HttpResponse,
    ) -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        async move {
            if resp.is_stream() {
                MiddlewareResult::Break(resp)
            } else {
                MiddlewareResult::Finish((req, resp))
            }
        }
        .boxed_local()
    }
}

/// SSE 中间件 Builder。
///
/// 功能说明：
/// - 用于配置默认 SSE 中间件的响应配置、`Accept` 校验和打开回调。
/// - Builder 不注册连接；只有中间件处理请求时才会建立 SSE sender。
///
/// 性能与安全：
/// - setter 均为 `O(1)`，不执行 I/O，不阻塞。
/// - `build` 校验 `SseConfig` 并返回可 clone 的中间件。
///
/// 测试入口：
/// - `sse_middleware_custom_acceptor_on_open_and_multi_connection_key` 覆盖 Builder 配置路径。
pub struct SseMiddlewareBuilder<K, S: Socket> {
    hub: SseHub<K, S>,
    config: SseConfig,
    acceptor: SseAcceptHandler<K, S>,
    on_open: Option<SseOpenHandler<K, S>>,
    heartbeat_starter: Option<SseHeartbeatStarter<S>>,
    require_accept_header: bool,
}

impl<K, S> SseMiddlewareBuilder<K, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    S: Socket,
{
    /// 设置 SSE 响应配置。
    pub fn config(mut self, config: SseConfig) -> Self {
        self.config = config;
        self
    }

    /// 设置是否要求请求 `Accept` 头包含 `text/event-stream`。
    ///
    /// 默认关闭。开启后，缺失或不匹配的请求会得到 `406 Not Acceptable`。
    pub fn require_accept_header(mut self, require_accept_header: bool) -> Self {
        self.require_accept_header = require_accept_header;
        self
    }

    /// 设置自动心跳任务运行时。
    ///
    /// `SseConfig::heartbeat_interval_ms` 大于 0 且配置了运行时时，中间件会在 acceptor 允许、
    /// Hub 注册成功并且 `on_open` 成功后启动心跳任务。未配置运行时时不会自动启动心跳，
    /// 调用方仍可通过 `SseSender::heartbeat` 或 `try_heartbeat` 自行发送心跳。
    ///
    /// 调用成本 `O(1)` 加 runtime clone 成本；不启动任务、不注册连接。实际任务启动发生在
    /// 每条 SSE 连接打开时，失败会回滚 Hub 注册并返回普通 HTTP 错误响应。
    pub fn heartbeat_runtime<R>(mut self, runtime: R) -> Self
    where
        R: AsyncRuntime<()>,
    {
        self.heartbeat_starter = Some(Arc::new(move |sender, interval_ms| {
            sender.start_heartbeat(runtime.clone(), interval_ms)
        }));
        self
    }

    /// 设置 SSE 建连接入决策回调。
    ///
    /// 回调同步执行，必须快速返回；拒绝时返回普通 HTTP 响应，不会打开 SSE stream。
    pub fn acceptor<F>(mut self, acceptor: F) -> Self
    where
        F: for<'a> Fn(SseAccept<'a, S>) -> SseResult<SseAcceptDecision<K>> + Send + Sync + 'static,
    {
        self.acceptor = Arc::new(acceptor);
        self
    }

    /// 设置 SSE 连接打开回调。
    ///
    /// 回调同步执行，必须快速返回；如需异步工作，应在回调内把 sender clone 交给外部任务。
    pub fn on_open<F>(mut self, handler: F) -> Self
    where
        F: Fn(SseOpen<K, S>) -> SseResult<()> + Send + Sync + 'static,
    {
        self.on_open = Some(Arc::new(handler));
        self
    }

    /// 完成中间件构建。
    ///
    /// 返回错误只可能来自非法 `SseConfig`。成功后中间件可重复 clone 并挂载到路由。
    pub fn build(self) -> SseResult<SseMiddleware<K, S>> {
        SseConfigBuilder {
            config: self.config.clone(),
        }
        .build()?;

        Ok(SseMiddleware {
            hub: self.hub,
            config: self.config,
            acceptor: self.acceptor,
            on_open: self.on_open,
            heartbeat_starter: self.heartbeat_starter,
            require_accept_header: self.require_accept_header,
        })
    }
}

/// 透明 SSE 连接 ID。
///
/// 该 ID 只表示已经通过 SSE 响应构建出的 SSE 连接，不等价于普通 HTTP 连接 ID。
///
/// 边界：
/// - 底层值为 `u32`，0 保留不用。
/// - 字段私有，外部只能从 `SseSender::id`、`SseHub::register` 或快照中获得。
/// - ID 不承诺跨进程、跨重启稳定。
///
/// 性能与安全：
/// - `Copy`，传递成本 `O(1)`。
/// - 线程安全、异步安全，无锁、无 I/O。
///
/// 测试入口：
/// - `sse_hub_try_send_to_id_reports_result` 和真实网络测试覆盖 ID 定向发送。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SseConnectionId(u32);

impl SseConnectionId {
    /// 获取底层 `u32` ID。
    ///
    /// 该方法只暴露只读值，不允许反向构造连接。调用成本 `O(1)`，无副作用。
    pub fn get(self) -> u32 {
        self.0
    }
}

/// SSE sender。
///
/// 功能说明：
/// - `SseSender` 是每条 SSE 连接的发送句柄。
/// - 一个 SSE 请求会生成一个 sender；sender 可被 clone 后交给业务线程、异步任务或 `SseHub`。
/// - async API 提供异步背压；`try_*` API 提供同步非阻塞发送能力。
///
/// 入参/出参：
/// - `send` / `try_send` 接受已经构造好的 `SseEvent`，成功只表示事件进入 `pi_http`
///   响应体队列，不表示 socket 已写完或客户端已消费。
/// - `send_data` / `try_send_data` 是默认 message 事件快捷方式。
/// - `comment` / `heartbeat` 用于 comment 帧，常用于 keep-alive。
/// - `finish` / `try_finish` 发送响应体结束标记；重复调用安全。
///
/// 生命周期边界：
/// - open 状态允许发送事件。
/// - finishing/closed 状态拒绝新事件。
/// - 底层 socket 已关闭时，`is_closed` 会把 sender 状态推进为 closed。
/// - `SseSender` drop 不自动关闭连接；第一版要求显式 `finish`、`try_finish`、`SseHub::close`
///   或等待底层连接关闭自动清理。
///
/// 性能与阻塞：
/// - 事件发送编码为 `O(n)` 时间、`O(n)` 空间，`n` 为事件编码长度。
/// - async API 会等待响应体队列容量，可能异步挂起，但不阻塞 OS 线程。
/// - `try_*` API 为 `O(n)` 编码 + `O(1)` 入队；队列满返回 `QueueFull`，发送门禁被占用返回 `Busy`。
/// - 顺序语义：同一线程连续成功调用按调用顺序入队；多线程或多任务并发成功调用按实际入队顺序输出。
///   对 async API，创建 future 本身不会执行发送，顺序以 future 被 poll 后进入发送流程为准。
///
/// 副作用、幂等与锁：
/// - 发送 API 有副作用，不幂等；重复调用会重复发送事件。
/// - `finish` / `try_finish` 对关闭状态幂等。
/// - 内部使用 `AtomicU8` 管理状态，使用 `futures::lock::Mutex` 串行化发送和关闭顺序。
/// - 事件编码在发送门禁内完成，避免同一 sender 上先调用的大事件因编码耗时而被后调用的小事件越过。
/// - 不持有 `SseHub` 的 `DashMap` guard；不会跨模块持锁 `.await`。
///
/// 线程/异步安全：
/// - `SseSender<S>` 在底层 `Socket` 契约成立时为 `Send + Sync`。
/// - 允许跨线程 clone 和调用 `try_*`，允许跨异步任务调用 async API。
///
/// 测试入口：
/// - `sse_sender_try_send_reports_queue_full` 覆盖同步非阻塞队列满。
/// - `sse_sender_send_and_finish_are_ordered` 覆盖发送、关闭和关闭后拒绝。
/// - `sse_sender_and_hub_are_send_sync_clone` 覆盖类型安全边界。
/// - `sse_real_network_get_stream_receives_chunked_event` 覆盖真实 TCP 输出。
pub struct SseSender<S: Socket> {
    inner: Arc<SseSenderInner<S>>,
}

struct SseSenderInner<S: Socket> {
    id: SseConnectionId,
    handle: SocketHandle<S>,
    response: ResponseHandler,
    max_event_bytes: usize,
    last_event_id: Option<String>,
    state: AtomicU8,
    send_gate: Arc<Mutex<()>>,
}

impl<S: Socket> Clone for SseSender<S> {
    fn clone(&self) -> Self {
        SseSender {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Socket> SseSender<S> {
    fn new(
        id: SseConnectionId,
        handle: SocketHandle<S>,
        response: ResponseHandler,
        max_event_bytes: usize,
        last_event_id: Option<String>,
    ) -> Self {
        SseSender {
            inner: Arc::new(SseSenderInner {
                id,
                handle,
                response,
                max_event_bytes,
                last_event_id,
                state: AtomicU8::new(SENDER_STATE_OPEN),
                send_gate: Arc::new(Mutex::new(())),
            }),
        }
    }

    /// 异步发送事件。
    ///
    /// 若队列已满，该方法会异步等待容量；若连接已关闭，返回 `Closed`。
    pub async fn send(&self, event: SseEvent) -> SseResult<()> {
        let _guard = self.inner.send_gate.lock().await;
        let encoded = event.encode(self.inner.max_event_bytes)?;
        self.write_encoded(encoded).await
    }

    /// 异步发送默认 message 事件。
    ///
    /// `data` 会复制为 owned `String` 后发送。语义等价于 `send(SseEvent::data(...))`。
    pub async fn send_data(&self, data: impl AsRef<str>) -> SseResult<()> {
        self.send(SseEvent::data(data.as_ref().to_string())).await
    }

    /// 异步发送 comment。
    ///
    /// comment 可用于业务 keep-alive 或调试标记；客户端不会把 comment 当普通 message 事件。
    pub async fn comment(&self, text: impl AsRef<str>) -> SseResult<()> {
        self.send(SseEvent::comment(text.as_ref().to_string()))
            .await
    }

    /// 异步发送 heartbeat comment。
    ///
    /// 发送空 comment，编码为 SSE 心跳帧。可能异步等待队列容量。
    pub async fn heartbeat(&self) -> SseResult<()> {
        self.comment("").await
    }

    /// 异步结束 SSE 响应体写入。重复调用是安全且幂等的。
    ///
    /// 成功后新事件会返回 `Closed`。该方法会等待响应体队列容量以写入结束标记。
    pub async fn finish(&self) -> SseResult<()> {
        let _guard = self.inner.send_gate.lock().await;
        self.finish_locked().await
    }

    /// 同步非阻塞地尝试发送事件。
    ///
    /// 不等待队列容量，不启动 runtime。队列满返回 `QueueFull`；已有发送/关闭操作持有发送门禁时返回 `Busy`。
    pub fn try_send(&self, event: SseEvent) -> SseResult<()> {
        let _guard = self.inner.send_gate.try_lock().ok_or(SseError::Busy)?;
        let encoded = event.encode(self.inner.max_event_bytes)?;
        self.try_write_encoded(encoded)
    }

    /// 同步非阻塞地尝试发送默认 message 事件。
    pub fn try_send_data(&self, data: impl AsRef<str>) -> SseResult<()> {
        self.try_send(SseEvent::data(data.as_ref().to_string()))
    }

    /// 同步非阻塞地尝试发送 comment。
    pub fn try_comment(&self, text: impl AsRef<str>) -> SseResult<()> {
        self.try_send(SseEvent::comment(text.as_ref().to_string()))
    }

    /// 同步非阻塞地尝试发送 heartbeat comment。
    pub fn try_heartbeat(&self) -> SseResult<()> {
        self.try_comment("")
    }

    /// 同步非阻塞地尝试结束 SSE 响应体写入。重复调用是安全且幂等的。
    ///
    /// 队列满时返回 `QueueFull` 并保持 finishing 状态，调用方可稍后重试。
    pub fn try_finish(&self) -> SseResult<()> {
        let _guard = self.inner.send_gate.try_lock().ok_or(SseError::Busy)?;
        self.try_finish_locked()
    }

    /// 获取 SSE 连接 ID。
    ///
    /// 调用成本 `O(1)`。返回值可用于 `SseHub::{send_to_id, try_send_to_id, get_by_id}`。
    pub fn id(&self) -> SseConnectionId {
        self.inner.id
    }

    /// 判断 SSE 连接是否关闭。
    ///
    /// 会读取底层 socket 状态；若 socket 已关闭，会把 sender 状态推进为 closed。
    /// 调用成本 `O(1)`，无 I/O。
    pub fn is_closed(&self) -> bool {
        if self.inner.handle.is_closed() {
            self.inner
                .state
                .store(SENDER_STATE_CLOSED, Ordering::Release);
            return true;
        }

        self.inner.state.load(Ordering::Acquire) == SENDER_STATE_CLOSED
    }

    /// 获取客户端重连时传入的 `Last-Event-ID`。
    ///
    /// 返回建连时请求头快照；本模块不会基于该值自动重放事件。
    pub fn last_event_id(&self) -> Option<&str> {
        self.inner.last_event_id.as_deref()
    }

    /// 获取远端地址。
    pub fn remote_addr(&self) -> SocketAddr {
        self.inner.handle.get_remote().clone()
    }

    /// 获取本地地址。
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.handle.get_local().clone()
    }

    fn start_heartbeat<R>(&self, runtime: R, interval_ms: usize) -> SseResult<()>
    where
        R: AsyncRuntime<()>,
    {
        let sender = self.clone();
        let runtime_for_task = runtime.clone();
        runtime
            .spawn(async move {
                loop {
                    runtime_for_task.timeout(interval_ms).await;
                    if sender.is_closed() {
                        break;
                    }
                    if sender.heartbeat().await.is_err() {
                        break;
                    }
                }
            })
            .map(|_| ())
            .map_err(SseError::Io)
    }

    async fn write_encoded(&self, encoded: Vec<u8>) -> SseResult<()> {
        if self.is_closed() || self.inner.state.load(Ordering::Acquire) != SENDER_STATE_OPEN {
            return Err(SseError::Closed);
        }

        match self.inner.response.write(encoded).await {
            Ok(_) => Ok(()),
            Err(e) => {
                self.inner
                    .state
                    .store(SENDER_STATE_CLOSED, Ordering::Release);
                Err(SseError::from(e))
            }
        }
    }

    fn try_write_encoded(&self, encoded: Vec<u8>) -> SseResult<()> {
        if self.is_closed() || self.inner.state.load(Ordering::Acquire) != SENDER_STATE_OPEN {
            return Err(SseError::Closed);
        }

        match self.inner.response.try_write(encoded) {
            Ok(_) => Ok(()),
            Err(e) => {
                let error = SseError::from(e);
                if matches!(error, SseError::Closed) {
                    self.inner
                        .state
                        .store(SENDER_STATE_CLOSED, Ordering::Release);
                }
                Err(error)
            }
        }
    }

    async fn finish_locked(&self) -> SseResult<()> {
        match self.inner.state.load(Ordering::Acquire) {
            SENDER_STATE_CLOSED => return Ok(()),
            SENDER_STATE_OPEN => {
                self.inner
                    .state
                    .store(SENDER_STATE_FINISHING, Ordering::Release);
            }
            _ => (),
        }

        match self.inner.response.finish().await {
            Ok(_) => {
                self.inner
                    .state
                    .store(SENDER_STATE_CLOSED, Ordering::Release);
                Ok(())
            }
            Err(e) => {
                self.inner
                    .state
                    .store(SENDER_STATE_CLOSED, Ordering::Release);
                Err(SseError::from(e))
            }
        }
    }

    fn try_finish_locked(&self) -> SseResult<()> {
        match self.inner.state.load(Ordering::Acquire) {
            SENDER_STATE_CLOSED => return Ok(()),
            SENDER_STATE_OPEN => {
                self.inner
                    .state
                    .store(SENDER_STATE_FINISHING, Ordering::Release);
            }
            _ => (),
        }

        match self.inner.response.try_finish() {
            Ok(_) => {
                self.inner
                    .state
                    .store(SENDER_STATE_CLOSED, Ordering::Release);
                Ok(())
            }
            Err(e) => {
                let error = SseError::from(e);
                if !matches!(error, SseError::QueueFull | SseError::Busy) {
                    self.inner
                        .state
                        .store(SENDER_STATE_CLOSED, Ordering::Release);
                }
                Err(error)
            }
        }
    }
}

/// SSE 连接快照。
///
/// 功能说明：
/// - `SseHub::snapshot` 返回该结构，用于观测当前 Hub 中的活动 SSE 连接。
///
/// 边界：
/// - 这是快照，不是实时视图；返回后连接可能立即关闭或被移除。
/// - 不暴露 `SseSender`，避免观测接口意外持有发送能力。
///
/// 性能与安全：
/// - clone 成本与 `K` 和 `last_event_id` 大小相关。
/// - 无锁字段，线程安全由所有权移动保证。
///
/// 测试入口：
/// - Hub 相关单元测试覆盖 ID、key 和关闭状态；真实网络测试覆盖连接进入 Hub 后发送。
#[derive(Clone, Debug)]
pub struct SseConnectionInfo<K> {
    /// 业务 key。
    pub key: K,
    /// 透明 SSE 连接 ID。
    pub id: SseConnectionId,
    /// 远端地址。
    pub remote_addr: SocketAddr,
    /// 本地地址。
    pub local_addr: SocketAddr,
    /// 建连时客户端传入的 `Last-Event-ID`。
    pub last_event_id: Option<String>,
    /// 快照时连接是否已关闭。
    pub closed: bool,
}

#[derive(Clone)]
struct SseHubEntry<K, S: Socket> {
    key: K,
    sender: SseSender<S>,
}

/// SSE 活动连接注册表。
///
/// 功能说明：
/// - `SseHub` 是轻量 `Clone + Send + Sync` 句柄，内部管理活动 SSE 连接。
/// - 业务可按 key 注册多个连接，也可按透明 `SseConnectionId` 定向访问单连接。
/// - Hub 不拥有底层 socket，只保存 `SseSender` clone；连接关闭后可自动或显式清理。
///
/// 入参/出参：
/// - `K` 是业务 key，必须 `Eq + Hash + Clone + Send + Sync + 'static`。
/// - `S` 是具体 socket 类型；`TcpSocket` 和 `TlsSocket` 使用不同 Hub，不在 SSE 模块中类型擦除。
/// - `register` 返回透明连接 ID；`unregister` 返回 sender 且不关闭连接。
/// - 批量发送返回 `SseSendReport`，调用方应检查失败分类。
///
/// 生命周期与边界：
/// - `unregister` 只摘除，不发送结束帧。
/// - `close` / `try_close` 会摘除并调用 sender 的 finish/try_finish。
/// - `remove_closed` 只清理已关闭连接。
/// - Hub 不做历史事件重放，不保存已发送事件。
///
/// 性能与阻塞：
/// - 内部使用 `DashMap` + `WyHasherBuilder`；按 ID 操作均摊 `O(1)`。
/// - 按 key 操作需要 clone 当前 key 下 ID 列表，复杂度 `O(m)`，`m` 为该 key 连接数。
/// - broadcast 复杂度 `O(c)`，`c` 为 Hub 当前连接数。
/// - async 发送可能在 sender 队列上异步挂起；`try_*` 发送不阻塞。
///
/// 副作用、幂等与锁：
/// - register/unregister/close 会修改 Hub 内部表，有副作用，不是纯函数。
/// - unregister 同一 ID 第一次返回 sender，后续返回 None。
/// - close 同一 ID 第一次关闭，后续返回 `ConnectionNotFound`。
/// - 异步发送 API 会先快照 sender，再释放 DashMap guard 后 await，避免异步死锁。
///
/// 测试入口：
/// - `sse_hub_register_get_unregister` 覆盖注册、查询和摘除。
/// - `sse_hub_try_send_to_id_reports_result` 覆盖 ID 定向发送报告。
/// - `sse_hub_close_removes_and_finishes` 覆盖关闭副作用。
/// - `sse_real_network_get_stream_receives_chunked_event` 覆盖真实跨线程 Hub 发送。
pub struct SseHub<K, S: Socket> {
    inner: Arc<SseHubInner<K, S>>,
}

struct SseHubInner<K, S: Socket> {
    by_id: DashMap<SseConnectionId, SseHubEntry<K, S>, WyHasherBuilder>,
    by_key: DashMap<K, Vec<SseConnectionId>, WyHasherBuilder>,
    auto_remove_closed: bool,
}

impl<K, S: Socket> Clone for SseHub<K, S> {
    fn clone(&self) -> Self {
        SseHub {
            inner: self.inner.clone(),
        }
    }
}

impl<K, S> SseHub<K, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    S: Socket,
{
    /// 构建 Hub Builder。
    ///
    /// 调用成本 `O(1)`，不分配连接表，不阻塞，无外部副作用。
    pub fn builder() -> SseHubBuilder<K, S> {
        SseHubBuilder {
            auto_remove_closed: true,
            marker: PhantomData,
        }
    }

    /// 注册 SSE sender。
    ///
    /// 成功后可通过 key 或 ID 找回 sender。若 ID 为 0 或与活动连接冲突，返回
    /// `ConnectionIdExhausted`。该方法不关闭、不发送、不启动任务；ID 表写入成功后才会
    /// 更新 key 索引，避免重复注册造成索引残留。
    pub fn register(&self, key: K, sender: SseSender<S>) -> SseResult<SseConnectionId> {
        let id = sender.id();
        if id.get() == 0 {
            return Err(SseError::ConnectionIdExhausted);
        }

        match self.inner.by_id.entry(id) {
            Entry::Occupied(_) => return Err(SseError::ConnectionIdExhausted),
            Entry::Vacant(slot) => {
                slot.insert(SseHubEntry {
                    key: key.clone(),
                    sender,
                });
            }
        }

        self.inner
            .by_key
            .entry(key)
            .or_insert_with(Vec::new)
            .push(id);

        Ok(id)
    }

    /// 摘除 SSE sender，不关闭连接。
    ///
    /// 返回被摘除的 sender，调用方可继续直接发送或关闭。重复摘除同一 ID 返回 None。
    pub fn unregister(&self, id: SseConnectionId) -> Option<SseSender<S>> {
        let (_, entry) = self.inner.by_id.remove(&id)?;
        self.remove_id_from_key(&entry.key, id);
        Some(entry.sender)
    }

    /// 摘除并异步关闭 SSE 连接。
    ///
    /// 先获取 sender 快照，再调用 sender `finish`，结束帧成功入队后从 Hub 摘除。
    /// 这样不会持有 Hub 内部锁跨 `.await`，也避免关闭失败时过早丢失连接索引。
    pub async fn close(&self, id: SseConnectionId) -> SseResult<()> {
        let sender = self.get_by_id(id).ok_or(SseError::ConnectionNotFound)?;
        let result = sender.finish().await;
        if result.is_ok() || matches!(result, Err(SseError::Closed)) {
            let _ = self.unregister(id);
        }
        result
    }

    /// 摘除并同步非阻塞地尝试关闭 SSE 连接。
    ///
    /// 不等待队列容量；队列满时返回 `QueueFull`，并保留 Hub 注册关系，调用方可稍后重试。
    /// 只有结束帧成功入队或连接已经确认关闭时，才从 Hub 摘除。
    pub fn try_close(&self, id: SseConnectionId) -> SseResult<()> {
        let sender = self.get_by_id(id).ok_or(SseError::ConnectionNotFound)?;
        let result = sender.try_finish();
        if result.is_ok() || matches!(result, Err(SseError::Closed)) {
            let _ = self.unregister(id);
        }
        result
    }

    /// 按业务 key 获取 sender 快照。
    ///
    /// 返回当前匹配 key 的 sender clone 列表。该结果是快照，不保证返回后连接仍活动。
    pub fn get(&self, key: &K) -> Vec<SseSender<S>> {
        let ids = self
            .inner
            .by_key
            .get(key)
            .map(|ids| ids.clone())
            .unwrap_or_default();
        ids.into_iter()
            .filter_map(|id| self.get_by_id(id))
            .collect()
    }

    /// 按透明连接 ID 获取 sender。
    ///
    /// 调用成本均摊 `O(1)`。返回 sender clone，不持有 DashMap guard。
    pub fn get_by_id(&self, id: SseConnectionId) -> Option<SseSender<S>> {
        self.inner.by_id.get(&id).map(|entry| entry.sender.clone())
    }

    /// 获取当前 SSE 活动连接快照。
    ///
    /// 复杂度 `O(c)`，`c` 为 Hub 当前连接数。该方法会调用 sender 地址和关闭状态 getter。
    pub fn snapshot(&self) -> Vec<SseConnectionInfo<K>> {
        self.inner
            .by_id
            .iter()
            .map(|entry| {
                let sender = &entry.value().sender;
                SseConnectionInfo {
                    key: entry.value().key.clone(),
                    id: *entry.key(),
                    remote_addr: sender.remote_addr(),
                    local_addr: sender.local_addr(),
                    last_event_id: sender.last_event_id().map(ToOwned::to_owned),
                    closed: sender.is_closed(),
                }
            })
            .collect()
    }

    /// 移除已关闭 sender。
    ///
    /// 返回移除数量。复杂度 `O(c)`；只摘除 Hub 条目，不额外写结束帧。
    pub fn remove_closed(&self) -> usize {
        let ids = self
            .inner
            .by_id
            .iter()
            .filter_map(|entry| {
                if entry.value().sender.is_closed() {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let len = ids.len();
        for id in ids {
            let _ = self.unregister(id);
        }
        len
    }

    /// 按业务 key 异步发送事件。
    ///
    /// 会向 key 下每个连接发送事件，并汇总报告。发送前会 clone sender 列表，避免持锁 await。
    pub async fn send_to(&self, key: &K, event: SseEvent) -> SseSendReport {
        let senders = self.get(key);
        self.send_many(senders, event).await
    }

    /// 按业务 key 同步非阻塞地尝试发送事件。
    ///
    /// 不等待队列容量；每个失败连接会记录到 `SseSendReport::failures`。
    pub fn try_send_to(&self, key: &K, event: SseEvent) -> SseSendReport {
        let senders = self.get(key);
        self.try_send_many(senders, event)
    }

    /// 按透明连接 ID 异步发送事件。
    ///
    /// ID 不存在时返回 not_found 报告，不 panic。
    pub async fn send_to_id(&self, id: SseConnectionId, event: SseEvent) -> SseSendReport {
        match self.get_by_id(id) {
            Some(sender) => self.send_many(vec![sender], event).await,
            None => SseSendReport::not_found(id),
        }
    }

    /// 按透明连接 ID 同步非阻塞地尝试发送事件。
    ///
    /// 这是非异步环境向单连接推送事件的主要 API。
    pub fn try_send_to_id(&self, id: SseConnectionId, event: SseEvent) -> SseSendReport {
        match self.get_by_id(id) {
            Some(sender) => self.try_send_many(vec![sender], event),
            None => SseSendReport::not_found(id),
        }
    }

    /// 向所有 SSE 活动连接异步广播事件。
    ///
    /// 会 clone 当前所有 sender 后逐个异步发送；复杂度 `O(c * n)`，`n` 为事件编码长度。
    pub async fn broadcast(&self, event: SseEvent) -> SseSendReport {
        let senders = self
            .inner
            .by_id
            .iter()
            .map(|entry| entry.value().sender.clone())
            .collect::<Vec<_>>();
        self.send_many(senders, event).await
    }

    /// 向所有 SSE 活动连接同步非阻塞地尝试广播事件。
    ///
    /// 不等待任意连接队列容量；适合定时器或外部同步线程做尽力广播。
    pub fn try_broadcast(&self, event: SseEvent) -> SseSendReport {
        let senders = self
            .inner
            .by_id
            .iter()
            .map(|entry| entry.value().sender.clone())
            .collect::<Vec<_>>();
        self.try_send_many(senders, event)
    }

    async fn send_many(&self, senders: Vec<SseSender<S>>, event: SseEvent) -> SseSendReport {
        let mut report = SseSendReport::new(senders.len());
        for sender in senders {
            let id = sender.id();
            match sender.send(event.clone()).await {
                Ok(_) => report.sent += 1,
                Err(error) => report.record(id, &error),
            }
        }
        if self.inner.auto_remove_closed {
            let _ = self.remove_closed();
        }
        report
    }

    fn try_send_many(&self, senders: Vec<SseSender<S>>, event: SseEvent) -> SseSendReport {
        let mut report = SseSendReport::new(senders.len());
        for sender in senders {
            let id = sender.id();
            match sender.try_send(event.clone()) {
                Ok(_) => report.sent += 1,
                Err(error) => report.record(id, &error),
            }
        }
        if self.inner.auto_remove_closed {
            let _ = self.remove_closed();
        }
        report
    }

    fn remove_id_from_key(&self, key: &K, id: SseConnectionId) {
        let remove_key = if let Some(mut ids) = self.inner.by_key.get_mut(key) {
            ids.retain(|item| item != &id);
            ids.is_empty()
        } else {
            false
        };

        if remove_key {
            self.inner.by_key.remove(key);
        }
    }
}

/// SSE Hub Builder。
///
/// 功能说明：
/// - 构建 `SseHub<K, S>`，当前只暴露自动清理已关闭连接的策略。
///
/// 性能与安全：
/// - Builder setter 为 `O(1)`，不阻塞，无 I/O。
/// - `build` 分配两个 `DashMap`，成本 `O(1)`。
///
/// 测试入口：
/// - Hub 单元测试和真实网络测试均通过默认 Builder 构建 Hub。
pub struct SseHubBuilder<K, S: Socket> {
    auto_remove_closed: bool,
    marker: PhantomData<(K, S)>,
}

impl<K, S> SseHubBuilder<K, S>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    S: Socket,
{
    /// 设置发送后是否自动清理已关闭连接。
    pub fn auto_remove_closed(mut self, auto_remove_closed: bool) -> Self {
        self.auto_remove_closed = auto_remove_closed;
        self
    }

    /// 完成 Hub 构建。
    pub fn build(self) -> SseHub<K, S> {
        SseHub {
            inner: Arc::new(SseHubInner {
                by_id: DashMap::with_hasher(WyHasherBuilder::default()),
                by_key: DashMap::with_hasher(WyHasherBuilder::default()),
                auto_remove_closed: self.auto_remove_closed,
            }),
        }
    }
}

/// SSE 发送失败分类。
///
/// 该枚举用于批量发送报告，不携带错误字符串，便于调用方按失败类型快速计数和处理。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseSendFailureKind {
    /// 连接已关闭。
    Closed,
    /// 队列已满。
    QueueFull,
    /// sender 正忙。
    Busy,
    /// 连接不存在。
    ConnectionNotFound,
    /// 其它 I/O 错误。
    Io,
    /// 事件非法。
    InvalidEvent,
}

/// 单个连接的发送失败记录。
///
/// `id` 指向失败连接，`kind` 是归一化失败分类。该类型为 `Copy`，传递成本 `O(1)`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SseSendFailure {
    /// 连接 ID。
    pub id: SseConnectionId,
    /// 失败分类。
    pub kind: SseSendFailureKind,
}

/// SSE 批量发送报告。
///
/// 功能说明：
/// - `SseHub` 的 key、ID 和 broadcast 发送 API 返回该报告。
/// - 报告区分成功入队、关闭、队列满、发送门禁忙、连接不存在和其它失败。
///
/// 边界：
/// - `sent` 只表示进入响应队列，不表示客户端已经收到。
/// - `total` 是本次发送尝试的目标连接数；按不存在 ID 发送时 `total` 为 0，
///   `not_found` 为 1。
///
/// 性能：
/// - 构造 `O(1)`；每次失败记录会向 `failures` 追加一项，空间复杂度 `O(f)`，
///   `f` 为失败连接数。
///
/// 测试入口：
/// - `sse_hub_try_send_to_id_reports_result` 覆盖成功、队列满和 not found 报告。
#[derive(Clone, Debug, Default)]
pub struct SseSendReport {
    /// 目标连接总数。
    pub total: usize,
    /// 成功进入响应队列的连接数。
    pub sent: usize,
    /// 已关闭连接数。
    pub closed: usize,
    /// 队列满连接数。
    pub queue_full: usize,
    /// sender 正忙连接数。
    pub busy: usize,
    /// 未找到连接数。
    pub not_found: usize,
    /// 其它失败连接数。
    pub failed: usize,
    /// 逐连接失败记录。
    pub failures: Vec<SseSendFailure>,
}

impl SseSendReport {
    fn new(total: usize) -> Self {
        SseSendReport {
            total,
            ..SseSendReport::default()
        }
    }

    fn not_found(id: SseConnectionId) -> Self {
        let mut report = SseSendReport::new(0);
        report.not_found = 1;
        report.failures.push(SseSendFailure {
            id,
            kind: SseSendFailureKind::ConnectionNotFound,
        });
        report
    }

    fn record(&mut self, id: SseConnectionId, error: &SseError) {
        let kind = match error {
            SseError::Closed => {
                self.closed += 1;
                SseSendFailureKind::Closed
            }
            SseError::QueueFull => {
                self.queue_full += 1;
                SseSendFailureKind::QueueFull
            }
            SseError::Busy => {
                self.busy += 1;
                SseSendFailureKind::Busy
            }
            SseError::ConnectionNotFound => {
                self.not_found += 1;
                SseSendFailureKind::ConnectionNotFound
            }
            SseError::InvalidEvent(_) | SseError::EventTooLarge { .. } => {
                self.failed += 1;
                SseSendFailureKind::InvalidEvent
            }
            _ => {
                self.failed += 1;
                SseSendFailureKind::Io
            }
        };
        self.failures.push(SseSendFailure { id, kind });
    }
}

fn accepts_event_stream<S: Socket>(req: &HttpRequest<S>) -> bool {
    req.headers()
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_ascii_lowercase())
                .any(|item| item.starts_with("text/event-stream") || item.starts_with("*/*"))
        })
        .unwrap_or(false)
}

fn sse_error_to_response(error: SseError) -> HttpResponse {
    let status = match error {
        SseError::InvalidMethod(_) => StatusCode::METHOD_NOT_ALLOWED,
        SseError::InvalidConfig(_) => StatusCode::INTERNAL_SERVER_ERROR,
        SseError::InvalidEvent(_) | SseError::EventTooLarge { .. } => StatusCode::BAD_REQUEST,
        SseError::QueueFull | SseError::Busy => StatusCode::SERVICE_UNAVAILABLE,
        SseError::Closed | SseError::ConnectionNotFound => StatusCode::GONE,
        SseError::ConnectionIdExhausted | SseError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    sse_error_response(status, error.to_string())
}

fn sse_error_response(status: StatusCode, body: impl AsRef<str>) -> HttpResponse {
    let mut resp = HttpResponse::new(1);
    resp.status(status.as_u16());
    resp.insert_header(CONTENT_TYPE.as_str(), "text/plain; charset=utf-8");
    if let Some(resp_body) = resp.as_mut_body() {
        let _ = resp_body.init();
        resp_body.push(body.as_ref().as_bytes());
    }
    resp
}

fn validate_single_line_field(name: &str, value: &str) -> SseResult<()> {
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        return Err(SseError::InvalidEvent(format!(
            "{} must not contain CR, LF or NUL",
            name
        )));
    }
    Ok(())
}

fn validate_text_field(name: &str, value: &str) -> SseResult<()> {
    if value.contains('\0') {
        return Err(SseError::InvalidEvent(format!(
            "{} must not contain NUL",
            name
        )));
    }
    Ok(())
}

fn encode_multiline_field(buf: &mut Vec<u8>, prefix: &str, value: &str, add_space_for_empty: bool) {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.split('\n') {
        buf.extend_from_slice(prefix.as_bytes());
        if prefix == ":" && !line.is_empty() {
            buf.extend_from_slice(b" ");
        }
        if add_space_for_empty || !line.is_empty() {
            buf.extend_from_slice(line.as_bytes());
        }
        buf.extend_from_slice(b"\n");
    }
}

fn read_last_event_id<S: Socket>(req: &HttpRequest<S>) -> SseResult<Option<String>> {
    let header_name = HeaderName::from_static(LAST_EVENT_ID_HEADER);
    if let Some(value) = req.headers().get(header_name) {
        let value = value
            .to_str()
            .map_err(|e| SseError::InvalidEvent(format!("invalid Last-Event-ID: {:?}", e)))?;
        if value.is_empty() {
            Ok(None)
        } else {
            validate_single_line_field(LAST_EVENT_ID_HEADER, value)?;
            Ok(Some(value.to_string()))
        }
    } else {
        Ok(None)
    }
}

fn next_connection_id() -> SseConnectionId {
    loop {
        let id = SSE_CONNECTION_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return SseConnectionId(id);
        }
    }
}

#[cfg(test)]
mod tests {
    //! SSE 单元测试模块。
    //!
    //! 本模块只覆盖生产侧 `pi_http::sse` 的静态逻辑/API 分支，包括事件编码、配置校验、
    //! sender 队列语义、Hub 注册表行为和类型安全边界。这里的 `TestSocket` 是最小测试桩，
    //! 只用于构造 `SocketHandle` 与 `ResponseHandler`，不验证真实网络 I/O。
    //!
    //! 非单元测试不得使用该测试桩；真实网络行为由
    //! `crates/http/tests/sse_real_network.rs` 中的 `sse_real_network_get_stream_receives_chunked_event`
    //! 负责验证。

    use std::cell::UnsafeCell;
    use std::future::Future;
    use std::io::Result as IoResult;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::task::Waker;

    use bytes::BytesMut;
    use crossbeam_channel::unbounded;
    use futures::executor::block_on;
    use https::{HeaderMap, HeaderValue, Version};
    use mio::Token;
    use pi_async_rt::rt::serial::AsyncValue;
    use tcp::{
        utils::{Hibernate, Ready, SocketContext},
        Socket, SocketEvent, SocketHandle, SocketImage,
    };

    use super::*;
    use crate::gateway::GatewayContext;
    use crate::middleware::{Middleware, MiddlewareResult};
    use crate::request::HttpRequest;
    use crate::response::HttpResponse;
    use crate::utils::HttpRecvResult;

    /// SSE 单元测试用最小 socket 桩。
    ///
    /// 对应生产侧宿主：`SseSender<TestSocket>` 只读取 `SocketHandle` 的地址和关闭状态，
    /// 并通过 `ResponseHandler` 队列验证发送语义；不会调用真实 socket 读写。
    ///
    /// 边界：
    /// - 该类型不用于集成测试或专项网络测试。
    /// - 未使用的 trait 方法显式 `unimplemented!` 或返回无副作用默认值，避免误把它当真实 socket。
    ///
    /// 性能/安全：
    /// - 所有被测试路径调用的方法均为 `O(1)`，不阻塞、不执行 I/O。
    /// - 线程安全性由 `SocketHandle` 测试镜像承担；该桩只在单元测试内使用。
    struct TestSocket;

    impl Socket for TestSocket {
        fn is_closed(&self) -> bool {
            false
        }

        fn is_flush(&self) -> bool {
            true
        }

        fn set_flush(&self, _flush: bool) {}

        fn get_handle(&self) -> SocketHandle<Self> {
            unimplemented!("test socket does not expose handle from inner socket")
        }

        fn remove_handle(&mut self) -> Option<SocketHandle<Self>> {
            None
        }

        fn get_local(&self) -> &std::net::SocketAddr {
            unimplemented!("test socket local addr is read from SocketImage")
        }

        fn get_remote(&self) -> &std::net::SocketAddr {
            unimplemented!("test socket remote addr is read from SocketImage")
        }

        fn get_token(&self) -> Option<&Token> {
            None
        }

        fn get_uid(&self) -> Option<&usize> {
            None
        }

        fn get_context(&self) -> Rc<UnsafeCell<SocketContext>> {
            unimplemented!("unused by SSE tests")
        }

        fn set_timeout(&self, _timeout: usize, _event: SocketEvent) {}

        fn unset_timeout(&self) {}

        fn is_security(&self) -> bool {
            false
        }

        fn read_ready(&mut self, _adjust: usize) -> Result<AsyncValue<usize>, usize> {
            unimplemented!("unused by SSE tests")
        }

        fn is_wait_wakeup_read_ready(&self) -> bool {
            false
        }

        fn wakeup_read_ready(&mut self) {}

        fn get_read_buffer(&self) -> Rc<UnsafeCell<Option<BytesMut>>> {
            unimplemented!("unused by SSE tests")
        }

        fn get_write_buffer(&mut self) -> Option<&mut BytesMut> {
            None
        }

        fn write_ready<B>(&mut self, _buf: B) -> IoResult<()>
        where
            B: AsRef<[u8]> + 'static,
        {
            Ok(())
        }

        fn reregister_interest(&mut self, _ready: Ready) -> IoResult<()> {
            Ok(())
        }

        fn is_hibernated(&self) -> bool {
            false
        }

        fn push_hibernated_task<F>(&self, _task: F)
        where
            F: Future<Output = ()> + 'static,
        {
        }

        fn run_hibernated_tasks(&self) {}

        fn hibernate(&self, _handle: SocketHandle<Self>, _ready: Ready) -> Option<Hibernate<Self>> {
            None
        }

        fn set_hibernate(&self, _hibernate: Hibernate<Self>) -> bool {
            false
        }

        fn set_hibernate_wakers(&self, _waker: Waker) {}

        fn wakeup(&mut self, _result: IoResult<()>) -> bool {
            true
        }

        fn close(&mut self, _reason: IoResult<()>) -> IoResult<()> {
            Ok(())
        }
    }

    /// 构造单元测试用 `SocketHandle<TestSocket>`。
    ///
    /// 该 handle 只服务于 `SseSender` 地址、ID 和关闭状态读取。它不绑定真实端口，
    /// 不产生网络副作用。真实 TCP 生命周期由集成测试覆盖。
    fn test_handle() -> SocketHandle<TestSocket> {
        let local = "127.0.0.1:8080".parse().unwrap();
        let remote = "127.0.0.1:10000".parse().unwrap();
        let (close_sender, _) = unbounded();
        let (timer_sender, _) = unbounded();
        let socket = Arc::new(UnsafeCell::new(TestSocket));
        let image = SocketImage::new(
            &socket,
            local,
            remote,
            Token(1),
            1,
            false,
            Arc::new(AtomicBool::new(false)),
            close_sender,
            timer_sender,
        );

        SocketHandle::new(image)
    }

    /// 单元测试用 SSE fixture。
    ///
    /// `_resp` 必须与 `sender` 同生命周期保存；否则 `HttpResponse` drop 后响应体消费者被释放，
    /// 生产侧 `try_send` 会按设计返回断连。该 fixture 明确测试“响应仍由 HTTP 流程持有”这一
    /// 生产侧前置条件。
    struct TestSseFixture {
        _resp: HttpResponse,
        sender: SseSender<TestSocket>,
    }

    /// 构造单元测试用 sender fixture。
    ///
    /// `channel_size` 直接映射到生产侧 `HttpResponse::new` 的响应体队列容量，用于验证
    /// `SseSender::try_send` 的队列满分支。该函数不执行 I/O，时间/空间复杂度均为 `O(1)`。
    fn test_sender(channel_size: usize) -> TestSseFixture {
        let resp = HttpResponse::new(channel_size);
        let handler = resp.get_response_handler().unwrap();
        let sender = SseSender::new(
            next_connection_id(),
            test_handle(),
            handler,
            DEFAULT_MAX_EVENT_BYTES,
            Some("last-id".to_string()),
        );

        TestSseFixture {
            _resp: resp,
            sender,
        }
    }

    /// 构造单元测试用 GET `/sse` 请求。
    ///
    /// 该 helper 只用于中间件静态分支测试，不绑定真实网络端口；真实请求解析和 TCP 输出由
    /// `sse_real_network` 集成测试覆盖。
    fn test_get_request(headers: HeaderMap) -> HttpRequest<TestSocket> {
        HttpRequest::new(
            test_handle(),
            "GET",
            "http://127.0.0.1/sse",
            Version::HTTP_11,
            headers,
            &[],
        )
        .expect("test HTTP request must be created")
    }

    /// 构造单元测试用 SSE 中间件配置。
    ///
    /// 关闭初始 comment 和心跳，避免中间件测试的响应体队列被无关帧污染。
    fn test_middleware_config() -> SseConfig {
        SseConfig::builder()
            .channel_size(8)
            .heartbeat_interval_ms(0)
            .send_initial_comment(false)
            .build()
            .expect("test middleware config must be valid")
    }

    fn expect_finish(
        result: MiddlewareResult<TestSocket>,
    ) -> (HttpRequest<TestSocket>, HttpResponse) {
        match result {
            MiddlewareResult::Finish(pair) => pair,
            _ => panic!("SSE middleware test expected Finish"),
        }
    }

    fn expect_break(result: MiddlewareResult<TestSocket>) -> HttpResponse {
        match result {
            MiddlewareResult::Break(resp) => resp,
            _ => panic!("SSE middleware test expected Break"),
        }
    }

    fn response_text(resp: HttpResponse) -> String {
        String::from_utf8(Vec::<u8>::from(resp)).expect("test response must be UTF-8")
    }

    fn drain_event(resp: &HttpResponse) -> String {
        let body = resp.as_body().expect("test response must have body");
        match block_on(body.next()) {
            HttpRecvResult::Ok(Some((_index, chunk))) => {
                String::from_utf8(chunk).expect("test SSE event must be UTF-8")
            }
            _ => panic!("test response body must yield one SSE event"),
        }
    }

    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_clone<T: Clone>() {}

    /// 测试生产侧 `SseEvent` 的标准字段编码、多行 data、comment、id 和 retry。
    #[test]
    fn sse_event_builder_and_encoding() {
        let event = SseEvent::builder()
            .comment("ready")
            .id("42")
            .event("notice")
            .retry(3000)
            .data("hello\nworld")
            .build()
            .unwrap();

        let encoded = event.encode(1024).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            ": ready\nid: 42\nevent: notice\nretry: 3000\ndata: hello\ndata: world\n\n"
        );
    }

    /// 测试生产侧 `SseEvent` 对非法单行字段和超大事件的拒绝。
    #[test]
    fn sse_event_rejects_invalid_or_large_event() {
        assert!(matches!(
            SseEvent::builder().event("bad\nname").build(),
            Err(SseError::InvalidEvent(_))
        ));

        let large = SseEvent::data("0123456789");
        assert!(matches!(
            large.encode(4),
            Err(SseError::EventTooLarge { .. })
        ));
    }

    /// 测试生产侧 `SseConfigBuilder` 对非法配置返回错误而不是静默修正。
    #[test]
    fn sse_config_builder_rejects_invalid_values() {
        assert!(matches!(
            SseConfig::builder().channel_size(0).build(),
            Err(SseError::InvalidConfig(_))
        ));
        assert!(matches!(
            SseConfig::builder().max_event_bytes(0).build(),
            Err(SseError::InvalidConfig(_))
        ));
    }

    /// 测试生产侧 `SseResponseBuilder::build` 只接受 GET 请求，非 GET 不创建响应队列和 sender。
    #[test]
    fn sse_response_builder_rejects_non_get_method() {
        let req = HttpRequest::new(
            test_handle(),
            "POST",
            "http://127.0.0.1/sse",
            Version::HTTP_11,
            HeaderMap::new(),
            &[],
        )
        .expect("test HTTP request must be created");

        assert!(matches!(
            SseResponse::builder(&req).build(),
            Err(SseError::InvalidMethod(method)) if method == "POST"
        ));
    }

    /// 测试生产侧 `SseSender::try_*` 的同步非阻塞队列满语义。
    #[test]
    fn sse_sender_try_send_reports_queue_full() {
        let fixture = test_sender(1);
        let sender = &fixture.sender;
        sender.try_send_data("first").unwrap();
        assert!(matches!(
            sender.try_send_data("second"),
            Err(SseError::QueueFull)
        ));
    }

    /// 测试生产侧 `SseSender::finish` 的幂等关闭语义和 `send` 后关闭拒绝。
    #[test]
    fn sse_sender_send_and_finish_are_ordered() {
        let fixture = test_sender(4);
        let sender = &fixture.sender;
        block_on(sender.send_data("first")).unwrap();
        block_on(sender.finish()).unwrap();
        block_on(sender.finish()).unwrap();
        assert!(matches!(
            block_on(sender.send_data("after-finish")),
            Err(SseError::Closed)
        ));
    }

    /// 测试生产侧 `SseSender` 在同一线程连续成功调用时严格按调用顺序写入响应队列。
    ///
    /// 该测试直接读取生产侧 `HttpResponse` 的响应体消费者，验证顺序语义落在真实队列输出上，
    /// 而不是只验证 API 返回值。多线程并发顺序按实际入队顺序定义，无法预设固定事件名顺序；
    /// 因此本测试覆盖确定性的同线程顺序分支。
    #[test]
    fn sse_sender_preserves_same_thread_successful_enqueue_order() {
        let fixture = test_sender(8);
        let sender = &fixture.sender;

        sender.try_send_data("first").unwrap();
        sender
            .try_send(SseEvent::named("notice", "second"))
            .unwrap();
        sender.try_finish().unwrap();

        let body = fixture._resp.as_body().unwrap();
        let first = match block_on(body.next()) {
            HttpRecvResult::Ok(Some((_index, chunk))) => String::from_utf8(chunk).unwrap(),
            _ => panic!("test response body must yield the first SSE event"),
        };
        let second = match block_on(body.next()) {
            HttpRecvResult::Ok(Some((_index, chunk))) => String::from_utf8(chunk).unwrap(),
            _ => panic!("test response body must yield the second SSE event"),
        };

        assert!(
            first.contains("data: first"),
            "first queued event must be the first user call, got: {}",
            first
        );
        assert!(
            second.contains("event: notice") && second.contains("data: second"),
            "second queued event must be the second user call, got: {}",
            second
        );
        assert!(matches!(block_on(body.next()), HttpRecvResult::Fin(None)));
    }

    /// 测试生产侧 `SseSender` 和 `SseHub` 的跨线程安全与轻量 Clone 类型边界。
    #[test]
    fn sse_sender_and_hub_are_send_sync_clone() {
        assert_send_sync::<SseSender<TestSocket>>();
        assert_clone::<SseSender<TestSocket>>();
        assert_send_sync::<SseHub<String, TestSocket>>();
        assert_clone::<SseHub<String, TestSocket>>();
        assert_send_sync::<SseMiddleware<SseConnectionId, TestSocket>>();
        assert_clone::<SseMiddleware<SseConnectionId, TestSocket>>();
    }

    /// 测试生产侧 `SseMiddleware` 默认以透明连接 ID 作为 Hub key，并在响应阶段旁路 stream。
    ///
    /// 该测试覆盖默认中间件的最小接入路径：外部只提供 Hub，中间件负责构建 SSE 响应、
    /// 注册 sender，并把 stream response 在响应阶段以 `Break` 返回给网关。
    #[test]
    fn sse_middleware_registers_default_connection_id_key() {
        let hub = SseHub::<SseConnectionId, TestSocket>::builder().build();
        let middleware = SseMiddleware::builder(hub.clone())
            .config(test_middleware_config())
            .build()
            .expect("default SSE middleware must build");
        let mut context = GatewayContext::new();
        let req = test_get_request(HeaderMap::new());

        let (req, resp) = expect_finish(block_on(middleware.request(&mut context, req)));
        assert!(resp.is_stream());

        let snapshot = hub.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].key, snapshot[0].id);

        let resp = expect_break(block_on(middleware.response(&mut context, req, resp)));
        assert!(resp.is_stream());
    }

    /// 测试生产侧 `SseMiddleware` 的外部 acceptor 和 `on_open` 回调。
    ///
    /// 该测试验证：
    /// - 外部可在 acceptor 中获知当前 HTTP 连接请求打开 SSE，并决定业务 key。
    /// - `on_open` 在 Hub 注册成功后得到可长期保存的 `SseSender` 安全句柄。
    /// - 同一个业务 key 下允许多个不同 HTTP 连接同时打开各自 SSE。
    /// - 通过 key 发送会同时命中同 key 下的多个 sender。
    #[test]
    fn sse_middleware_custom_acceptor_on_open_and_multi_connection_key() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let saved_senders = Arc::new(StdMutex::new(Vec::<SseSender<TestSocket>>::new()));
        let opened = Arc::new(AtomicU32::new(0));
        let saved_for_open = saved_senders.clone();
        let opened_for_open = opened.clone();
        let middleware = SseMiddleware::with_acceptor(hub.clone(), |accept| {
            assert_eq!(accept.request.url().path(), "/sse");
            assert!(!accept.sender.is_closed());
            Ok(SseAcceptDecision::accept("user-a".to_string()))
        })
        .config(test_middleware_config())
        .on_open(move |open| {
            assert_eq!(open.key, "user-a");
            open.sender
                .try_send_data(format!("connected-{}", open.id.get()))?;
            saved_for_open.lock().unwrap().push(open.sender.clone());
            opened_for_open.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .build()
        .expect("custom SSE middleware must build");
        let mut context = GatewayContext::new();
        let mut responses = Vec::new();

        for _ in 0..2 {
            let req = test_get_request(HeaderMap::new());
            let (_req, resp) = expect_finish(block_on(middleware.request(&mut context, req)));
            assert!(resp.is_stream());
            assert!(drain_event(&resp).contains("data: connected-"));
            responses.push(resp);
        }

        assert_eq!(opened.load(Ordering::Relaxed), 2);
        assert_eq!(saved_senders.lock().unwrap().len(), 2);
        assert_eq!(hub.get(&"user-a".to_string()).len(), 2);

        let report = hub.try_send_to(&"user-a".to_string(), SseEvent::data("broadcast"));
        assert_eq!(report.total, 2);
        assert_eq!(report.sent, 2);
        for resp in &responses {
            assert!(drain_event(resp).contains("data: broadcast"));
        }
    }

    /// 测试生产侧 `SseMiddleware` 允许外部拒绝打开 SSE。
    ///
    /// 被拒绝时应返回普通 HTTP 响应，不注册 Hub，不返回 stream response，也不会要求外部保存
    /// sender。
    #[test]
    fn sse_middleware_acceptor_can_reject_before_stream_is_returned() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let middleware = SseMiddleware::with_acceptor(hub.clone(), |_accept| {
            Ok(SseAcceptDecision::reject(
                StatusCode::FORBIDDEN,
                "sse denied",
            ))
        })
        .config(test_middleware_config())
        .build()
        .expect("rejecting SSE middleware must build");
        let mut context = GatewayContext::new();
        let req = test_get_request(HeaderMap::new());

        let resp = expect_break(block_on(middleware.request(&mut context, req)));
        assert!(!resp.is_stream());
        assert!(hub.snapshot().is_empty());
        let text = response_text(resp).to_ascii_lowercase();
        assert!(text.contains("http/1.1 403"));
        assert!(text.contains("sse denied"));
    }

    /// 测试生产侧 `SseMiddleware` 可选 `Accept: text/event-stream` 校验。
    ///
    /// 缺少 `Accept` 头时返回 `406`；带有 SSE Accept 头时允许继续打开 stream。
    #[test]
    fn sse_middleware_can_require_accept_event_stream_header() {
        let hub = SseHub::<SseConnectionId, TestSocket>::builder().build();
        let middleware = SseMiddleware::builder(hub.clone())
            .config(test_middleware_config())
            .require_accept_header(true)
            .build()
            .expect("accept-check SSE middleware must build");
        let mut context = GatewayContext::new();

        let req = test_get_request(HeaderMap::new());
        let resp = expect_break(block_on(middleware.request(&mut context, req)));
        assert!(!resp.is_stream());
        assert!(response_text(resp).to_ascii_lowercase().contains("406"));
        assert!(hub.snapshot().is_empty());

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let req = test_get_request(headers);
        let (_req, resp) = expect_finish(block_on(middleware.request(&mut context, req)));
        assert!(resp.is_stream());
        assert_eq!(hub.snapshot().len(), 1);
    }

    /// 测试生产侧 `SseMiddlewareBuilder::heartbeat_runtime` 能让默认中间件启动自动心跳。
    ///
    /// 该测试不打开网络，只读取生产侧响应体队列中的 heartbeat comment；真实 TCP 输出仍由
    /// `sse_real_network_get_stream_receives_chunked_event` 负责覆盖。
    #[test]
    fn sse_middleware_heartbeat_runtime_sends_comment() {
        let _timer = pi_async_rt::rt::startup_global_time_loop(1);
        let runtime = pi_async_rt::rt::AsyncRuntimeBuilder::default_multi_thread(
            Some("sse-heartbeat-unit"),
            None,
            Some(1),
            Some(1),
        );
        let hub = SseHub::<SseConnectionId, TestSocket>::builder().build();
        let config = SseConfig::builder()
            .channel_size(8)
            .heartbeat_interval_ms(1)
            .send_initial_comment(false)
            .build()
            .expect("heartbeat middleware config must be valid");
        let middleware = SseMiddleware::builder(hub.clone())
            .config(config)
            .heartbeat_runtime(runtime.clone())
            .build()
            .expect("heartbeat SSE middleware must build");
        let mut context = GatewayContext::new();
        let req = test_get_request(HeaderMap::new());

        let (_req, resp) = expect_finish(block_on(middleware.request(&mut context, req)));
        assert!(resp.is_stream());
        assert_eq!(hub.snapshot().len(), 1);

        let heartbeat = drain_event(&resp);
        assert_eq!(heartbeat, ":\n\n");

        let id = hub.snapshot()[0].id;
        hub.try_close(id)
            .expect("heartbeat test connection must close");
        let _ = runtime.close();
    }

    /// 测试生产侧 `SseMiddleware` 在 `on_open` 失败时清理已注册 sender。
    ///
    /// 该专项分支保护外部回调错误路径：中间件不能返回半初始化 stream，也不能在 Hub 中留下
    /// 已经注册但业务认为失败的连接。
    #[test]
    fn sse_middleware_on_open_error_unregisters_sender() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let middleware = SseMiddleware::with_acceptor(hub.clone(), |_accept| {
            Ok(SseAcceptDecision::accept("user-a".to_string()))
        })
        .config(test_middleware_config())
        .on_open(|_open| Err(SseError::InvalidEvent("open rejected".to_string())))
        .build()
        .expect("on-open-failing SSE middleware must build");
        let mut context = GatewayContext::new();
        let req = test_get_request(HeaderMap::new());

        let resp = expect_break(block_on(middleware.request(&mut context, req)));
        assert!(!resp.is_stream());
        assert!(hub.snapshot().is_empty());
        let text = response_text(resp).to_ascii_lowercase();
        assert!(text.contains("400"));
        assert!(text.contains("open rejected"));
    }

    /// 测试生产侧 `SseHub` 注册、按 key 查询、按 ID 查询和 unregister 只摘除不关闭。
    #[test]
    fn sse_hub_register_get_unregister() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let fixture = test_sender(4);
        let sender = &fixture.sender;
        let id = hub.register("user-a".to_string(), sender.clone()).unwrap();

        assert_eq!(hub.get(&"user-a".to_string()).len(), 1);
        assert!(hub.get_by_id(id).is_some());
        let unregistered = hub.unregister(id).unwrap();
        assert_eq!(unregistered.id(), id);
        assert!(hub.get_by_id(id).is_none());
        assert!(!unregistered.is_closed());
    }

    /// 测试生产侧 `SseHub::register` 拒绝重复 sender，且失败注册不会污染 key 索引。
    #[test]
    fn sse_hub_register_rejects_duplicate_sender_without_key_leak() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let fixture = test_sender(4);
        let sender = &fixture.sender;
        let id = hub.register("user-a".to_string(), sender.clone()).unwrap();

        assert!(matches!(
            hub.register("user-b".to_string(), sender.clone()),
            Err(SseError::ConnectionIdExhausted)
        ));
        assert_eq!(hub.get(&"user-a".to_string()).len(), 1);
        assert_eq!(hub.get(&"user-b".to_string()).len(), 0);
        assert_eq!(hub.unregister(id).unwrap().id(), id);
    }

    /// 测试生产侧 `SseHub::try_send_to_id` 的同步非阻塞成功、队列满和 not found 报告。
    #[test]
    fn sse_hub_try_send_to_id_reports_result() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let fixture = test_sender(1);
        let sender = &fixture.sender;
        let id = hub.register("user-a".to_string(), sender.clone()).unwrap();

        let report = hub.try_send_to_id(id, SseEvent::data("first"));
        assert_eq!(report.total, 1);
        assert_eq!(report.sent, 1);

        let report = hub.try_send_to_id(id, SseEvent::data("second"));
        assert_eq!(report.queue_full, 1);

        let report = hub.try_send_to_id(SseConnectionId(u32::MAX), SseEvent::data("missing"));
        assert_eq!(report.not_found, 1);
    }

    /// 测试生产侧 `SseHub::close` 摘除并关闭连接，`unregister` 与关闭副作用保持分离。
    #[test]
    fn sse_hub_close_removes_and_finishes() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let fixture = test_sender(4);
        let sender = &fixture.sender;
        let id = hub.register("user-a".to_string(), sender.clone()).unwrap();

        block_on(hub.close(id)).unwrap();
        assert!(hub.get_by_id(id).is_none());
        assert!(sender.is_closed());
    }

    /// 测试生产侧 `SseHub::try_close` 在队列满时不提前摘除，释放队列容量后可重试关闭。
    #[test]
    fn sse_hub_try_close_keeps_registration_when_queue_full() {
        let hub = SseHub::<String, TestSocket>::builder().build();
        let fixture = test_sender(1);
        let sender = &fixture.sender;
        let id = hub.register("user-a".to_string(), sender.clone()).unwrap();

        sender.try_send_data("first").unwrap();
        assert!(matches!(hub.try_close(id), Err(SseError::QueueFull)));
        assert!(hub.get_by_id(id).is_some());
        assert!(matches!(
            sender.try_send_data("after-close-started"),
            Err(SseError::Closed)
        ));

        let body = fixture._resp.as_body().unwrap();
        match block_on(body.next()) {
            HttpRecvResult::Ok(Some((_index, chunk))) => {
                assert!(String::from_utf8(chunk).unwrap().contains("data: first"));
            }
            _ => panic!("test response body must yield the queued SSE event"),
        }

        hub.try_close(id).unwrap();
        assert!(hub.get_by_id(id).is_none());
        assert!(sender.is_closed());
    }
}
