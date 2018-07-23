#![feature(fnbox)]

extern crate pi_lib;
extern crate pi_base;
extern crate httpc;

use std::thread;
use std::fs::File;
use std::io::Result;
use std::boxed::FnBox;

use pi_lib::atom::Atom;
use pi_base::worker_pool::WorkerPool;
use pi_base::pi_base_impl::EXT_TASK_POOL;

use httpc::{HttpClientOptions, SharedHttpc, SharedHttpClient, HttpClientBody, HttpClientResponse};

#[test]
fn test_httpc_basic() {
    let worker_pool = Box::new(WorkerPool::new(10, 1024 * 1024, 30000));
    worker_pool.run(EXT_TASK_POOL.clone());

    let r = SharedHttpClient::create(HttpClientOptions::Default);
    assert!(r.is_ok());
    let client = r.unwrap();

    let body = HttpClientBody::body("asdfasdfasf".to_string());
    client.get(Atom::from("http://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
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
    client.get(Atom::from("http://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                println!("!!!!!!resp body: {:?}", resp.bin());
            }
        }
    }));

    let r = File::open(r"E:\rust\git\pi_net\test.txt");
    assert!(r.is_ok());
    let file = r.unwrap();
    let body = HttpClientBody::body(file);
    client.get(Atom::from("http://www.baidu.com"), body, Box::new(move |_client: SharedHttpClient, result: Result<HttpClientResponse>| {
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
    client.get(Atom::from("http://www.baidu.com"), json, Box::new(move |client: SharedHttpClient, result: Result<HttpClientResponse>| {
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
        .add_form_file(Atom::from("fileData"), r"E:\rust\git\pi_net\test.txt").unwrap();
    client.get(Atom::from("http://www.baidu.com"), form, Box::new(move |client: SharedHttpClient, result: Result<HttpClientResponse>| {
        match result {
            Err(s) => println!("!!!!!!reason: {}", s),
            Ok(mut resp) => {
                println!("!!!!!!resp url: {}", *resp.url());
                println!("!!!!!!resp status: {}", resp.status());
                println!("!!!!!!resp body: {:?}", resp.text());
            }
        }
    }));

    thread::sleep_ms(30000);
}