# pi_net

`pi_net` 是一组基于 Rust 的网络协议库。工作区下 `crates/*` 中的每个 crate 都按独立库发布和验证；日常构建、测试和审计应聚焦到具体 crate，例如 `pi_http` 使用 `cargo check -p pi_http`，不要默认构建整个 workspace。

## pi_http SSE 使用指南

`pi_http` 通过 `pi_http::sse` 提供 HTTP/1.1 Server-Sent Events 支持。客户端发起 GET 请求，服务端返回 `Content-Type: text/event-stream` 后，可以在这条响应流上持续向客户端推送 UTF-8 文本事件。

SSE 不依赖 HTTP/2。浏览器 `EventSource`、移动端 HTTP 客户端和服务端 HTTP 客户端都可以基于 HTTP/1.1 使用。

适合使用 SSE 的场景：

- 服务端向浏览器推送通知、任务进度、状态变更、轻量实时数据。
- 客户端只需要接收服务端事件，不需要在同一条连接上双向通信。
- 事件是 UTF-8 文本，单条事件较小，可以接受浏览器自动重连。
- 外部业务希望用自己的 session、用户 ID 或连接 ID 管理推送目标。

不适合使用 SSE 的场景：

- 大文件下载、视频流、二进制高吞吐流。
- 强实时双向通信；这类需求应使用 WebSocket。
- 需要库自动保存历史事件、离线消息或断线补发；这些应由业务层处理。

### 浏览器连接

浏览器端使用标准 `EventSource`：

```js
const source = new EventSource("/sse?token=demo-token&session=web-42");

source.addEventListener("notice", (event) => {
  console.log("notice", event.lastEventId, event.data);
});

source.onmessage = (event) => {
  console.log("message", event.data);
};

source.onerror = () => {
  // 浏览器会按 SSE 规则自动重连。
};
```

浏览器重连时会把最近收到的 `id:` 作为 `Last-Event-ID` 请求头发回服务端。`pi_http` 会把该值暴露给 `SseSender::last_event_id()`；是否补发历史事件由业务自行决定。

### 两种接入方式

`SseMiddleware` 有两种常用接入方式：

- 直接接入：SSE 中间件就是当前 SSE 路由的处理器。外部在 `acceptor` 中获知打开请求并决定允许或拒绝。
- port 中间件接入：路由使用 `MiddlewareChain`，把 `SseMiddleware` 放在 `HttpPort` 前面。后续 `HttpPort` handler 读取 SSE 候选信息，执行业务鉴权、session 绑定和初始化，然后决定允许或拒绝。

直接接入适合只依赖请求本身做快速判断的场景。port 中间件接入适合外部业务已经依赖 `HttpPort`，需要把 SSE 建连请求交给上层 handler 处理的场景。

### 直接接入示例

```rust
use std::sync::{Arc, Mutex};

use https::StatusCode;
use pi_http::{
    gateway::GatewayContext,
    route::HttpRoute,
    sse::{SseAcceptDecision, SseConfig, SseEvent, SseHub, SseMiddleware, SseSender},
};
use tcp::connect::TcpSocket;

#[derive(Clone)]
struct DirectSseState {
    hub: SseHub<String, TcpSocket>,
    opened: Arc<Mutex<Vec<SseSender<TcpSocket>>>>,
}

fn build_direct_sse_route(
    state: DirectSseState,
) -> HttpRoute<TcpSocket, GatewayContext, SseMiddleware<String, TcpSocket>> {
    let config = SseConfig::builder()
        .channel_size(32)
        .max_event_bytes(64 * 1024)
        .heartbeat_interval_ms(15_000)
        .send_initial_comment(true)
        .build()
        .expect("valid SSE config");

    let opened = state.opened.clone();
    let middleware = SseMiddleware::with_acceptor(state.hub.clone(), move |accept| {
        let token = accept
            .request
            .url()
            .query_pairs()
            .find(|(name, _)| name == "token")
            .map(|(_, value)| value.to_string());

        match token.as_deref() {
            Some("demo-token") => Ok(SseAcceptDecision::accept("user-42".to_string())),
            _ => Ok(SseAcceptDecision::reject(
                StatusCode::FORBIDDEN,
                "SSE is not allowed for this request",
            )),
        }
    })
    .config(config)
    .require_accept_header(true)
    .on_open(move |open| {
        opened.lock().unwrap().push(open.sender.clone());

        open.sender.try_send(
            SseEvent::builder()
                .id(format!("connected-{}", open.id.get()))
                .event("notice")
                .data("connected")
                .build()?,
        )?;

        Ok(())
    })
    .build()
    .expect("valid SSE middleware");

    let mut route = HttpRoute::new();
    route.at("/sse").get(middleware);
    route
}
```

