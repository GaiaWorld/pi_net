#![feature(fnbox)]

extern crate atom;
extern crate worker;
extern crate file as lib_file;
extern crate future;

extern crate http;
extern crate modifier;

extern crate https;

use std::thread;

use http::StatusCode;

use modifier::Set;

use atom::Atom;
use worker::worker_pool::WorkerPool;
use worker::impls::{STORE_WORKER_WALKER, NET_WORKER_WALKER, STORE_TASK_POOL, NET_TASK_POOL};

use https::Plugin;
use https::https_impl::{start_https, start_http};
use https::request::Request;
use https::response::Response;
use https::handler::{HttpsResult, Handler};
use https::mount::Mount;
use https::file::StaticFile;
use https::files::StaticFileBatch;
use https::upload::FileUpload;
use https::params::{Params, Value};

#[test]
fn test_http_server() {
    let store_pool = Box::new(WorkerPool::new(8, 1024 * 1024, 10000, STORE_WORKER_WALKER.clone()));
    store_pool.run(STORE_TASK_POOL.clone());
    let ext_pool = Box::new(WorkerPool::new(8, 1024 * 1024, 10000, NET_WORKER_WALKER.clone()));
    ext_pool.run(NET_TASK_POOL.clone());
    
    struct Test;

    impl Handler for Test {
        fn handle(&self, mut req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
            {
                let map = req.get_ref::<Params>().unwrap();
                println!("!!!!!!params: {:?}", map);
                if let Some(Value::String(n)) = map.find(&["n"]) {
                    if let Some(Value::String(f)) = map.find(&["f"]) {
                        println!("!!!!!!n: {}, f: {}", n, f);
                    }
                }
            }
            Some((req, res.set((StatusCode::OK, "Hello World")), Ok(())))
        }
    }

    println!("!!!!!!https starting...");
    // let mut options = FilesAccOptions::new();
    // options.add_route(Atom::from("/"), Atom::from("./htdocs/"));
    // options.add_route(Atom::from("/app/"), Atom::from("./app/"));
    // options.add_route(Atom::from("/"), Atom::from("/app/"));
    let mut mount = Mount::new();
    mount.mount("/fs", StaticFileBatch::new("./app/"));
    mount.mount("/test", Test); //只支持前缀匹配，/表示匹配所有
    mount.mount("/upload", FileUpload::new("./upload/"));
    mount.mount("/", StaticFile::new("./htdocs/"));
    mount.mount("/app", StaticFile::new("./app/"));
    start_http(mount, Atom::from("0.0.0.0"), 80, 5000, 10000);
    loop {
        thread::sleep_ms(30000);
    }
}

#[test]
fn test_https_server() {
    let store_pool = Box::new(WorkerPool::new(8, 1024 * 1024, 10000, STORE_WORKER_WALKER.clone()));
    store_pool.run(STORE_TASK_POOL.clone());
    let ext_pool = Box::new(WorkerPool::new(8, 1024 * 1024, 10000, NET_WORKER_WALKER.clone()));
    ext_pool.run(NET_TASK_POOL.clone());

    struct Test;

    impl Handler for Test {
        fn handle(&self, mut req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
            {
                let map = req.get_ref::<Params>().unwrap();
                println!("!!!!!!params: {:?}", map);
                if let Some(Value::String(n)) = map.find(&["n"]) {
                    if let Some(Value::String(f)) = map.find(&["f"]) {
                        println!("!!!!!!n: {}, f: {}", n, f);
                    }
                }
            }
            Some((req, res.set((StatusCode::OK, "Hello World")), Ok(())))
        }
    }

    println!("!!!!!!https starting...");
    // let mut options = FilesAccOptions::new();
    // options.add_route(Atom::from("/"), Atom::from("./htdocs/"));
    // options.add_route(Atom::from("/app/"), Atom::from("./app/"));
    // options.add_route(Atom::from("/"), Atom::from("/app/"));
    let mut mount = Mount::new();
    let mut fs = StaticFileBatch::new("./app/");
    fs.add_gen_resp_header("Access-Control-Allow-Origin", "*");
    let mut root = StaticFile::new("./htdocs_tls/");
    root.add_gen_resp_header("Access-Control-Allow-Origin", "*");
    let mut app = StaticFile::new("./app/");
    app.add_gen_resp_header("Access-Control-Allow-Origin", "*");
    mount.mount("/fs", fs);
    mount.mount("/test", Test); //只支持前缀匹配，/表示匹配所有
    mount.mount("/upload", FileUpload::new("./upload_tls/"));
    mount.mount("/", root);
    mount.mount("/app", app);
    start_https(mount, Atom::from("0.0.0.0"), 443, 5000, 10000, Atom::from("./cert.crt"), Atom::from("./rsa_aes_private.key"));
    loop {
        thread::sleep_ms(30000);
    }
}