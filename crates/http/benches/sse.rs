#![feature(test)]

//! SSE real-network benchmarks for `pi_http`.
//!
//! 本文件是基准测试，不是协议正确性测试替代品。每个 benchmark 都通过真实
//! `SocketListener` 绑定 `127.0.0.1` 随机端口，并用真实 `TcpStream` 客户端发送
//! HTTP/1.1 GET 请求读取 `text/event-stream` 响应。
//!
//! 覆盖范围：
//! - 严格顺序：同一连接、同一发送线程连续发送固定序列，客户端逐条验证顺序。
//! - 并发吞吐：多个真实客户端并发建立 SSE，每个连接接收固定事件数。
//! - 首事件延迟：单连接从写入请求到读到第一条 `data:` 的延迟。
//!
//! 运行入口：
//!
//! ```bash
//! /home/vmos/.cargo/bin/cargo bench -p pi_http --bench sse
//! ```

extern crate test;

use std::io::{Error, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use pi_async_rt::rt::{serial::AsyncRuntimeBuilder, startup_global_time_loop};
use tcp::{
    connect::TcpSocket,
    server::{PortsAdapterFactory, SocketListener},
    SocketConfig,
};
use test::{black_box, Bencher};

use pi_http::{
    gateway::GatewayContext,
    route::HttpRoute,
    server::HttpListenerFactory,
    sse::{SseAcceptDecision, SseConfig, SseEvent, SseHub, SseMiddleware},
    virtual_host::{VirtualHost, VirtualHostPool, VirtualHostTab},
};

const STRICT_ORDER_EVENTS: usize = 64;
const THROUGHPUT_CLIENTS: usize = 16;
const THROUGHPUT_EVENTS_PER_CLIENT: usize = 32;

/// SSE 基准场景。
///
/// 每个场景都由生产侧 `SseMiddleware` 的 `on_open` 回调驱动发送，避免在基准里绕过
/// 路由、中间件、Hub、stream response 和真实 TCP 输出路径。
#[derive(Clone, Copy)]
enum BenchScenario {
    StrictOrder { events: usize },
    ConcurrentThroughput { events_per_client: usize },
    FirstEventLatency,
}

/// 真实网络 benchmark server。
///
/// `_timer` 和 `listener` 必须保留到 benchmark 结束，避免全局时间循环或 listener 被提前 drop。
struct BenchServer {
    _timer: Option<pi_async_rt::rt::GlobalTimeLoopHandle>,
    listener: Option<SocketListener<TcpSocket, PortsAdapterFactory<TcpSocket>>>,
    addr: SocketAddr,
}

impl Drop for BenchServer {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.close(Err(Error::new(
                ErrorKind::Interrupted,
                "close SSE benchmark listener",
            )));
        }
    }
}

/// 从本机回环地址申请一个当前空闲端口。
///
/// 该函数执行真实网络 bind；极端并发环境下端口释放后仍可能被其它进程抢占，后续
/// `SocketListener::try_bind` 会暴露该失败。
fn reserve_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("benchmark must reserve a local TCP port")
        .local_addr()
        .expect("benchmark listener must expose local addr")
        .port()
}

/// 启动真实 `pi_http` SSE benchmark server。
///
/// 构建成本不计入 `b.iter`，但所有迭代都通过真实 TCP listener、路由、中间件和连接写出路径。
fn start_bench_server(scenario: BenchScenario) -> BenchServer {
    let timer = startup_global_time_loop(10);
    let port = reserve_local_port();
    let addr: SocketAddr = format!("127.0.0.1:{}", port)
        .parse()
        .expect("benchmark addr must parse");

    let hub = SseHub::<String, TcpSocket>::builder().build();
    let hub_for_open = hub.clone();
    let middleware = SseMiddleware::with_acceptor(hub.clone(), |_accept| {
        Ok(SseAcceptDecision::accept("bench-client".to_string()))
    })
    .config(
        SseConfig::builder()
            .channel_size(1024)
            .heartbeat_interval_ms(0)
            .send_initial_comment(false)
            .build()
            .expect("benchmark SSE config must be valid"),
    )
    .on_open(move |open| {
        let hub_for_thread = hub_for_open.clone();
        let id = open.id;
        thread::spawn(move || {
            match scenario {
                BenchScenario::StrictOrder { events } => {
                    send_sequence(&hub_for_thread, id, events);
                }
                BenchScenario::ConcurrentThroughput { events_per_client } => {
                    send_sequence(&hub_for_thread, id, events_per_client);
                }
                BenchScenario::FirstEventLatency => {
                    send_sequence(&hub_for_thread, id, 1);
                }
            }
            hub_for_thread
                .try_close(id)
                .expect("benchmark SSE connection must close");
        });
        Ok(())
    })
    .build()
    .expect("benchmark SSE middleware must build");

    let mut route = HttpRoute::<TcpSocket, GatewayContext, SseMiddleware<String, TcpSocket>>::new();
    route.at("/sse").get(middleware);
    let host = VirtualHost::with(route);
    let mut hosts = VirtualHostTab::<TcpSocket, SseMiddleware<String, TcpSocket>>::new();
    hosts
        .add_default(host)
        .expect("benchmark virtual host must register");

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(
        port,
        HttpListenerFactory::<TcpSocket, _>::with_hosts(hosts, 5000).new_service(),
    );

    let rt = AsyncRuntimeBuilder::default_local_thread(None, None);
    let mut config = SocketConfig::new("127.0.0.1", &[port]);
    config.set_option(16 * 1024, 16 * 1024, 16 * 1024, 64);
    let listener = SocketListener::try_bind(
        vec![rt],
        factory,
        config,
        128,
        1024 * 1024,
        128,
        8,
        16 * 1024,
        16 * 1024,
        Some(10),
    )
    .expect("benchmark SSE server must bind");

    thread::sleep(Duration::from_millis(100));

    BenchServer {
        _timer: timer,
        listener: Some(listener),
        addr,
    }
}