直接接入时，外部获知 SSE 打开请求的时机是 `acceptor` 回调；真正需要缓存 sender、发送初始化事件或建立进程内映射时，推荐在 `on_open` 中处理。

### port 中间件接入示例

下面示例展示一个常见生产接入方式：

- `DefaultParser` 先把 query 参数解析到 `GatewayContext.params`。
- `SseMiddleware` 标记当前请求是 SSE 候选请求。
- `HttpPort` handler 读取候选连接 ID、nonce、业务 token/session。
- handler 决定允许或拒绝 SSE。
- `on_open` 拿到最终 sender，建立 `session -> connection id`、`connection id -> sender` 映射，并发送初始化事件。

```rust
use std::{
    cell::RefCell,
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use futures::future::{FutureExt, LocalBoxFuture};
use https::{HeaderMap, StatusCode};
use pi_atom::Atom;
use pi_gray::GrayVersion;
use pi_handler::{Args, Handler, SGenType};
use pi_hash::XHashMap;
use pi_http::{
    default_parser::DefaultParser,
    gateway::GatewayContext,
    middleware::MiddlewareChain,
    port::HttpPort,
    response::ResponseHandler,
    route::HttpRoute,
    sse::{
        write_sse_accept_headers, write_sse_reject_headers, SseConfig, SseConnectionId,
        SseEvent, SseHub, SseMiddleware, SseSender, SSE_PARAM_CONNECTION_ID,
        SSE_PARAM_LAST_EVENT_ID, SSE_PARAM_NONCE,
    },
};
use tcp::connect::TcpSocket;

#[derive(Clone)]
struct SessionSseRegistry {
    hub: SseHub<String, TcpSocket>,
    by_session: Arc<Mutex<HashMap<String, Vec<SseConnectionId>>>>,
    by_id: Arc<Mutex<HashMap<SseConnectionId, SseSender<TcpSocket>>>>,
}

impl SessionSseRegistry {
    fn new() -> Self {
        Self {
            hub: SseHub::<String, TcpSocket>::builder().build(),
            by_session: Arc::new(Mutex::new(HashMap::new())),
            by_id: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn register(&self, session: String, id: SseConnectionId, sender: SseSender<TcpSocket>) {
        self.by_session
            .lock()
            .unwrap()
            .entry(session)
            .or_default()
            .push(id);
        self.by_id.lock().unwrap().insert(id, sender);
    }

    fn sender(&self, id: SseConnectionId) -> Option<SseSender<TcpSocket>> {
        self.by_id.lock().unwrap().get(&id).cloned()
    }

    fn remove(&self, id: SseConnectionId) {
        self.by_id.lock().unwrap().remove(&id);
        for ids in self.by_session.lock().unwrap().values_mut() {
            ids.retain(|item| *item != id);
        }
    }
}

struct SseOpenPortHandler {
    registry: SessionSseRegistry,
}

impl Handler for SseOpenPortHandler {
    type A = SocketAddr;
    type B = String;
    type C = Arc<HeaderMap>;
    type D = Arc<RefCell<XHashMap<String, SGenType>>>;
    type E = ResponseHandler;
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(
        &self,
        _env: Arc<dyn GrayVersion>,
        _topic: Atom,
        args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>,
    ) -> LocalBoxFuture<'static, Self::HandleResult> {
        let _registry = self.registry.clone();

        async move {
            let Args::FiveArgs(_addr, _method, _headers, params, response) = args else {
                return;
            };

            let (connection_id, nonce, token, session, last_event_id) = {
                let params = params.borrow();

                let connection_id = match params.get(SSE_PARAM_CONNECTION_ID) {
                    Some(SGenType::Str(value)) => value.clone(),
                    _ => {
                        response.status(StatusCode::BAD_REQUEST.as_u16());
                        let _ = response.finish().await;
                        return;
                    }
                };

                let nonce = match params.get(SSE_PARAM_NONCE) {
                    Some(SGenType::Str(value)) => value.clone(),
                    _ => {
                        response.status(StatusCode::BAD_REQUEST.as_u16());
                        let _ = response.finish().await;
                        return;
                    }
                };

                let token = match params.get("token") {
                    Some(SGenType::Str(value)) => value.clone(),
                    _ => String::new(),
                };

                let session = match params.get("session") {
                    Some(SGenType::Str(value)) if !value.is_empty() => value.clone(),
                    _ => "anonymous-session".to_string(),
                };

                let last_event_id = match params.get(SSE_PARAM_LAST_EVENT_ID) {
                    Some(SGenType::Str(value)) => Some(value.clone()),
                    _ => None,
                };

                (connection_id, nonce, token, session, last_event_id)
            };

            if token != "demo-token" {
                let _ = write_sse_reject_headers(
                    &response,
                    connection_id.as_str(),
                    &nonce,
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                );
                let _ = response.finish().await;
                return;
            }

            // 这里可以执行业务侧 session 绑定、权限检查、Last-Event-ID 记录和初始化准备。
            if let Some(last_event_id) = last_event_id {
                eprintln!("SSE reconnect from last event id: {last_event_id}");
            }

            if write_sse_accept_headers(&response, connection_id.as_str(), &nonce, &session)
                .is_err()
            {
                response.status(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
            }

            let _ = response.finish().await;
        }
        .boxed_local()
    }
}

fn build_port_sse_route(
    registry: SessionSseRegistry,
) -> HttpRoute<TcpSocket, GatewayContext, Arc<MiddlewareChain<TcpSocket, GatewayContext>>> {
    let hub = registry.hub.clone();
    let registry_for_open = registry.clone();

    let sse = SseMiddleware::with_acceptor(hub.clone(), |_accept| {
        unreachable!("port_handshake_string_key mode does not use direct acceptor")
    })
    .config(
        SseConfig::builder()
            .channel_size(64)
            .max_event_bytes(64 * 1024)
            .heartbeat_interval_ms(0)
            .send_initial_comment(true)
            .build()
            .expect("valid SSE config"),
    )
    .require_accept_header(true)
    .port_handshake_string_key()
    .on_open(move |open| {
        registry_for_open.register(open.key.clone(), open.id, open.sender.clone());

        open.sender.try_send(
            SseEvent::builder()
                .id(format!("open-{}", open.id.get()))
                .event("notice")
                .data("session connected")
                .build()?,
        )?;

        Ok(())
    })
    .build()
    .expect("valid SSE middleware");

    let port = HttpPort::with_handler(
        None,
        Arc::new(SseOpenPortHandler {
            registry: registry.clone(),
        }),
    );

    let mut chain = MiddlewareChain::<TcpSocket, GatewayContext>::new();
    chain.push_back(Arc::new(DefaultParser::with(1024, None, None)));
    chain.push_back(Arc::new(sse));
    chain.push_back(Arc::new(port));
    chain.finish();

    let mut route = HttpRoute::new();
    route.at("/sse").get(Arc::new(chain));
    route
}
```

