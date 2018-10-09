use std::sync::Arc;
use std::path::PathBuf;
use std::fs::create_dir_all;
use std::io::{Error as IOError, Result as IOResult, ErrorKind};

use http::StatusCode;
use hyper::body::Body;
use modifier::Set;
use npnc::ConsumeError;
use path_absolutize::*;

use pi_base::task::TaskType;
use pi_base::pi_base_impl::cast_store_task;
use pi_base::file::{AsynFileOptions, WriteOptions, Shared, AsyncFile, SharedFile};
use pi_lib::atom::Atom;

use Plugin;
use HttpResponse;
use request::Request;
use response::Response;
use handler::{HttpsError, HttpsResult, Handler};
use params::{Params, Value};

/*
* 默认的http文件上传处理器
*/
#[derive(Clone)]
pub struct FileUpload {
    root: PathBuf,
}

impl Handler for FileUpload {
    fn handle(&self, mut req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        if req.url.path().len() > 1 || req.url.path()[0] != "" {
            //无效的url路径，则忽略
            return Some((req, res, 
                        Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, invalid url path")))));
        }

        let file: String;
        let mut content = Vec::new();
        {
            let map = req.get_ref::<Params>().unwrap();
            if let Some(&Value::String(ref path)) = map.find(&["$file_name"]) {
                file = path.clone();
            } else {
                return Some((req, res, 
                            Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, empty relative path")))));
            }
            if let Some(&Value::Bin(ref bin)) = map.find(&["content"]) {
                content = bin.to_vec();
            }
        }

        let file_path = PathBuf::from(&file);
        if !file_path.is_relative() {
            //不是相对路径，则返回错误
            return Some((req, res, 
                        Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, invalid relative path")))));
        }

        let path = self.root.join(file);
        if let Some(dir) = path.parent() {
            if let Ok(p) = dir.absolutize() {
                if !p.starts_with(PathBuf::from(&self.root).absolutize().ok().unwrap()) {
                    //标准化后根路径被改变，则返回错误
                    return Some((req, res, 
                                Err(HttpsError::new(IOError::new(ErrorKind::Other, "upload file error, absolute path overflow")))));
                }
            }
            if !dir.exists() {
                //路径不存在，则创建子目录
                if create_dir_all(dir).is_err() {
                    //创建子目录失败
                    return Some((req, res, 
                                Err(HttpsError::new(IOError::new(ErrorKind::Other, "upload file error, make relative path failed")))));
                }
            }
            async_save_file(req, res, path, content);
            None
        } else {
            //无效的绝对路径
            Some((req, res, 
                Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, invalid absolute path")))))
        }
    }
}

//异步存储文件，并设置回应
fn async_save_file(req: Request, res: Response, path: PathBuf, content: Vec<u8>) {
    let open = Box::new(move |f: IOResult<AsyncFile>| {
        match f {
            Err(e) => {
                //打开失败
                async_save_files_error(req, res, e);
            }
            Ok(r) => {
                //打开成功
                let file = Arc::new(r);
                let write = Box::new(move |_: SharedFile, result: IOResult<usize>| {
                    match result {
                        Err(e) => {
                            //写失败
                            async_save_files_error(req, res, e);
                        },
                        Ok(_size) => {
                            //写成功
                            async_save_files_ok(req, res);
                        },
                    }
                });
                file.pwrite(WriteOptions::Flush, 0, content, write);
            },
        }
    });
    AsyncFile::open(path, AsynFileOptions::OnlyAppend(1), open);
}

//异步存储文件错误
fn async_save_files_error(req: Request, mut res: Response, err: IOError) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回错误
                    let func = Box::new(move || {
                        async_save_files_error(req, res, err);
                    });
                    cast_store_task(TaskType::Sync, 3000003, func, Atom::from("async save files failed task"));
                },
                _ => println!("!!!> Https Async Save Files Error, task wakeup failed, task id: {}, err: {:?}", req.uid, err),
            }
        },
        Ok(waker) => {
            let sender = res.sender.as_ref().unwrap().clone();
            let mut http_res = HttpResponse::<Body>::new(Body::empty());
            res = res.set(StatusCode::INTERNAL_SERVER_ERROR);
            res.write_back(&mut http_res);
            sender.produce(Ok(http_res)).is_ok();
            waker.notify();
        },
    }
}

//异步存储文件成功
fn async_save_files_ok(req: Request, mut res: Response) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回成功
                    let func = Box::new(move || {
                        async_save_files_ok(req, res);
                    });
                    cast_store_task(TaskType::Sync, 3000003, func, Atom::from("async save files ok task"));
                },
                _ => println!("!!!> Https Async Save Files Ok, task wakeup failed, task id: {}, e: {:?}", req.uid, e),
            }
        },
        Ok(waker) => {
            let sender = res.sender.as_ref().unwrap().clone();
            let mut http_res = HttpResponse::<Body>::new(Body::empty());
            res = res.set(StatusCode::OK);
            res.write_back(&mut http_res);
            sender.produce(Ok(http_res)).is_ok();
            waker.notify();
        },
    }
}

impl FileUpload {
    //构建文件上传处理器
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        let dir = root.into();
        if !dir.exists() {
            //不存在，则创建根目录
            if create_dir_all(&dir).is_err() {
                //创建根目录失败
                panic!("!!!> New FileUpload Failed, make root failed, root: {:?}", dir);
            }
        }

        FileUpload {
            root: dir,
        }
    }
}