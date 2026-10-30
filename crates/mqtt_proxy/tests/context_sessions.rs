//! 主线第43节的MQTT及代理（proxy）直接调用链（call chain）验证。
//! 使用真实TCP/WebSocket与原业务入口（business entry），仅测试数据（test data）由本文件提供。
//! 每项独立子进程（subprocess）限25秒，通知（notification）和网络（network）读写限3秒。

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc::{sync_channel, SyncSender}};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use futures::{channel::oneshot, future::{FutureExt, LocalBoxFuture}};
use mqtt::{broker::{MqttBroker, MqttBrokerListener}, server::MqttBrokerProtocol,
    session::MqttConnect, utils::BrokerSession, v311::WsMqtt311};
use pi_atom::Atom;
use pi_async_rt::rt::serial::AsyncRuntimeBuilder;
use pi_gray::GrayVersion;
use pi_handler::{Args, Handler};
use pi_mqtt_proxy::service::{MqttEvent, MqttProxyListener, MqttProxyService};
use tcp::{SocketConfig, connect::TcpSocket, server::{PortsAdapterFactory, SocketListener},
    utils::{ContextAccessError, ContextHandle}};
use ws::server::WebsocketListener;

struct Stamp { owner: ThreadId, dropped: SyncSender<()> }
impl Drop for Stamp {
    fn drop(&mut self) {
        assert_eq!(self.owner, thread::current().id());
        self.dropped.send(()).unwrap();
    }
}
thread_local! {
    static CACHED: RefCell<Option<(ContextHandle<BrokerSession>, ContextHandle<Stamp>)>> = const { RefCell::new(None) };
}

struct Listener { proxy: MqttProxyListener, dropped: SyncSender<()> }
impl MqttBrokerListener<TcpSocket> for Listener {
    fn connected(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect<TcpSocket>>)
        -> LocalBoxFuture<'static, std::io::Result<()>> {
        let handle = connect.get_session().unwrap();
        let nested = {
            let mut view = handle.try_write().unwrap();
            let context = view.as_mut().get_context_mut();
            assert!(context.set(Stamp { owner: thread::current().id(), dropped: self.dropped.clone() }));
            context.get::<Stamp>().unwrap()
        };
        CACHED.with(|slot| assert!(slot.borrow_mut().replace((handle, nested)).is_none()));
        self.proxy.connected(protocol, connect)
    }

    fn closed(&self, protocol: MqttBrokerProtocol, connect: Arc<dyn MqttConnect<TcpSocket>>,
              context: BrokerSession, reason: std::io::Result<()>) -> LocalBoxFuture<'static, ()> {
        let (old, nested) = CACHED.with(|slot| slot.borrow_mut().take().unwrap());
        assert!(matches!(old.try_read(), Err(ContextAccessError::Closed)));
        assert_eq!(nested.try_read().unwrap().as_ref().owner, thread::current().id());
        // 原生产链（production chain）已交付owned会话（session）；此处只取回测试（test）自身数据（data）。
        let stamp = context.get_context().try_remove::<Stamp>().unwrap().unwrap();
        assert!(matches!(nested.try_read(), Err(ContextAccessError::Closed)));
        drop(stamp);
        self.proxy.closed(protocol, connect, context, reason)
    }
}