port 中间件接入时，业务判断应放在 `HttpPort` handler 或其后续逻辑中。`on_open` 仍然很重要：它是拿到 `SseSender` 安全句柄的时机，适合注册 sender、缓存连接 ID、发送初始化事件或启动业务侧推送。

允许多个不同 HTTP 连接同时打开各自的 SSE。可以让同一用户的多个页面使用同一个 session key，也可以按设备、页面或业务连接 ID 分配不同 key。

### sender、Hub 和活动连接

业务层如果允许某个客户端打开 SSE，需要持续持有 `SseSender<S>`，或持有能找到 sender 的 `SseHub<K, S>`。

按业务 key 给同一个 session 下所有 SSE 连接发送：

```rust
let report = registry.hub.try_send_to(
    &"web-42".to_string(),
    SseEvent::named("notice", "hello all pages"),
);

if report.queue_full > 0 || report.busy > 0 {
    // 可以稍后重试、丢弃低优先级事件，或关闭慢连接。
}
```

按透明连接 ID 只发送给某一条 SSE 连接：

```rust
let id: SseConnectionId = /* 从 on_open 或 registry 中取得 */;

let report = registry
    .hub
    .try_send_to_id(id, SseEvent::data("only this connection"));

if report.not_found > 0 {
    registry.remove(id);
}
```

