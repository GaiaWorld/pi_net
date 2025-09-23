use std::thread;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{FutureExt, LocalBoxFuture};
use futures::{SinkExt, StreamExt};
use env_logger;
use pi_async_rt::rt::startup_global_time_loop;

use pi_atom::Atom;
use pi_gray::GrayVersion;
use pi_handler::{Args, Handler};

use tcp::{Socket, SocketConfig,
          connect::TcpSocket,
          server::{PortsAdapterFactory, SocketListener},
          utils::Ready};
use ws::server::WebsocketListener;
use mqtt::server::{WsMqttBrokerFactory, register_mqtt_listener, register_mqtt_service};

use pi_mqtt_proxy::service::{MqttEvent, MqttConnectHandle, MqttProxyListener, MqttProxyService};

// 随机数trait
use rand::Rng;

// WebSocket相关trait
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

struct TestMqttConnectHandler<S: Socket> {
    _phantom: std::marker::PhantomData<S>,
}

unsafe impl<S: Socket> Send for TestMqttConnectHandler<S> {}
unsafe impl<S: Socket> Sync for TestMqttConnectHandler<S> {}

impl<S: Socket> Handler for TestMqttConnectHandler<S> {
    type A = MqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self,
              env: Arc<dyn GrayVersion>, _: Atom,
              args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        async move {
            let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle<S>) };
            match args {
                Args::OneArgs(MqttEvent::Connect(socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd)) => {
                    // println!("并发测试客户端连接: socket_id: {:?}, broker_name: {:?}, client_id: {:?}, keep_alive: {:?}, is_clean_session: {:?}, user: {:?}, pwd: {:?}",
                    //         socket_id, broker_name, client_id, keep_alive, is_clean_session, user, pwd);

                    if let Some(hibernate) = connect.hibernate(Ready::ReadWrite) {
                        let connect_copy = connect.clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(50));
                            while !connect_copy.wakeup(Ok(())) {
                                thread::sleep(Duration::from_millis(1));
                            }
                        });
                        let _ = hibernate.await;
                    }
                }
                Args::OneArgs(MqttEvent::Disconnect(socket_id, broker_name, client_id, reason)) => {
                    // println!("并发测试客户端断开: socket_id: {:?}, broker_name: {:?}, client_id: {:?}, reason: {:?}",
                    //         socket_id, broker_name, client_id, reason);
                },
                _ => {
                    // println!("并发测试其他事件");
                },
            }
        }.boxed_local()
    }
}

impl<S: Socket> TestMqttConnectHandler<S> {
    pub fn new() -> Self {
        TestMqttConnectHandler { _phantom: std::marker::PhantomData }
    }
}

struct TestMqttRequestHandler<S: Socket> {
    _phantom: std::marker::PhantomData<S>,
}

unsafe impl<S: Socket> Send for TestMqttRequestHandler<S> {}
unsafe impl<S: Socket> Sync for TestMqttRequestHandler<S> {}

impl<S: Socket> Handler for TestMqttRequestHandler<S> {
    type A = MqttEvent;
    type B = ();
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();

    fn handle(&self, env: Arc<dyn GrayVersion>, _topic: Atom, args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>) -> LocalBoxFuture<'static, Self::HandleResult> {
        async move {
            let connect = unsafe { Arc::from_raw(Arc::into_raw(env) as *const MqttConnectHandle<S>) };
            match args {
                Args::OneArgs(MqttEvent::Sub(socket_id, broker_name, client_id, topics)) => {
                    println!("并发测试订阅: socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}",
                            socket_id, broker_name, client_id, topics);

                    for (topic, _) in topics {
                        connect.sub(topic);
                    }
                },
                Args::OneArgs(MqttEvent::Unsub(socket_id, broker_name, client_id, topics)) => {
                    println!("并发测试退订: socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topics: {:?}",
                            socket_id, broker_name, client_id, topics);

                    for topic in topics {
                        connect.unsub(topic);
                    }
                },
                Args::OneArgs(MqttEvent::Publish(socket_id, broker_name, client_id, _address, topic, payload)) => {
                    println!("并发测试发布: socket_id: {:?}, broker_name: {:?}, client_id: {:?}, topic: {:?}, payload_len: {:?}",
                            socket_id, broker_name, client_id, topic, payload.len());

                    connect.reply(payload.as_slice().to_vec());
                },
                _ => {
                    println!("并发测试其他请求事件");
                },
            }
        }.boxed_local()
    }
}