struct Business {
    broker: MqttBroker<TcpSocket>,
    events: SyncSender<(String, usize)>,
    resumed: SyncSender<()>,
    pause: Mutex<Option<oneshot::Receiver<()>>>,
}
impl Handler for Business {
    type A = MqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, environment: Arc<dyn GrayVersion>, _topic: Atom,
              args: Args<Self::A, (), (), (), (), (), (), ()>) -> LocalBoxFuture<'static, ()> {
        let broker = self.broker.clone();
        let events = self.events.clone();
        let resumed = self.resumed.clone();
        let event = match args { Args::OneArgs(event) => event, _ => panic!("one event is required") };
        let is_publish = matches!(&event, MqttEvent::Publish(..));
        let pause = if is_publish { self.pause.lock().unwrap().take() } else { None };
        async move {
            let (label, uid, client) = match event {
                MqttEvent::Connect(uid, broker, client, keep_alive, clean, user, password) => {
                    assert_eq!(broker, "context-broker"); assert_eq!(keep_alive, 60); assert!(clean);
                    assert!(user.is_none() && password.is_none()); ("connected", uid, client)
                },
                MqttEvent::Disconnect(uid, broker, client, _) => {
                    assert_eq!(broker, "context-broker"); ("closed", uid, client)
                },
                MqttEvent::Sub(uid, _, client, topics) => {
                    assert_eq!(topics, vec![("topic".to_string(), 0)]); ("subscribed", uid, client)
                },
                MqttEvent::Unsub(uid, _, client, topics) => {
                    assert_eq!(topics, vec!["topic".to_string()]); ("unsubscribed", uid, client)
                },
                MqttEvent::Publish(uid, _, client, remote, topic, payload) => {
                    assert!(remote.is_some()); assert_eq!(topic, "topic"); assert_eq!(payload.as_slice(), b"hello");
                    ("published", uid, client)
                },
            };
            assert_eq!(client, "context-client"); assert_eq!(environment.get_id(), uid);
            let cached = if label == "closed" {
                assert!(broker.get_session(&client).is_none()); None
            } else {
                let connect = broker.get_session(&client).unwrap();
                let handle = connect.get_session().unwrap();
                {
                    // 原代理（proxy）的许可（permit）须在进入处理器（handler）前归还。
                    let view = handle.try_write().expect("the proxy permit must end before this future");
                    assert_eq!(view.as_ref().get_client_id(), &client);
                    let stamp = view.as_ref().get_context().get::<Stamp>().unwrap();
                    assert_eq!(stamp.try_read().unwrap().as_ref().owner, thread::current().id());
                }
                Some(handle)
            };
            events.send((label.to_string(), uid)).unwrap();
            if let Some(pause) = pause {
                pause.await.unwrap();
                assert!(matches!(cached.unwrap().try_read(), Err(ContextAccessError::Closed)));
                resumed.send(()).unwrap();
            }
        }.boxed_local()
    }
}

fn frame(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 126);
    let key = [1_u8, 2, 3, 4];
    let mut bytes = vec![0x82, 0x80 | payload.len() as u8]; bytes.extend_from_slice(&key);
    bytes.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ key[index % 4])); bytes
}
fn packet(header: u8, body: &[u8]) -> Vec<u8> {
    assert!(body.len() < 128);
    let mut bytes = vec![header, body.len() as u8]; bytes.extend_from_slice(body); bytes
}
fn message(client: &mut TcpStream, expected: &[u8]) {
    let mut header = [0_u8; 2]; client.read_exact(&mut header).unwrap();
    assert_eq!(header, [0x82, expected.len() as u8]);
    let mut body = vec![0; expected.len()]; client.read_exact(&mut body).unwrap(); assert_eq!(body, expected);
}

