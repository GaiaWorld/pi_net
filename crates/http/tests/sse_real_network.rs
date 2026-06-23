//! SSE real-network integration tests for `pi_http`.
//!
//! 本测试文件只覆盖非单元测试侧的真实网络路径：服务端通过 `SocketListener`
//! 绑定本机 TCP 端口，客户端通过 `std::net::TcpStream` 发送真实 HTTP/1.1 GET
//! 请求并读取 chunked `text/event-stream` 响应。测试不使用 fake socket，也不直接
//! 操作内部响应队列。
//!
//! 测试侧验收目标：
//! - 对应生产侧模块/API：`pi_http::sse::{SseMiddleware, SseConfig, SseEvent, SseHub}`。
//! - 对应生产侧路径：`HttpListener -> HttpGateway -> Middleware -> HttpConnect`
//!   的 HTTP/1.1 流响应发送路径。
//! - 业务边界：验证 HTTP/1.1 SSE 建连、外部 accept 决策、真实 TCP 输出、Hub 注册、
//!   透明连接 ID 定向发送和显式关闭；不验证历史事件重放，也不验证 TLS。
//! - 性能边界：测试数据量为常数级，客户端读取循环为 `O(n)` 时间、`O(n)` 空间，
//!   `n` 为响应字节数；服务端只发送一个事件和一个结束标记。
//! - 阻塞边界：客户端 `TcpStream` 使用读写超时，避免失败时永久阻塞；服务端发送线程
//!   使用 `try_*` API，不阻塞异步运行时。
//! - 副作用：测试会临时占用 `127.0.0.1` 的一个空闲端口，并启动/关闭一个真实
//!   `SocketListener`。
//! - 幂等性：测试本身不是幂等的外部资源操作，但端口和 listener 会在测试结束时释放。
//! - 线程/异步安全：`SseHub` 和 `SseSender` 从中间件异步路径 clone 后跨线程使用，
//!   验证其 `Send + Sync` 设计约束在真实路径下成立。

use std::io::{Error, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use https::StatusCode;
use pi_async_rt::rt::{serial::AsyncRuntimeBuilder, startup_global_time_loop};
use tcp::{
    connect::TcpSocket,
    server::{PortsAdapterFactory, SocketListener},
    SocketConfig,
};

use pi_http::{
    gateway::GatewayContext,
    route::HttpRoute,
    server::HttpListenerFactory,
    sse::{SseAcceptDecision, SseConfig, SseEvent, SseHub, SseMiddleware},
    virtual_host::{VirtualHost, VirtualHostPool, VirtualHostTab},
};

/// 真实网络 SSE 测试场景。
///
/// 对应生产侧顺序语义：
/// - `SingleEvent` 验证基础建连、单事件和关闭。
/// - `Reject` 验证外部 acceptor 能在 stream 返回前拒绝打开 SSE。
/// - `SameThreadOrder` 验证同一线程内连续成功调用按调用顺序进入真实网络输出。
/// - `CrossThreadControlledOrder` 验证跨线程发送时，按受控成功入队顺序进入真实网络输出。
/// - `Heartbeat` 验证默认中间件配置运行时后能通过真实 TCP 输出 heartbeat comment。
#[derive(Clone, Copy)]
enum SseNetworkScenario {
    SingleEvent,
    Reject,
    SameThreadOrder,
    CrossThreadControlledOrder,
    Heartbeat,
}

/// 在真实网络测试服务端线程中发送一条具名 SSE 事件。
///
/// 本 helper 只封装测试侧重复逻辑；被测生产 API 是
/// `SseHub::try_send_to_id`、`SseEventBuilder` 和后续 `HttpConnect` chunked 输出。
/// 成功只表示事件已进入生产侧响应队列，最终网络输出由客户端断言验证。
fn send_ordered_event(
    hub: &SseHub<String, TcpSocket>,
    id: pi_http::sse::SseConnectionId,
    event_id: &str,
    event_name: &str,
    data: &str,
) {
    let event = SseEvent::builder()
        .id(event_id)
        .event(event_name)
        .data(data)
        .build()
        .expect("SSE ordered test event must be valid");
    let report = hub.try_send_to_id(id, event);
    assert_eq!(report.total, 1);
    assert_eq!(report.sent, 1);
}

/// 从本机回环地址申请一个当前空闲端口。
///
/// 这是 `SocketListener` 缺少“绑定 0 后返回实际端口”能力下的测试侧折中：
/// 标准库 listener 释放端口后再由 `pi_tcp` 绑定。该函数本身执行真实网络 bind，
/// 时间/空间复杂度为 `O(1)`。极端并发测试环境下存在端口被其它进程抢占的非确定性，
/// 失败时由后续 `SocketListener::try_bind` 返回错误。
fn reserve_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("test must reserve a local TCP port")
        .local_addr()
        .expect("test listener must expose local addr")
        .port()
}