从非异步环境发送，优先使用 `try_*`：

```rust
if let Some(sender) = registry.sender(id) {
    match sender.try_send_data("message from sync code") {
        Ok(()) => {}
        Err(error) => eprintln!("SSE send failed: {error}"),
    }
}
```

从异步环境发送，可以使用 async API：

```rust
sender.send_data("hello").await?;

sender
    .send(
        SseEvent::builder()
            .id("evt-100")
            .event("notice")
            .data("payload")
            .retry(3000)
            .build()?,
    )
    .await?;
```

查看当前活动连接快照：

```rust
for info in registry.hub.snapshot() {
    println!(
        "sse key={}, id={}, remote={}, closed={}",
        info.key,
        info.id.get(),
        info.remote_addr,
        info.closed
    );
}
```

`snapshot()` 返回只读快照。快照返回后，连接可能马上关闭或被业务摘除；发送前仍需要处理 `SseSendReport`。

### 发送顺序

顺序语义以“成功入队”为准：

- 同一个 `SseSender` 上，同一线程内连续成功调用会按用户调用顺序输出。
- 多线程或多异步任务并发发送时，按实际成功入队顺序输出。
- `try_*` 返回 `Busy`、`QueueFull` 或其它错误时，本次调用没有入队，不参与输出顺序。
- async API 的 future 只有被 poll 后才会进入发送流程；只创建 future 不会产生发送顺序。

### 心跳

如果希望中间件自动发送 heartbeat comment，需要配置 `heartbeat_runtime`。`heartbeat_interval_ms` 大于 0 且配置了运行时时才会自动心跳。

```rust
let heartbeat_rt = pi_async_rt::rt::AsyncRuntimeBuilder::default_multi_thread(
    Some("sse-heartbeat"),
    None,
    Some(1),
    Some(1),
);

let middleware = SseMiddleware::with_acceptor(hub.clone(), |_accept| {
    Ok(SseAcceptDecision::accept("user-42".to_string()))
})
.config(
    SseConfig::builder()
        .heartbeat_interval_ms(15_000)
        .send_initial_comment(true)
        .build()?,
)
.heartbeat_runtime(heartbeat_rt)
.build()?;
```

如果不想自动心跳，可以把 `heartbeat_interval_ms(0)`，并由业务定时调用：

```rust
let _ = sender.try_heartbeat();
```

### 关闭和清理

`SseSender` drop 不会自动关闭网络连接。需要关闭时显式调用：

```rust
sender.finish().await?;
```

通过 Hub 关闭：

```rust
registry.hub.close(id).await?;
registry.remove(id);
```

同步非阻塞关闭：

```rust
match registry.hub.try_close(id) {
    Ok(()) => {
        registry.remove(id);
    }
    Err(pi_http::sse::SseError::QueueFull) => {
        // 结束标记暂时无法入队；连接仍保留在 Hub 中，可以稍后重试。
    }
    Err(error) => {
        eprintln!("close failed: {error}");
    }
}
```

只想从 Hub 摘除但不关闭连接：

```rust
if let Some(sender) = registry.hub.unregister(id) {
    registry.remove(id);
    let _ = sender.try_finish();
}
```

清理已经关闭的连接：

```rust
let removed = registry.hub.remove_closed();
println!("removed closed SSE connections: {removed}");
```

### 配置建议

