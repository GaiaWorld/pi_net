//! 主线第43节的WebSocket直接消费者（consumer）验证，使用真实TCP装配（assembly）。
//! 单帧、分帧及控制帧（control frame）沿原协议（protocol）；等待（await）期间仅缓存普通句柄（context handle）。
//! 每个场景（scenario）一个有截止的子进程（subprocess），不增加生产依赖（production dependency）。

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc::{sync_channel, SyncSender}};
use std::thread;
use std::time::{Duration, Instant};

use futures::{channel::oneshot, future::{FutureExt, LocalBoxFuture}};
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use tcp::{Socket, SocketConfig};
use tcp::connect::TcpSocket;
use tcp::server::{PortsAdapterFactory, SocketListener};
use tcp::utils::ContextAccessError;
use pi_ws::connect::WsSocket;
use pi_ws::server::WebsocketListener;
use pi_ws::utils::{ChildProtocol, WsFrameType, WsSession};

struct Protocol {
    ready: SyncSender<Vec<u8>>,
    resumed: SyncSender<()>,
    closed: SyncSender<()>,
    pause: Mutex<Option<oneshot::Receiver<()>>>,
}

impl<S: Socket> ChildProtocol<S> for Protocol {
    fn protocol_name(&self) -> &str { "context-sequence" }
    fn is_strict(&self) -> bool { false }

    /// 同步取得消息（message）及嵌套普通句柄（context handle），异步（async）部分重新访问外层会话（session）。
    fn decode_protocol(&self, connect: WsSocket<S>, context: &mut WsSession) -> LocalBoxFuture<'static, std::io::Result<()>> {
        assert!(context.is_binary());
        let message = context.pop_msg();
        assert!(context.get_context().set(73_u32));
        let nested = context.get_context().get::<u32>().unwrap();
        let ready = self.ready.clone();
        let resumed = self.resumed.clone();
        let pause = self.pause.lock().unwrap().take();
        async move {
            let parent = connect.get_session().unwrap();
            {
                let view = parent.try_read().expect("the outer permit must have ended before this step");
                assert!(view.as_ref().is_binary());
            }
            assert_eq!(*nested.try_read().unwrap().as_ref(), 73);
            ready.send(message.clone()).unwrap();
            if let Some(pause) = pause {
                pause.await.unwrap();
                assert!(matches!(parent.try_read(), Err(ContextAccessError::Closed)));
                assert!(matches!(nested.try_read(), Err(ContextAccessError::Closed)));
            } else {
                connect.send(WsFrameType::Binary, message)?;
            }
            resumed.send(()).unwrap();
            Ok(())
        }.boxed_local()
    }

    /// owned会话（session）由原关闭链交付；嵌套移出不要求先归还普通句柄（context handle）。
    fn close_protocol(&self, _connect: WsSocket<S>, context: WsSession,
                      _reason: std::io::Result<()>) -> LocalBoxFuture<'static, ()> {
        assert_eq!(context.get_context().try_remove::<u32>().unwrap(), Some(73));
        assert!(context.get_context().is_empty());
        let closed = self.closed.clone();
        async move { closed.send(()).unwrap(); }.boxed_local()
    }

    fn protocol_timeout(&self, _connect: WsSocket<S>, _context: &mut WsSession,
                        _event: tcp::SocketEvent) -> LocalBoxFuture<'static, std::io::Result<()>> {
        async { Ok(()) }.boxed_local()
    }
}

/// 发送的帧（frame）使用固定四字节掩码（mask），只覆盖本场景（scenario）的短数据（data）。
fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 126);
    let key = [1_u8, 2, 3, 4];
    let mut bytes = vec![(if fin { 0x80 } else { 0 }) | opcode, 0x80 | payload.len() as u8];
    bytes.extend_from_slice(&key);
    bytes.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ key[index % 4]));
    bytes
}

fn response(stream: &mut TcpStream, opcode: u8, expected: &[u8]) {
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header, [0x80 | opcode, expected.len() as u8]);
    let mut payload = vec![0_u8; expected.len()];
    stream.read_exact(&mut payload).unwrap();
    assert_eq!(payload, expected);
}