impl<S: Socket> TestMqttRequestHandler<S> {
    pub fn new() -> Self {
        TestMqttRequestHandler { _phantom: std::marker::PhantomData }
    }
}

/// Linux下的并发连接测试：启动MQTT Broker并使用多个WebSocket MQTT v3.1客户端进行并发连接测试
#[test]
fn test_concurrent_websocket_mqtt_connections() {
    // 启动日志系统
    env_logger::builder().format_timestamp_millis().init();

    let _handle = startup_global_time_loop(10);
    let rts = vec![pi_async_rt::rt::serial::AsyncRuntimeBuilder::default_local_thread(None, None); 8];

    let protocol_name = "mqttv3.1";
    let broker_name = "test_ws_mqtt_concurrent";
    let port = 38081;  // 使用不同端口避免冲突

    // 构建Mqtt Broker，并注册Mqtt全局监听器和全局服务
    let broker_factory = Arc::new(WsMqttBrokerFactory::new(protocol_name,
                                                           broker_name,
                                                           port,
                                                           false));
    let event_handler = Arc::new(TestMqttConnectHandler::<TcpSocket>::new());
    let rpc_handler = Arc::new(TestMqttRequestHandler::<TcpSocket>::new());
    let listener = Arc::new(MqttProxyListener::with_handler(Some(event_handler)));
    let service = Arc::new(MqttProxyService::with_handler(Some(rpc_handler)));
    register_mqtt_listener(broker_name, listener);
    register_mqtt_service(broker_name, service);

    let mut factory = PortsAdapterFactory::<TcpSocket>::new();
    factory.bind(port,
                 Box::new(WebsocketListener::with_protocol(broker_factory.new_child_protocol(false))));
    let mut config = SocketConfig::new("0.0.0.0", factory.ports().as_slice());
    config.set_option(16384, 16384, 16384, 16);

    match SocketListener::bind(rts,
                               factory,
                               config,
                               100000,
                               1024 * 1024,
                               10240,
                               16,
                               4096,
                               4096,
                               Some(1000)) {
        Err(e) => {
            println!("!!!> Mqtt Listener Bind Error, reason: {:?}", e);
            return;
        },
        Ok(driver) => {
            println!("===> Mqtt Listener Bind Ok");
        }
    }

    // 等待服务器启动
    thread::sleep(Duration::from_millis(100));

    // 启动客户端并发连接测试（8线程运行时）
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    client_rt.block_on(async {
        run_connect_test().await;
    });

    thread::sleep(Duration::from_millis(1000000000));
}

/// 轻量级WebSocket MQTT v3.1客户端实现
#[derive(Debug)]
enum ClientError {
    WebSocketError(tokio_tungstenite::tungstenite::Error),
    IoError(std::io::Error),
    UrlParseError(url::ParseError),
    MqttError(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for ClientError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        ClientError::WebSocketError(e)
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::IoError(e)
    }
}

impl From<url::ParseError> for ClientError {
    fn from(e: url::ParseError) -> Self {
        ClientError::UrlParseError(e)
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::WebSocketError(e) => write!(f, "WebSocket error: {}", e),
            ClientError::IoError(e) => write!(f, "IO error: {}", e),
            ClientError::UrlParseError(e) => write!(f, "URL parse error: {}", e),
            ClientError::MqttError(msg) => write!(f, "MQTT error: {}", msg),
        }
    }
}

impl std::error::Error for ClientError {}

struct WsMqttClient {
    client_id: String,
    websocket_stream: Option<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
    connected: bool,
}

impl WsMqttClient {
    /// 创建新的WebSocket MQTT客户端
    fn new(client_id: String) -> Self {
        Self {
            client_id,
            websocket_stream: None,
            connected: false,
        }
    }