```rust
let config = SseConfig::builder()
    .channel_size(32)
    .max_event_bytes(64 * 1024)
    .heartbeat_interval_ms(15_000)
    .send_initial_comment(true)
    .no_cache(true)
    .disable_proxy_buffering(true)
    .build()?;
```

建议：

- `channel_size` 不要当成无限缓冲。慢客户端应由业务层限流、丢弃低优先级事件或关闭。
- `max_event_bytes` 保持较小。SSE 适合小型文本事件，不适合大文件。
- `heartbeat_interval_ms` 常用值为 15 到 30 秒；过低会增加连接和任务开销。
- `send_initial_comment(true)` 有助于客户端和中间代理尽快确认流已打开。
- `no_cache(true)` 和 `disable_proxy_buffering(true)` 通常应保持开启。
- port 中间件模式下，`SseMiddleware` 应位于 `HttpPort` 前面；如果需要 query 参数，通常把 `DefaultParser` 放在 `SseMiddleware` 前面。

### 低层手动接入

如果不使用默认中间件，也可以在自定义中间件中手动创建响应和 sender：

```rust
let (resp, sender) = pi_http::sse::SseResponse::builder(&req)
    .config(SseConfig::default())
    .build()?;

hub.register("user-42".to_string(), sender.clone())?;
sender.try_send_data("connected")?;

return MiddlewareResult::Finish((req, resp));
```

自定义中间件的响应阶段需要对 `resp.is_stream()` 返回 `Break(resp)`，避免后续普通响应处理影响 SSE。

### 常见问题

`try_send` 成功是否表示浏览器已经收到？

不是。它只表示事件进入 `pi_http` 响应队列。客户端是否已经收到取决于后续 socket 写出、网络和浏览器状态。

如何给同一个用户的多个页面都推送？

让这些连接使用同一个业务 key，然后使用 `hub.send_to` 或 `hub.try_send_to`。

如何只推给某一个页面？

保存 `SseOpen::id` 或 `SseSender::id()`，使用 `send_to_id` / `try_send_to_id`。

如何处理浏览器断线重连？

事件设置 `id`，浏览器重连时会带上 `Last-Event-ID`。业务层可通过 `SseSender::last_event_id()` 或 port handler 中的 `SSE_PARAM_LAST_EVENT_ID` 自行补发。

TCP 和 TLS 怎么区分？

按实际 socket 类型创建对应的 `SseHub<K, S>` 和 `SseMiddleware<K, S>`。例如 TCP 使用 `SseHub<String, TcpSocket>`；TLS 使用自己的 TLS socket 类型。

### 使用禁忌

- 不要在 acceptor、`on_open` 或 port handler 中执行阻塞 I/O、长时间 sleep 或复杂计算。
- 不要把大文件、二进制流或高频大块数据放进 SSE。
- 不要把 `try_*` 成功当作客户端确认。
- 不要手动给 SSE 响应设置 `Content-Length` 或启用压缩。
- 不要把普通 HTTP keep-alive 连接当作 SSE 连接。
- 不要依赖 drop 自动关闭连接。
- 不要重复注册同一个 `SseSender`。
- 不要在没有持有 sender 或 Hub 的情况下期望后续还能推送。
- 不要让客户端直接决定业务 key；port handler 或 acceptor 应先完成鉴权和 session 绑定。

### 验证命令

当前 SSE 相关验证只在 `pi_http` 子库范围内运行：

```bash
/home/vmos/.cargo/bin/cargo check -p pi_http --lib
/home/vmos/.cargo/bin/cargo test -p pi_http --lib
/home/vmos/.cargo/bin/cargo test -p pi_http --test sse_real_network
/home/vmos/.cargo/bin/cargo test -p pi_http --doc
/home/vmos/.cargo/bin/cargo test -p pi_http --tests --no-run
/home/vmos/.cargo/bin/cargo bench -p pi_http --bench sse
```

不要把 `cargo test -p pi_http` 作为标准自动回归命令，因为它会执行 `crates/http/tests/test.rs`。该文件中的测试只适合人工介入观察/示例验证，不属于标准自动测试集。