/// 客户端（client）先完成原握手（handshake），再提交数据（data），不依赖合包（coalescing）时序。
fn protocol_case(mode: &str) {
    let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let runtime = AsyncRuntimeBuilder::<()>::default_local_thread(Some("ProtocolOwner"), None);
    let started = Instant::now();
    while !runtime.is_running() {
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::yield_now();
    }
    let (ready_tx, ready_rx) = sync_channel(4);
    let (resumed_tx, resumed_rx) = sync_channel(4);
    let (closed_tx, closed_rx) = sync_channel(4);
    let (pause_tx, pause_rx) = oneshot::channel();
    let protocol = Arc::new(Protocol {
        ready: ready_tx, resumed: resumed_tx, closed: closed_tx,
        pause: Mutex::new(if mode == "pending" { Some(pause_rx) } else { None }),
    });
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port, Box::new(WebsocketListener::<TcpSocket>::with_protocol(protocol)));
    let listener = SocketListener::try_bind(vec![runtime.clone()], factory,
        SocketConfig::new("127.0.0.1", &[port]), 1, 2 * 1024 * 1024, 16, 1, 4096, 4096, Some(10)).unwrap();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    client.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    let request = format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n");
    client.write_all(request.as_bytes()).unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 8192);
        let mut byte = [0_u8; 1];
        client.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    assert!(header.starts_with(b"HTTP/1.1 101 "));
    let expected = if mode == "fragmented" { b"abcd".as_slice() } else { b"hello".as_slice() };
    let bytes = if mode == "fragmented" {
        [frame(false, 2, b"ab"), frame(true, 0, b"cd")].concat()
    } else { frame(true, 2, expected) };
    client.write_all(&bytes).unwrap();
    assert_eq!(ready_rx.recv_timeout(Duration::from_secs(3)).unwrap(), expected);
    if mode == "pending" {
        client.shutdown(Shutdown::Both).unwrap();
        closed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        pause_tx.send(()).unwrap();
        resumed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    } else {
        resumed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        response(&mut client, 2, expected);
        client.write_all(&frame(true, 9, &[])).unwrap();
        response(&mut client, 10, &[]);
        client.shutdown(Shutdown::Both).unwrap();
        closed_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        drop(pause_tx);
    }
    let (fence_tx, fence_rx) = sync_channel(1);
    runtime.send(async move { fence_tx.send(()).unwrap(); });
    fence_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(ready_rx.try_recv().is_err());
    assert!(closed_rx.try_recv().is_err());
    assert!(resumed_rx.try_recv().is_err());
    drop(client);
    listener.close(Ok(()));
}

/// 子入口（child entry）只处理一个选定场景（scenario）。
#[test]
#[ignore]
fn test_protocol_child() {
    let mode = std::env::var("PI_WS_CONTEXT_SEQUENCE").unwrap();
    assert!(mode == "single" || mode == "fragmented" || mode == "pending");
    protocol_case(&mode);
}

fn run_child(mode: &str) {
    let path = std::env::temp_dir().join(format!("pi-ws-context-{}-{mode}.txt", std::process::id()));
    let output = std::fs::OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "test_protocol_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PI_WS_CONTEXT_SEQUENCE", mode).env("RUST_BACKTRACE", "0")
        .stdout(Stdio::from(output.try_clone().unwrap())).stderr(Stdio::from(output));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() { break status; }
        if started.elapsed() >= Duration::from_secs(25) {
            child.kill().unwrap();
            child.wait().unwrap();
            let output = std::fs::read_to_string(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            panic!("protocol sequence exceeded deadline: {mode}\n{output}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    println!("{mode}\n{output}");
    assert!(status.success(), "protocol sequence mode={mode} status={status}");
}

/// 单帧消息（single-frame message）与空Ping/Pong保持原行为。
#[test]
fn test_single_message_and_control() { run_child("single"); }
/// 首帧/结束帧（first/final frame）消息（message）保持原拼接顺序。
#[test]
fn test_fragmented_message_and_control() { run_child("fragmented"); }
/// 解码等待（decode wait）时正常关闭仍移出两层会话（session），旧句柄（context handle）返回Closed。
#[test]
fn test_close_while_protocol_pending() { run_child("pending"); }