/// 启动只服务 `/sse` 的真实 `pi_http` 服务端。
///
/// 返回 listener 和地址。调用方必须在测试结束时调用 `close`，否则运行时线程会继续存在。
/// 该函数启动真实 TCP listener，有端口占用副作用；时间复杂度与 listener 初始化成本相关，
/// 测试规模下按 `O(1)` 处理。
fn start_sse_server(
    scenario: SseNetworkScenario,
) -> (
    SocketListener<TcpSocket, PortsAdapterFactory<TcpSocket>>,
    SocketAddr,
) {
    let port = reserve_local_port();
    let addr: SocketAddr = format!("127.0.0.1:{}", port)
        .parse()
        .expect("test addr must parse");

    let hub = SseHub::<String, TcpSocket>::builder().build();
    let accept_scenario = scenario;
    let hub_for_open = hub.clone();
    let heartbeat_interval = if matches!(scenario, SseNetworkScenario::Heartbeat) {
        10
    } else {
        0
    };
    let middleware_builder = SseMiddleware::with_acceptor(hub.clone(), move |accept| {
        assert_eq!(accept.request.url().path(), "/sse");
        if matches!(accept_scenario, SseNetworkScenario::Reject) {
            Ok(SseAcceptDecision::reject(
                StatusCode::FORBIDDEN,
                "sse rejected by acceptor",
            ))
        } else {
            Ok(SseAcceptDecision::accept("client-a".to_string()))
        }
    })
    .config(
        SseConfig::builder()
            .channel_size(8)
            .heartbeat_interval_ms(heartbeat_interval)
            .send_initial_comment(false)
            .build()
            .expect("SSE test config must be valid"),
    )
    .on_open(move |open| {
        assert_eq!(open.key, "client-a");
        let hub_for_thread = hub_for_open.clone();
        let id = open.id;
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            match scenario {
                SseNetworkScenario::SingleEvent => {
                    send_ordered_event(
                        &hub_for_thread,
                        id,
                        "evt-1",
                        "notice",
                        "hello from pi_http",
                    );
                    hub_for_thread
                        .try_close(id)
                        .expect("SSE test connection must close explicitly");
                }
                SseNetworkScenario::SameThreadOrder => {
                    send_ordered_event(&hub_for_thread, id, "evt-1", "notice", "first");
                    send_ordered_event(&hub_for_thread, id, "evt-2", "notice", "second");
                    hub_for_thread
                        .try_close(id)
                        .expect("SSE ordered test connection must close explicitly");
                }
                SseNetworkScenario::CrossThreadControlledOrder => {
                    let (first_done_sender, first_done_receiver) = mpsc::channel();
                    let first_hub = hub_for_thread.clone();
                    let second_hub = hub_for_thread.clone();
                    let first = thread::spawn(move || {
                        send_ordered_event(&first_hub, id, "evt-1", "notice", "first");
                        first_done_sender
                            .send(())
                            .expect("first sender must notify second sender");
                    });
                    let second = thread::spawn(move || {
                        first_done_receiver
                            .recv()
                            .expect("second sender must wait for first enqueue");
                        send_ordered_event(&second_hub, id, "evt-2", "notice", "second");
                        second_hub.try_close(id).expect(
                            "SSE cross-thread ordered test connection must close explicitly",
                        );
                    });

                    first.join().expect("first SSE sender thread must finish");
                    second.join().expect("second SSE sender thread must finish");
                }
                SseNetworkScenario::Heartbeat => {
                    thread::sleep(Duration::from_millis(80));
                    hub_for_thread
                        .try_close(id)
                        .expect("SSE heartbeat test connection must close explicitly");
                }
                SseNetworkScenario::Reject => {}
            }
        });
        Ok(())
    });
    let middleware_builder = if matches!(scenario, SseNetworkScenario::Heartbeat) {
        middleware_builder.heartbeat_runtime(
            pi_async_rt::rt::AsyncRuntimeBuilder::default_multi_thread(
                Some("sse-real-heartbeat"),
                None,
                Some(1),
                Some(1),
            ),
        )
    } else {
        middleware_builder
    };
    let middleware = middleware_builder
        .build()
        .expect("SSE real-network middleware must build");

    let mut route = HttpRoute::<TcpSocket, GatewayContext, SseMiddleware<String, TcpSocket>>::new();
    route.at("/sse").get(middleware);
    let host = VirtualHost::with(route);
    let mut hosts = VirtualHostTab::<TcpSocket, SseMiddleware<String, TcpSocket>>::new();
    hosts
        .add_default(host)
        .expect("test virtual host must register");

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(
        port,
        HttpListenerFactory::<TcpSocket, _>::with_hosts(hosts, 5000).new_service(),
    );

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let mut config = SocketConfig::new("127.0.0.1", &[port]);
    config.set_option(16 * 1024, 16 * 1024, 16 * 1024, 16);
    let listener = SocketListener::try_bind(
        vec![rt],
        factory,
        config,
        64,
        1024 * 1024,
        128,
        8,
        16 * 1024,
        16 * 1024,
        Some(10),
    )
    .expect("test SSE server must bind");

    (listener, addr)
}

