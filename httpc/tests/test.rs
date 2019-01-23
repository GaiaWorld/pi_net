#![feature(fnbox)]

extern crate atom;
extern crate worker;
extern crate httpc;

use std::thread;
use std::fs::File;
use std::io::Result;
use std::boxed::FnBox;

use atom::Atom;
use worker::worker_pool::WorkerPool;
use worker::impls::{NET_WORKER_WALKER, NET_TASK_POOL};

use httpc::{HttpClientOptions, SharedHttpc, SharedHttpClient, HttpClient, HttpClientBody, HttpClientResponse};

#[test]
fn test_httpc_basic() {
    let worker_pool = Box::new(WorkerPool::new(8, 1024 * 1024, 30000, NET_WORKER_WALKER.clone()));
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

    thread::sleep_ms(30000);
}