/// 向指定 SSE 连接发送连续序列。
///
/// 成功返回只表示事件进入响应队列；最终网络输出由 benchmark 客户端读取和断言。
fn send_sequence(
    hub: &SseHub<String, TcpSocket>,
    id: pi_http::sse::SseConnectionId,
    events: usize,
) {
    for index in 0..events {
        let event = SseEvent::builder()
            .id(format!("evt-{:04}", index))
            .event("bench")
            .data(format!("seq-{:04}", index))
            .build()
            .expect("benchmark SSE event must be valid");
        let report = hub.try_send_to_id(id, event);
        assert_eq!(report.total, 1);
        assert_eq!(report.sent, 1);
    }
}

/// 读取真实 SSE 响应并返回所有 `data:` 行以及首事件延迟。
///
/// `first_event_started` 应在客户端写入 HTTP 请求前获取；首事件延迟以读取到第一条
/// `data:` 行时计算。函数会继续读到 chunked 结束帧，避免 benchmark 迭代留下半开连接。
fn read_sse_events(addr: SocketAddr, first_event_started: Instant) -> (Vec<String>, Duration) {
    let mut stream = TcpStream::connect(addr).expect("benchmark client must connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("benchmark client must set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("benchmark client must set write timeout");
    let req = format!(
        "GET /sse HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        addr
    );
    stream
        .write_all(req.as_bytes())
        .expect("benchmark client must write HTTP request");

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let mut data = Vec::new();
    let mut first_latency = None;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&response);
                data.clear();
                for line in text.lines() {
                    if let Some(value) = line.strip_prefix("data: ") {
                        if first_latency.is_none() {
                            first_latency = Some(first_event_started.elapsed());
                        }
                        data.push(value.to_string());
                    }
                }
                if response
                    .windows(b"0\r\n\r\n".len())
                    .any(|w| w == b"0\r\n\r\n")
                {
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                panic!(
                    "benchmark client timed out, partial response: {:?}",
                    response
                );
            }
            Err(e) => panic!("benchmark client read failed: {:?}", e),
        }
    }

    (
        data,
        first_latency.expect("benchmark response must contain at least one SSE data line"),
    )
}

fn assert_strict_order(data: &[String], events: usize) {
    assert_eq!(data.len(), events);
    for (index, value) in data.iter().enumerate() {
        assert_eq!(value, &format!("seq-{:04}", index));
    }
}

#[bench]
fn bench_sse_real_network_strict_order_64_events(b: &mut Bencher) {
    let server = start_bench_server(BenchScenario::StrictOrder {
        events: STRICT_ORDER_EVENTS,
    });
    b.bytes = STRICT_ORDER_EVENTS as u64;

    b.iter(|| {
        let started = Instant::now();
        let (data, first_latency) = read_sse_events(server.addr, started);
        black_box(first_latency);
        assert_strict_order(&data, STRICT_ORDER_EVENTS);
    });
}

#[bench]
fn bench_sse_real_network_concurrent_throughput_16x32(b: &mut Bencher) {
    let server = start_bench_server(BenchScenario::ConcurrentThroughput {
        events_per_client: THROUGHPUT_EVENTS_PER_CLIENT,
    });
    b.bytes = (THROUGHPUT_CLIENTS * THROUGHPUT_EVENTS_PER_CLIENT) as u64;

    b.iter(|| {
        let mut clients = Vec::with_capacity(THROUGHPUT_CLIENTS);
        for _ in 0..THROUGHPUT_CLIENTS {
            let addr = server.addr;
            clients.push(thread::spawn(move || {
                let started = Instant::now();
                let (data, first_latency) = read_sse_events(addr, started);
                black_box(first_latency);
                assert_eq!(data.len(), THROUGHPUT_EVENTS_PER_CLIENT);
            }));
        }
        for client in clients {
            client
                .join()
                .expect("benchmark concurrent client must finish");
        }
    });
}

#[bench]
fn bench_sse_real_network_first_event_latency(b: &mut Bencher) {
    let server = start_bench_server(BenchScenario::FirstEventLatency);
    b.bytes = 1;

    b.iter(|| {
        let started = Instant::now();
        let (data, first_latency) = read_sse_events(server.addr, started);
        assert_eq!(data.len(), 1);
        black_box(first_latency);
    });
}