/// 断言真实 HTTP 响应文本中 `first` 出现在 `second` 之前。
///
/// 该 helper 对应生产侧顺序语义验收：事件顺序必须在最终网络字节流中保持，而不是只在
/// 内部 sender 或 Hub API 返回值中保持。
fn assert_response_text_order(response: &[u8], first: &str, second: &str) {
    let text = String::from_utf8_lossy(response).to_ascii_lowercase();
    let first_index = text
        .find(first)
        .unwrap_or_else(|| panic!("response must contain first marker `{}`: {}", first, text));
    let second_index = text
        .find(second)
        .unwrap_or_else(|| panic!("response must contain second marker `{}`: {}", second, text));

    assert!(
        first_index < second_index,
        "`{}` must appear before `{}` in response: {}",
        first,
        second,
        text
    );
}

/// 通过真实 TCP 客户端读取 HTTP 响应，直到读到 chunked 结束帧。
///
/// 本函数只用于真实网络测试，目标是验证生产侧最终输出的 HTTP 字节。读取循环最多运行
/// 5 秒，时间复杂度 `O(n)`、空间复杂度 `O(n)`；`n` 为读取到的响应字节数。
fn read_sse_response(addr: SocketAddr) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("test client must connect to SSE server");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("test client must set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("test client must set write timeout");
    let req = format!(
        "GET /sse HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        addr
    );
    stream
        .write_all(req.as_bytes())
        .expect("test client must write HTTP request");

    let started = Instant::now();
    let mut response = Vec::new();
    let mut buf = [0u8; 1024];
    while started.elapsed() < Duration::from_secs(5) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if response
                    .windows(b"0\r\n\r\n".len())
                    .any(|w| w == b"0\r\n\r\n")
                {
                    return response;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue;
            }
            Err(e) => panic!("test client read failed: {:?}", e),
        }
    }

    response
}

/// 真实网络验证 `pi_http` SSE 能在 HTTP/1.1 上以 chunked `text/event-stream` 输出事件。
///
/// 对应生产侧功能/API：
/// - `SseResponse::builder`：构建 HTTP/1.1 SSE 响应头与 sender。
/// - `SseHub::register` / `try_send_to_id` / `try_close`：跨线程同步非阻塞发送与关闭。
/// - `HttpConnect::run_service`：流响应头、chunked 数据块和结束帧写出。
///
/// 验收断言：
/// - 响应状态为 `HTTP/1.1 200`。
/// - 响应头包含 `content-type: text/event-stream; charset=utf-8`。
/// - 响应头包含 `transfer-encoding: chunked`。
/// - 响应体包含标准 SSE 字段 `id`、`event`、`data`。
/// - 响应体包含 chunked 结束帧 `0\r\n\r\n`。
#[test]
fn sse_real_network_get_stream_receives_chunked_event() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _timer = startup_global_time_loop(10);
    let (listener, addr) = start_sse_server(SseNetworkScenario::SingleEvent);
    thread::sleep(Duration::from_millis(100));

    let response = read_sse_response(addr);
    listener.close(Err(Error::new(
        ErrorKind::Interrupted,
        "close SSE real-network test listener",
    )));

    let text = String::from_utf8_lossy(&response).to_ascii_lowercase();
    assert!(
        text.contains("http/1.1 200"),
        "response must contain HTTP 200 status, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("content-type:text/event-stream; charset=utf-8"),
        "response must contain SSE content type, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("transfer-encoding:chunked"),
        "response must contain chunked transfer encoding, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("id: evt-1"),
        "response must contain SSE id field, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("event: notice"),
        "response must contain SSE event field, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("data: hello from pi_http"),
        "response must contain SSE data field, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        response
            .windows(b"0\r\n\r\n".len())
            .any(|w| w == b"0\r\n\r\n"),
        "response must contain chunked finish frame, got: {}",
        String::from_utf8_lossy(&response)
    );
}

