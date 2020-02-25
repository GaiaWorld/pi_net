extern crate atom;
extern crate worker;
extern crate httpc;

use std::thread;
use std::fs::File;
use std::io::Result;
use std::time::Duration;

use worker::worker_pool::WorkerPool;
use worker::impls::{NET_WORKER_WALKER, NET_TASK_POOL};
use worker::worker::WorkerType;
use atom::Atom;

use httpc::{HttpClientOptions, SharedHttpc, SharedHttpClient, HttpClient, HttpClientBody, HttpClientResponse};

#[test]
fn test_httpc_tsl() {
    let worker_pool = Box::new(WorkerPool::new("tset httpc".to_string(), WorkerType::Net, 8, 1024 * 1024, 30000, NET_WORKER_WALKER.clone()));
    worker_pool.run(NET_TASK_POOL.clone());

    let r = HttpClient::create(HttpClientOptions::VaildHost("".into(), "./apiclient_cert.p12".into(), "1516555961".into(), true, true, 10, 10000));
    if let Err(ref e) = r {
        panic!("create http client failed, e: {:?}", e);
    }
    let mut client = r.unwrap();

    let body = HttpClientBody::body("<xml><mch_appid>wx3d86fc2e76c0af41</mch_appid><mchid>1516555961</mchid><nonce_str>1757499962</nonce_str><partner_trade_no>0</partner_trade_no><openid>o-iCM1a6EwG8J4cM9Fu0snaoPIhY</openid><check_name>NO_CHECK</check_name><amount>30</amount><desc>wx_withdrawl</desc><spbill_create_ip>127.0.0.1</spbill_create_ip><sign>306188F52FB6072232DE5856D63D085E</sign></xml>".to_string());
    HttpClient::post(&mut client, Atom::from("https://api.mch.weixin.qq.com/mmpaymkttransfers/promotion/transfers"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                for key in resp.headers_keys().unwrap() {
                    match resp.get_header(key.clone()) {
                        None => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) if vec.len() == 0 => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) => {
                            print!("!!!!!!resp header, key: {}, value: {}", &*key, &*vec[0]);
                            for key_ in vec {
                                print!(" {:?}", &*key_);
                            }
                            println!("");
                        }
                    }
                }
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    thread::sleep(Duration::from_millis(30000));
}

#[test]
fn test_httpc_basic() {
    let worker_pool = Box::new(WorkerPool::new("tset httpc".to_string(), WorkerType::Net, 8, 1024 * 1024, 30000, NET_WORKER_WALKER.clone()));
    worker_pool.run(NET_TASK_POOL.clone());

    let r = HttpClient::create(HttpClientOptions::Default);
    assert!(r.is_ok());
    let mut client = r.unwrap();

    let body = HttpClientBody::body("asdfasdfasf".to_string());
    HttpClient::get(&mut client, Atom::from("http://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                for key in resp.headers_keys().unwrap() {
                    match resp.get_header(key.clone()) {
                        None => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) if vec.len() == 0 => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) => {
                            print!("!!!!!!resp header, key: {}, value: {}", &*key, &*vec[0]);
                            for key_ in vec {
                                print!(" {:?}", &*key_);
                            }
                            println!("");
                        }
                    }
                }
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    let body = HttpClientBody::body(vec![10, 10, 10]);
    HttpClient::get(&mut client, Atom::from("http://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                println!("!!!!!!resp body: {:?}", resp.bin());
            }
        }
    }));

    let r = File::open(r"E:\rust\pi\pi_net\httpc\tests\test.txt");
    assert!(r.is_ok());
    let file = r.unwrap();
    let body = HttpClientBody::body(file);
    HttpClient::get(&mut client, Atom::from("http://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    let mut json = HttpClientBody::json(Atom::from("x"), "Hello".to_string());
    json.add_json_kv(Atom::from("y"), "Hello".to_string());
    HttpClient::get(&mut client, Atom::from("http://www.baidu.com"), json, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    let mut form = HttpClientBody::form(Atom::from("x"), "Hello".to_string());
    form = form.add_form_kv(Atom::from("fileName"), "test.txt".to_string())
        .add_form_file(Atom::from("fileData"), r"E:\rust\pi\pi_net\httpc\tests\test.txt").unwrap();
    HttpClient::get(&mut client, Atom::from("http://www.baidu.com"), form, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    let r = HttpClient::create(HttpClientOptions::Normal(true, true, true, 10, 10000));
    if let Err(ref e) = r {
        panic!("create http client failed, e: {:?}", e);
    }
    let mut client = r.unwrap();

    let body = HttpClientBody::body("asdfasdfasf".to_string());
    HttpClient::get(&mut client, Atom::from("https://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                for key in resp.headers_keys().unwrap() {
                    match resp.get_header(key.clone()) {
                        None => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) if vec.len() == 0 => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) => {
                            print!("!!!!!!resp header, key: {}, value: {}", &*key, &*vec[0]);
                            for key_ in vec {
                                print!(" {:?}", &*key_);
                            }
                            println!("");
                        }
                    }
                }
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    let r = HttpClient::create(HttpClientOptions::VaildHost("./ca-cert.der".into(), "./client.p12".into(), "11111111".into(), true, true, 10, 10000));
    if let Err(ref e) = r {
        panic!("create http client failed, e: {:?}", e);
    }
    let mut client = r.unwrap();

    let body = HttpClientBody::body("asdfasdfasf".to_string());
    HttpClient::get(&mut client, Atom::from("https://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                for key in resp.headers_keys().unwrap() {
                    match resp.get_header(key.clone()) {
                        None => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) if vec.len() == 0 => println!("!!!!!!resp header, key: {}, value:", &*key),
                        Some(ref vec) => {
                            print!("!!!!!!resp header, key: {}, value: {}", &*key, &*vec[0]);
                            for key_ in vec {
                                print!(" {:?}", &*key_);
                            }
                            println!("");
                        }
                    }
                }
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    thread::sleep(Duration::from_millis(30000));
}