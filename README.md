# pi_net

`pi_net` 是一组基于 Rust 的网络协议库。工作区下 `crates/*` 中的每个 crate 都按独立库发布和验证；日常构建、测试和审计应聚焦到具体 crate，例如 `pi_http` 使用 `cargo check -p pi_http`，不要默认构建整个 workspace。

## pi_http SSE 使用指南

`pi_http` 提供 HTTP/1.1 Server-Sent Events 支持，入口为 `pi_http::sse`。SSE 由客户端主动发起 HTTP GET 请求，服务端返回 `text/event-stream` 后，可以持续向该客户端推送文本事件。

SSE 不依赖 HTTP/2。浏览器 `EventSource`、移动端 HTTP 客户端、服务端 HTTP 客户端都可以通过 HTTP/1.1 使用它。

适合使用 SSE 的场景：

- 服务端向浏览器推送通知、任务进度、状态变更、轻量实时数据。
- 客户端只需要接收服务端事件，不需要在同一条连接上双向通信。
- 事件是 UTF-8 文本，单条事件较小，可以接受浏览器自动重连。

不适合使用 SSE 的场景：

- 大文件下载、视频流、二进制高吞吐流。
- 强实时双向通信；这类需求应使用 WebSocket。
- 需要库自动保存历史事件、离线消息或断线补发；这些由业务层处理。

### 前端连接

浏览器端使用标准 `EventSource`：

```js
const source = new EventSource("/sse?token=demo-token");

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

### 推荐服务端接入方式

推荐使用 `SseMiddleware` 挂到某个 GET 路由上。外部通过 acceptor 回调获知“当前 HTTP 请求想打开 SSE”，并在回调中决定允许或拒绝。

允许后，中间件会把连接注册到你提供的 `SseHub`，随后调用 `on_open`。业务层应在 `on_open` 中保存 `SseSender`，或保存 `SseHub` clone 后按 key / ID 推送。

```rust
use std::sync::{Arc, Mutex};

use https::StatusCode;
use pi_http::{
    gateway::GatewayContext,
    route::HttpRoute,
    sse::{SseAcceptDecision, SseConfig, SseEvent, SseHub, SseMiddleware, SseSender},
    virtual_host::{VirtualHost, VirtualHostPool, VirtualHostTab},
};
use tcp::connect::TcpSocket;

#[derive(Clone)]
struct UserSseState {
    hub: SseHub<String, TcpSocket>,
    opened: Arc<Mutex<Vec<SseSender<TcpSocket>>>>,
}

fn build_sse_route(
    state: UserSseState,
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
        open.sender.try_send(
            SseEvent::builder()
                .id(format!("connected-{}", open.id.get()))
                .event("notice")
                .data("connected")
                .build()?,
        )?;

        opened.lock().unwrap().push(open.sender.clone());
        Ok(())
    })
    .build()
    .expect("valid SSE middleware");

    let mut route = HttpRoute::new();
    route.at("/sse").get(middleware);
    route
}
```

上面的代码只展示 SSE 路由。服务端监听、`HttpListenerFactory`、`VirtualHost`、`PortsAdapterFactory` 的组装方式与普通 `pi_http` 服务一致。

### 打开连接的业务时机

外部获知 SSE 打开请求的唯一推荐时机是 `SseMiddleware::with_acceptor` 或 Builder 的 `acceptor` 回调。

acceptor 可以读取：

- 当前 `HttpRequest<S>`。
- 当前 `GatewayContext`。
- 当前待打开连接的 `SseSender<S>` clone。
- 请求 URL、query、header、远端地址、`Last-Event-ID`。

acceptor 应返回：

- `SseAcceptDecision::accept(key)`：允许打开，并用 `key` 注册到 Hub。
- `SseAcceptDecision::reject(status, message)`：拒绝打开，返回普通 HTTP 响应。
- `Err(error)`：按 SSE 错误映射为普通 HTTP 错误响应。

推荐只在 acceptor 中做快速判断，不要执行阻塞 I/O。需要数据库、RPC、复杂鉴权时，最好在进入 SSE 路由前完成，或使用请求中已经准备好的鉴权结果。

### 持有 sender

业务层如果允许某个客户端打开 SSE，需要持续持有 `SseSender<S>` 或能找到它的 `SseHub<K, S>`。

推荐在 `on_open` 中持有 sender：

```rust
let saved = opened.clone();

