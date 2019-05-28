extern crate nodec;
extern crate worker;
extern crate atom;
extern crate timer;

use std::thread;
use std::sync::Arc;
use std::io::Result;
use std::time::Duration;

use worker::task::{TaskType, Task};
use worker::impls::{NET_WORKER_WALKER, NET_TASK_POOL};
use worker::worker_pool::WorkerPool;
use atom::Atom;
use worker::worker::WorkerType;
use timer::TIMER;

use nodec::wsc::SharedWSClient;
use nodec::mqttc::SharedMqttClient;
use nodec::rpc::RPCClient;

#[test]
fn test_wsc() {
    TIMER.run();
    let worker_pool = Box::new(WorkerPool::new("test mqttc".to_string(), WorkerType::Net, 10, 1024 * 1024, 10000000, NET_WORKER_WALKER.clone()));
    worker_pool.run(NET_TASK_POOL.clone());

    println!("!!!!!!test ws...");
    if let Ok(wsc) = SharedWSClient::create("ws://echo.websocket.org/") {
        if let Ok(_) = wsc.connect() {
            wsc.send_text("Abc");
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!reply failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!reply ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));

            wsc.send_bin(b"ABC".to_vec());
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!reply failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!reply ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));

            wsc.ping(b"Hello".to_vec());
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!ping failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!ping ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));

            wsc.close(1001, "invalid close");
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!close failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!close ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));
        }
    }

    thread::sleep(Duration::from_millis(5000));

    println!("!!!!!!test wss...");
    if let Ok(wsc) = SharedWSClient::create("wss://echo.websocket.org/") {
        if let Ok(_) = wsc.connect() {
            wsc.send_text("Abc");
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!reply failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!reply ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));

            wsc.send_bin(b"ABC".to_vec());
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!reply failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!reply ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));

            wsc.ping(b"Hello".to_vec());
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!ping failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!ping ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));

            wsc.close(1001, "invalid close");
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!close failed, e: {:?}", e),
                    Ok(vec) => println!("!!!!!!!!!!!!close ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(1000), Box::new(func));
        }
    }

    thread::sleep(Duration::from_millis(5000));

    println!("!!!!!!test timeout...");
    if let Ok(wsc) = SharedWSClient::create("ws://echo.websocket.org/") {
        if let Ok(_) = wsc.connect() {
            wsc.send_text("Abc");
            let func = move |wsc: SharedWSClient, result: Result<Option<Vec<u8>>>| {
                match result {
                    Err(e) => println!("!!!!!!!!!!!!reply timeout, url: {:?}, e: {:?}", wsc.get_url(), e),
                    Ok(vec) => println!("!!!!!!!!!!!!reply ok, vec: {:?}", vec),
                }
            };
            wsc.receive(Some(100), Box::new(func));

            println!("!!!!!!!!!!!!send failed, e: {:?}", wsc.send_text("Abc"));
        }
    }

    thread::sleep(Duration::from_millis(10000000000));
}

#[test]
fn test_mqttc() {
    TIMER.run();
    let worker_pool = Box::new(WorkerPool::new("test mqttc".to_string(), WorkerType::Net, 10, 1024 * 1024, 1000000, NET_WORKER_WALKER.clone()));
    worker_pool.run(NET_TASK_POOL.clone());

    match SharedMqttClient::create("ws://127.0.0.1:1234") {
        Err(e) => println!("!!!!!!create mqtt failed, e: {:?}", e),
        Ok(client) => {
            let func = Arc::new(move |result: Result<Option<Vec<u8>>>| {
                println!("!!!!!!mqtt connect result: {:?}", result);
                false
            });
            client.connect(None, 10000, "EchoClient".to_string(), true, None, None, Some(5000), func);
        },
    }

    thread::sleep(Duration::from_millis(10000000000));
}

#[test]
fn test_rpc() {
    TIMER.run();
    let worker_pool = Box::new(WorkerPool::new("test mqttc".to_string(), WorkerType::Net, 10, 1024 * 1024, 10000000, NET_WORKER_WALKER.clone()));
    worker_pool.run(NET_TASK_POOL.clone());

    match RPCClient::create("ws://127.0.0.1:1234") {
        Err(e) => println!("!!!!!!create rpc client failed, e: {:?}", e),
        Ok(client) => {
            let client_copy = client.clone();
            let connect_cb = Arc::new(move |result| {
                println!("!!!!!!rpc client connect, result: {:?}", result);
                let request_cb = Arc::new(move |result| {
                    println!("!!!!!!rpc client request, result: {:?}", result);
                });
                client_copy.request("game/app_a/user/server/user_base_rpc_call.login".to_string(), vec![10, 10, 10], 10, request_cb);
            });
            let closed_cb = Arc::new(move |result| {
                println!("!!!!!!rpc client closed, result: {:?}", result);
            });
            client.connect(65535, "EchoClient", 10, connect_cb, closed_cb);
        }
    }

    thread::sleep(Duration::from_millis(10000000000));
}