fn session(mode: &str) {
    let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let runtime = AsyncRuntimeBuilder::<()>::default_local_thread(Some("SessionOwner"), None);
    let protocol = Arc::new(WsMqtt311::with_name("mqtt", "context-broker", 0, false));
    let broker = protocol.get_broker().clone();
    let (events_tx, events_rx) = sync_channel(8);
    let (dropped_tx, dropped_rx) = sync_channel(2);
    let (resumed_tx, resumed_rx) = sync_channel(1);
    let (pause_tx, pause_rx) = oneshot::channel();
    let business = Arc::new(Business { broker: broker.clone(), events: events_tx, resumed: resumed_tx,
        pause: Mutex::new(if mode == "pending" { Some(pause_rx) } else { None }) });
    broker.register_listener(Arc::new(Listener {
        proxy: MqttProxyListener::with_handler(Some(business.clone())), dropped: dropped_tx }));
    if mode != "standard" { broker.register_service(Arc::new(MqttProxyService::with_handler(Some(business)))); }
    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port, Box::new(WebsocketListener::<TcpSocket>::with_protocol(protocol)));
    let listener = SocketListener::try_bind(vec![runtime.clone()], factory, SocketConfig::new("127.0.0.1", &[port]),
        1, 2 * 1024 * 1024, 16, 1, 4096, 4096, Some(10)).unwrap();
    let mut client = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(3)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(3))).unwrap(); client.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    client.write_all(format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: mqtt\r\n\r\n").as_bytes()).unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 8192); let mut byte = [0]; client.read_exact(&mut byte).unwrap(); header.push(byte[0]);
    }
    assert!(header.starts_with(b"HTTP/1.1 101 "));
    let mut body = b"\0\x04MQTT\x04\x02\0\x3c\0\x0econtext-client".to_vec();
    client.write_all(&frame(&packet(0x10, &body))).unwrap();
    let (label, uid) = events_rx.recv_timeout(Duration::from_secs(3)).unwrap(); assert_eq!(label, "connected");
    message(&mut client, &[0x20, 2, 0, 0]);
    client.write_all(&frame(&packet(0x82, b"\0\x01\0\x05topic\0"))).unwrap();
    message(&mut client, &[0x90, 3, 0, 1, 0]);
    if mode != "standard" { assert_eq!(events_rx.recv_timeout(Duration::from_secs(3)).unwrap(), ("subscribed".to_string(), uid)); }
    body = packet(0x30, b"\0\x05topichello");
    client.write_all(&frame(&body)).unwrap();
    if mode == "standard" { message(&mut client, &body); }
    else { assert_eq!(events_rx.recv_timeout(Duration::from_secs(3)).unwrap(), ("published".to_string(), uid)); }
    if mode == "pending" { client.shutdown(Shutdown::Both).unwrap(); }
    else {
        client.write_all(&frame(&packet(0xa2, b"\0\x02\0\x05topic"))).unwrap();
        message(&mut client, &[0xb0, 2, 0, 2]);
        if mode != "standard" { assert_eq!(events_rx.recv_timeout(Duration::from_secs(3)).unwrap(), ("unsubscribed".to_string(), uid)); }
        client.write_all(&frame(&[0xc0, 0])).unwrap(); message(&mut client, &[0xd0, 0]);
        client.write_all(&frame(&[0xe0, 0])).unwrap();
    }
    assert_eq!(events_rx.recv_timeout(Duration::from_secs(3)).unwrap(), ("closed".to_string(), uid));
    dropped_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(broker.session_size(), 0);
    if mode == "pending" { pause_tx.send(()).unwrap(); resumed_rx.recv_timeout(Duration::from_secs(3)).unwrap(); }
    else { drop(pause_tx); }
    let (fence_tx, fence_rx) = sync_channel(1);
    runtime.send(async move { CACHED.with(|slot| assert!(slot.borrow().is_none())); fence_tx.send(()).unwrap(); });
    fence_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(events_rx.try_recv().is_err()); assert!(dropped_rx.try_recv().is_err());
    listener.close(Ok(()));
}

#[test]
#[ignore]
fn test_session_child() {
    let mode = std::env::var("PI_MQTT_CONTEXT_SESSION").unwrap();
    assert!(mode == "normal" || mode == "pending" || mode == "standard"); session(&mode);
}
fn run_child(mode: &str) {
    let path = std::env::temp_dir().join(format!("pi-mqtt-context-{}-{mode}.txt", std::process::id()));
    let file = std::fs::OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "test_session_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env("PI_MQTT_CONTEXT_SESSION", mode).env("RUST_BACKTRACE", "0")
        .stdout(Stdio::from(file.try_clone().unwrap())).stderr(Stdio::from(file));
    #[cfg(unix)]
    { use std::os::unix::process::CommandExt; command.process_group(0); }
    let mut child = command.spawn().unwrap(); let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() { break status; }
        if started.elapsed() >= Duration::from_secs(25) {
            child.kill().unwrap(); child.wait().unwrap();
            let text = std::fs::read_to_string(&path).unwrap(); std::fs::remove_file(&path).unwrap();
            panic!("session sequence exceeded deadline: {mode}\n{text}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let text = std::fs::read_to_string(&path).unwrap(); std::fs::remove_file(&path).unwrap();
    println!("{mode}\n{text}"); assert!(status.success(), "session sequence status={status}");
}
/// 原代理（proxy）连接、订阅、发布、退订和关闭事件（event）的数据及身份保持。
#[test]
fn test_proxy_messages() { run_child("normal"); }
/// 业务等待（business wait）时关闭仍可交付owned会话（session），缓存句柄（cached handle）随后Closed。
#[test]
fn test_close_while_publish_pending() { run_child("pending"); }
/// 未设置代理服务（proxy service）时沿原订阅表（subscription table）真实回送发布内容（publish payload）。
#[test]
fn test_standard_messages() { run_child("standard"); }