let middleware = SseMiddleware::with_acceptor(hub.clone(), |_accept| {
    Ok(SseAcceptDecision::accept("user-42".to_string()))
})
.on_open(move |open| {
    saved.lock().unwrap().push(open.sender.clone());
    Ok(())
})
.build()?;
```

不要只保存普通 HTTP 连接信息。SSE 发送必须通过 `SseSender` 或 `SseHub` 完成。

允许多个不同 HTTP 连接同时打开各自的 SSE。例如同一个用户打开多个浏览器标签页时，可以让这些连接使用同一个业务 key：

```rust
let report = hub.try_send_to(
    &"user-42".to_string(),
    SseEvent::named("notice", "hello all tabs"),
);

if report.sent < report.total {
    eprintln!("some SSE connections did not accept the event: {report:?}");
}
```

### 自动心跳

如果使用 `SseMiddleware` 并希望自动发送 heartbeat comment，需要给 Builder 配置 `heartbeat_runtime`。`heartbeat_interval_ms` 大于 0 且配置了运行时时才会自动心跳。

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

### 发送事件

直接向单条连接发送：

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

从同步线程或非异步环境发送，使用 `try_*`：

```rust
match sender.try_send_data("sync message") {
    Ok(()) => {}
    Err(error) => {
        eprintln!("SSE send failed: {error}");
    }
}
```

通过业务 key 发送：

```rust
let report = hub.try_send_to(
    &"user-42".to_string(),
    SseEvent::named("notice", "hello user"),
);

if report.queue_full > 0 || report.busy > 0 {
    // 可选择稍后重试、丢弃低优先级事件，或关闭慢连接。
}
```

通过透明连接 ID 发送：

```rust
let id = sender.id();
let report = hub.try_send_to_id(id, SseEvent::data("only this connection"));

if report.not_found > 0 {
    // 该 SSE 连接已经不在 Hub 中，可能已关闭或被 unregister。
}
```

广播到当前所有 SSE 连接：

```rust
let report = hub.try_broadcast(SseEvent::named("system", "maintenance soon"));
println!("sent={}, total={}", report.sent, report.total);
```

### 发送顺序

顺序语义以“成功入队”为准：

- 同一个 `SseSender` 上，同一线程内连续成功调用会按用户调用顺序输出。
- 多线程或多异步任务并发发送时，按实际成功入队顺序输出。
- `try_*` 返回 `Busy`、`QueueFull` 或其它错误时，本次调用没有入队，不参与输出顺序。
- async API 的 future 只有被 poll 后才会进入发送流程；只创建 future 不会产生发送顺序。

### 关闭和清理

`SseSender` drop 不会自动关闭网络连接。需要关闭时显式调用：

```rust
sender.finish().await?;
```

通过 Hub 关闭：

```rust
hub.close(id).await?;
```

同步非阻塞关闭：

```rust
match hub.try_close(id) {
    Ok(()) => {}
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
if let Some(sender) = hub.unregister(id) {
    let _ = sender.try_finish();
}
```

清理已经关闭的连接：

```rust
let removed = hub.remove_closed();
println!("removed closed SSE connections: {removed}");
```

### 查看活动连接

```rust
for info in hub.snapshot() {
    println!(
        "sse id={}, remote={}, closed={}",
        info.id.get(),
        info.remote_addr,
        info.closed
    );
}
```

`snapshot()` 返回只读快照。快照返回后，连接可能马上关闭或被业务摘除；发送前仍需要处理 `SseSendReport`。

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

让这些连接在 acceptor 中返回同一个业务 key，然后使用 `hub.send_to` 或 `hub.try_send_to`。

如何只推给某一个页面？

保存 `SseOpen::id` 或 `SseSender::id()`，使用 `send_to_id` / `try_send_to_id`。

如何处理浏览器断线重连？

事件设置 `id`，浏览器重连时会带上 `Last-Event-ID`。业务层可通过 `sender.last_event_id()` 读取并自行补发。

TCP 和 TLS 怎么区分？

按实际 socket 类型创建对应的 `SseHub<K, S>` 和 `SseMiddleware<K, S>`。例如 TCP 使用 `SseHub<String, TcpSocket>`；TLS 使用自己的 TLS socket 类型。

### 使用禁忌

- 不要在 acceptor 或 `on_open` 中执行阻塞 I/O、长时间 sleep 或复杂计算。
- 不要把大文件、二进制流或高频大块数据放进 SSE。
- 不要把 `try_*` 成功当作客户端确认。
- 不要手动给 SSE 响应设置 `Content-Length` 或启用压缩。
- 不要把普通 HTTP keep-alive 连接当作 SSE 连接。
- 不要依赖 drop 自动关闭连接。
- 不要重复注册同一个 `SseSender`。
- 不要在没有持有 sender 或 Hub 的情况下期望后续还能推送。

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