/// 真实网络验证外部 acceptor 可以在返回 stream 前拒绝当前 HTTP 连接打开 SSE。
///
/// 对应生产侧功能/API：
/// - `SseMiddleware::with_acceptor`：外部在建连时机决定允许或拒绝。
/// - `SseAcceptDecision::Reject`：拒绝后返回普通 HTTP 响应，不注册 SSE stream。
///
/// 验收断言：
/// - 响应状态为 `HTTP/1.1 403`。
/// - 响应体包含拒绝原因。
/// - 响应不是 `text/event-stream`，也没有 chunked 结束帧。
#[test]
fn sse_real_network_acceptor_can_reject_open_request() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _timer = startup_global_time_loop(10);
    let (listener, addr) = start_sse_server(SseNetworkScenario::Reject);
    thread::sleep(Duration::from_millis(100));

    let response = read_sse_response(addr);
    listener.close(Err(Error::new(
        ErrorKind::Interrupted,
        "close SSE reject test listener",
    )));

    let text = String::from_utf8_lossy(&response).to_ascii_lowercase();
    assert!(
        text.contains("http/1.1 403"),
        "response must contain HTTP 403 status, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("sse rejected by acceptor"),
        "response must contain acceptor rejection message, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        !text.contains("content-type:text/event-stream"),
        "reject response must not be SSE stream, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        !response
            .windows(b"0\r\n\r\n".len())
            .any(|w| w == b"0\r\n\r\n"),
        "reject response must not contain chunked finish frame, got: {}",
        String::from_utf8_lossy(&response)
    );
}

/// 真实网络验证默认 SSE 中间件配置心跳运行时后，会输出标准 SSE comment heartbeat。
///
/// 对应生产侧功能/API：
/// - `SseMiddlewareBuilder::heartbeat_runtime`：在连接打开后启动自动 heartbeat 任务。
/// - `SseSender::heartbeat`：把空 comment 写入响应队列。
/// - `HttpConnect::run_service`：把 heartbeat comment 作为 chunked `text/event-stream` 输出。
#[test]
fn sse_real_network_heartbeat_runtime_emits_comment() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _timer = startup_global_time_loop(10);
    let (listener, addr) = start_sse_server(SseNetworkScenario::Heartbeat);
    thread::sleep(Duration::from_millis(100));

    let response = read_sse_response(addr);
    listener.close(Err(Error::new(
        ErrorKind::Interrupted,
        "close SSE heartbeat test listener",
    )));

    let text = String::from_utf8_lossy(&response).to_ascii_lowercase();
    assert!(
        text.contains("http/1.1 200"),
        "heartbeat response must contain HTTP 200 status, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("content-type:text/event-stream; charset=utf-8"),
        "heartbeat response must be SSE stream, got: {}",
        String::from_utf8_lossy(&response)
    );
    assert!(
        text.contains("\r\n:\n\n\r\n") || text.contains(":\n\n"),
        "heartbeat response must contain SSE comment frame, got: {}",
        String::from_utf8_lossy(&response)
    );
}

/// 真实网络验证同一线程内连续成功发送的 SSE 事件按用户调用顺序输出。
///
/// 对应生产侧顺序语义：`SseSender` / `SseHub::try_send_to_id` 的同线程成功调用必须被
/// `HttpConnect` 按相同顺序写成 HTTP/1.1 chunked SSE 字节。
#[test]
fn sse_real_network_same_thread_order_matches_call_order() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _timer = startup_global_time_loop(10);
    let (listener, addr) = start_sse_server(SseNetworkScenario::SameThreadOrder);
    thread::sleep(Duration::from_millis(100));

    let response = read_sse_response(addr);
    listener.close(Err(Error::new(
        ErrorKind::Interrupted,
        "close SSE same-thread order test listener",
    )));

    assert_response_text_order(&response, "id: evt-1", "id: evt-2");
    assert_response_text_order(&response, "data: first", "data: second");
}

/// 真实网络验证跨线程受控成功入队的 SSE 事件按入队顺序输出。
///
/// 测试侧用 `mpsc` 明确让第二个线程在第一个线程发送成功后再发送，因此被测顺序是
/// “跨线程成功入队顺序”，不是线程创建顺序。该测试保护生产侧跨线程 `SseHub` clone 和
/// `SseSender` 队列输出顺序。
#[test]
fn sse_real_network_cross_thread_controlled_enqueue_order_matches_output_order() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _timer = startup_global_time_loop(10);
    let (listener, addr) = start_sse_server(SseNetworkScenario::CrossThreadControlledOrder);
    thread::sleep(Duration::from_millis(100));

    let response = read_sse_response(addr);
    listener.close(Err(Error::new(
        ErrorKind::Interrupted,
        "close SSE cross-thread order test listener",
    )));

    assert_response_text_order(&response, "id: evt-1", "id: evt-2");
    assert_response_text_order(&response, "data: first", "data: second");
}