    /// 连接到MQTT Broker（带重试机制）
    async fn connect(&mut self, server_addr: &str) -> Result<(), ClientError> {
        let max_retries = 3;
        let mut retry_count = 0;

        while retry_count < max_retries {
            match self.try_connect_once(server_addr).await {
                Ok(_) => {
                    return Ok(());
                },
                Err(e) => {
                    retry_count += 1;
                    if retry_count < max_retries {
                        eprintln!("连接 {} 第 {} 次尝试失败: {:?}, 1秒后重试...",
                                 self.client_id, retry_count, e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    } else {
                        eprintln!("连接 {} 最终连接失败: {:?}", self.client_id, e);
                        return Err(e);
                    }
                }
            }
        }

        Ok(())
    }

    /// 尝试连接一次
    async fn try_connect_once(&mut self, server_addr: &str) -> Result<(), ClientError> {
        let url = url::Url::parse(&format!("ws://{}:{}", server_addr, "38081"))?;

        // 创建WebSocket请求，指定子协议为mqttv3.1
        let mut request = url.into_client_request()?;
        request.headers_mut().insert("Sec-WebSocket-Protocol", "mqttv3.1".parse().unwrap());

        let (ws_stream, _) = tokio_tungstenite::connect_async(request).await?;
        self.websocket_stream = Some(ws_stream);

        // 发送MQTT CONNECT报文
        let connect_packet = self.create_connect_packet();
        self.send_packet(connect_packet).await?;

        // 等待CONNACK响应
        self.wait_for_connack().await?;

        self.connected = true;
        Ok(())
    }

    /// 创建MQTT CONNECT报文
    fn create_connect_packet(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // MQTT固定头部
        buf.push(0x10); // CONNECT报文类型

        // 可变长度头部（暂时预留位置）
        buf.push(0x00); // 长度占位符

        // 协议名
        let protocol_name = "MQTT";
        buf.push((protocol_name.len() >> 8) as u8);
        buf.push(protocol_name.len() as u8);
        buf.extend_from_slice(protocol_name.as_bytes());

        // 协议级别
        buf.push(0x04); // MQTT 3.1.1

        // 连接标志
        let mut flags = 0x00;
        flags |= 0x02; // Clean session
        buf.push(flags);

        // Keep alive
        buf.push(0x00);
        buf.push(30); // 30 seconds

        // Client ID
        let client_id = self.client_id.clone();
        buf.push((client_id.len() >> 8) as u8);
        buf.push(client_id.len() as u8);
        buf.extend_from_slice(client_id.as_bytes());

        // 更新长度
        let remaining_length = buf.len() - 2; // 减去固定头部
        buf[1] = remaining_length as u8;

        buf
    }

    /// 发送MQTT报文
    async fn send_packet(&mut self, packet: Vec<u8>) -> Result<(), ClientError> {
        if let Some(ref mut ws_stream) = self.websocket_stream {
            let msg = Message::Binary(packet);
            ws_stream.send(msg).await?;
        }
        Ok(())
    }

    /// 等待CONNACK响应
    async fn wait_for_connack(&mut self) -> Result<(), ClientError> {
        if let Some(ref mut ws_stream) = self.websocket_stream {
            match ws_stream.next().await {
                Some(Ok(msg)) => {
                    match msg {
                        Message::Binary(data) => {
                            if data.len() >= 4 && data[0] == 0x20 {
                                // CONNACK报文
                                let return_code = data[3];
                                if return_code == 0x00 {
                                    return Ok(());
                                } else {
                                    return Err(ClientError::MqttError(format!("CONNACK returned error code: {}", return_code)));
                                }
                            }
                        },
                        _ => return Err(ClientError::MqttError("Unexpected message type".to_string())),
                    }
                },
                Some(Err(e)) => return Err(e.into()),
                None => return Err(ClientError::MqttError("Connection closed".to_string())),
            }
        }
        Ok(())
    }

    /// 断开连接（确保资源正确释放）
    async fn disconnect(&mut self) -> Result<(), ClientError> {
        if self.connected {
            // 发送DISCONNECT报文
            let disconnect_packet = vec![0xE0, 0x00]; // MQTT DISCONNECT
            if let Err(e) = self.send_packet(disconnect_packet).await {
                eprintln!("连接 {} 发送DISCONNECT报文失败: {:?}", self.client_id, e);
            }

            // 关闭WebSocket连接
            if let Some(mut ws_stream) = self.websocket_stream.take() {
                if let Err(e) = ws_stream.close(None).await {
                    eprintln!("连接 {} 关闭WebSocket失败: {:?}", self.client_id, e);
                }
            }

            self.connected = false;
            eprintln!("连接 {} 已断开", self.client_id);
        }
        Ok(())
    }
}

/// 运行并发连接测试
async fn run_connect_test() {
    let server_addr = "127.0.0.1";
    let concurrent_clients = 100; // 并发客户端数量
    let connections_per_client = 100; // 每个客户端的连接数量
    let test_cycles = 10; // 测试循环次数

    println!("开始并发连接测试: {} 个客户端，每个客户端 {} 个连接，{} 次循环",
             concurrent_clients, connections_per_client, test_cycles);

    let start_time = std::time::Instant::now();
    let mut handles = Vec::new();

    // 创建并发客户端
    for i in 0..concurrent_clients {
        let client_id = format!("test_client_{}", i);
        let server_addr = server_addr.to_string();

        let handle = tokio::spawn(async move {
            let mut client_total_connections = 0;

            // 执行10次循环
            for cycle in 0..test_cycles {
                let cycle_start = std::time::Instant::now();
                let mut client_handles = Vec::new();

                // 为每个客户端创建100个连接（添加间隔）
                for j in 0..connections_per_client {
                    let connection_id = format!("{}_{}_{}", client_id, cycle, j);
                    let server_addr = server_addr.clone();

                    let conn_handle = tokio::spawn(async move {
                        let mut client = WsMqttClient::new(connection_id.clone());
                        let mut connect_count = 0;

                        // 连接 -> 等待 -> 断开 -> 重复，确保完成10次连接
                        while connect_count < 1 {
                            match client.connect(&server_addr).await {
                                Ok(_) => {
                                    connect_count += 1;
                                    // 减少连接保持时间，确保能完成10次连接
                                    // let wait_time = rand::thread_rng().gen_range(300..=800);
                                    // tokio::time::sleep(Duration::from_millis(wait_time)).await;

                                    if let Err(e) = client.disconnect().await {
                                        eprintln!("连接 {} 断开连接失败: {:?}", connection_id, e);
                                    }

                                    eprintln!("连接 {} 完成第 {} 次连接", connection_id, connect_count);
                                },
                                Err(e) => {
                                    eprintln!("连接 {} 连接失败: {:?}", connection_id, e);
                                }
                            }
                        }

                        (connection_id, connect_count)
                    });

                    client_handles.push(conn_handle);

                    // 在连接创建之间添加小延迟，避免端口冲突
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }

                // 等待该客户端的当前循环所有连接完成
                let mut cycle_connections = 0;
                for conn_handle in client_handles {
                    match conn_handle.await {
                        Ok((connection_id, count)) => {
                            cycle_connections += count;
                            client_total_connections += count;
                        },
                        Err(e) => {
                            eprintln!("连接任务失败: {:?}", e);
                        }
                    }
                }

                // 打印当前循环完成信息
                println!("客户端 {} 第 {} 次循环完成：建立了 {} 个连接", client_id, cycle + 1, cycle_connections);

                // 如果不是最后一次循环，休眠10秒
                if cycle < test_cycles - 1 {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }

            (client_id, client_total_connections)
        });

        handles.push(handle);

        // 在客户端启动之间添加小延迟
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 等待所有客户端完成
    let mut total_connections = 0;
    for handle in handles {
        match handle.await {
            Ok((client_id, count)) => {
                total_connections += count;
                println!("客户端 {} 总共完成了 {} 次连接", client_id, count);
            },
            Err(e) => {
                eprintln!("客户端任务失败: {:?}", e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("并发连接测试完成:");
    println!("- 总连接次数: {}", total_connections);
    println!("- 测试时间: {:?}", elapsed);
    println!("- 平均每秒连接数: {:.2}", total_connections as f64 / elapsed.as_secs_f64());

    // 等待10分钟（600秒）
    println!("所有连接已关闭，等待10秒...");
    tokio::time::sleep(Duration::from_secs(10)).await;
    println!("测试结束");
    #[cfg(target_os = "linux")]
    unsafe {
        let b = libc::malloc_trim(0);
        println!("malloc_trim: {}", b);
    }
}