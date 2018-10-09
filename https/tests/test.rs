#![feature(fnbox)]

extern crate pi_lib;
extern crate pi_base;

extern crate http;
extern crate modifier;

extern crate https;

use std::thread;

use http::StatusCode;

use modifier::Set;

use pi_lib::atom::Atom;
use pi_base::pi_base_impl::{STORE_TASK_POOL, EXT_TASK_POOL};
use pi_base::worker_pool::WorkerPool;

use https::Plugin;
use https::https_impl::start_http;
use https::request::Request;
use https::response::Response;
use https::handler::{HttpsResult, Handler, Chain};
use https::mount::Mount;
use https::file::StaticFile;
use https::files::StaticFileBatch;
use https::upload::FileUpload;
use https::params::{Params, Value};

#[test]
fn test_https() {
    let store_pool = Box::new(WorkerPool::new(10, 1024 * 1024, 10000));
    store_pool.run(STORE_TASK_POOL.clone());
    let ext_pool = Box::new(WorkerPool::new(10, 1024 * 1024, 10000));
    ext_pool.run(EXT_TASK_POOL.clone());
    
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