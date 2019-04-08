use std::sync::Arc;
use std::path::PathBuf;
use std::time::Instant;
use std::fs::create_dir_all;
use std::io::{Error as IOError, Result as IOResult, ErrorKind};

use http::StatusCode;
use hyper::body::Body;
use modifier::Set;
use npnc::ConsumeError;
use path_absolutize::*;

use worker::task::TaskType;
use worker::impls::cast_store_task;
use lib_file::file::{AsyncFileOptions, WriteOptions, Shared, AsyncFile, SharedFile};
use atom::Atom;
use apm::counter::{GLOBAL_PREF_COLLECT, PrefCounter, PrefTimer};

use Plugin;
use HttpResponse;
use request::Request;
use response::Response;
use handler::{HttpsError, HttpsResult, Handler};
use params::{Params, Value};

/*
* https文件上传任务优先级
*/
const HTTPS_ASYNC_FILE_UPLOAD_PRIORITY: usize = 100;

/*
* 文件移除方法标记
*/
const FILE_REMOVE_METHOD: &str = "_$remove";

lazy_static! {
    //http服务器文件上传处理器数量
    static ref HTTPS_UPLOAD_FILE_HANDLER_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_upload_file_handler_count"), 0).unwrap();
    //http服务器文件上传成功数量
    static ref HTTPS_UPLOAD_FILE_OK_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_upload_file_ok_count"), 0).unwrap();
    //http服务器文件上传失败数量
    static ref HTTPS_UPLOAD_FILE_ERROR_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_upload_file_error_count"), 0).unwrap();
    //http服务器文件上传字节数量
    static ref HTTPS_UPLOAD_FILE_BYTE_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_upload_file_byte_count"), 0).unwrap();
    //http服务器文件上传成功总时长
    static ref HTTPS_UPLOAD_FILE_OK_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_upload_file_ok_time"), 0).unwrap();
    //http服务器文件上传失败总时长
    static ref HTTPS_UPLOAD_FILE_ERROR_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_upload_file_error_time"), 0).unwrap();
    //http服务器文件移除成功数量
    static ref HTTPS_REMOVE_FILE_OK_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_remove_file_ok_count"), 0).unwrap();
    //http服务器文件移除失败数量
    static ref HTTPS_REMOVE_FILE_ERROR_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_remove_file_error_count"), 0).unwrap();
    //http服务器文件移除成功总时长
    static ref HTTPS_REMOVE_FILE_OK_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_remove_file_ok_time"), 0).unwrap();
    //http服务器文件移除失败总时长
    static ref HTTPS_REMOVE_FILE_ERROR_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_remove_file_error_time"), 0).unwrap();
}

/*
* 默认的http文件上传处理器
*/
#[derive(Clone)]
pub struct FileUpload {
    root: PathBuf,
}

impl Handler for FileUpload {
    fn handle(&self, mut req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        let start = HTTPS_UPLOAD_FILE_OK_TIME.start();

        if req.url.path().len() > 1 || req.url.path()[0] != "" {
            //无效的url路径
            HTTPS_UPLOAD_FILE_ERROR_TIME.timing(start);
            HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);

            return Some((req, res, 
                        Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, invalid url path")))));
        }

        let mut is_remove = false;
        let mut file = String::from("");
        let mut content = Vec::new();
        {
            let map = req.get_mut::<Params>().unwrap();
            if let Some(&Value::String(ref r)) = map.find(&["method"]) {
                if r == FILE_REMOVE_METHOD {
                    //文件移除
                    is_remove = true;
                    if let Some(Value::String(path)) = map.take(&["file_name"]) {
                        file = path;
                    } else {
                        HTTPS_REMOVE_FILE_ERROR_TIME.timing(start);
                        HTTPS_REMOVE_FILE_ERROR_COUNT.sum(1);

                        return Some((req, res, 
                                    Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "remove file error, empty relative path")))));
                    }
                }
            }
            if !is_remove {
                //不是文件移除，则为文件上传
                if let Some(Value::String(path)) = map.take(&["$file_name"]) {
                    file = path;
                } else {
                    HTTPS_UPLOAD_FILE_ERROR_TIME.timing(start);
                    HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);

                    return Some((req, res, 
                                Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, empty relative path")))));
                }
                if let Some(Value::Bin(bin)) = map.take(&["content"]) {
                    content = bin;
                }
            }
        }

        let file_path = PathBuf::from(&file);
        if !file_path.is_relative() {
            //不是相对路径
            HTTPS_UPLOAD_FILE_ERROR_TIME.timing(start);
            HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);

            return Some((req, res, 
                        Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, invalid relative path")))));
        }

        let path = self.root.join(file);
        if let Some(dir) = path.parent() {
            if let Ok(p) = dir.absolutize() {
                if !p.starts_with(PathBuf::from(&self.root).absolutize().ok().unwrap()) {
                    //标准化后根路径被改变
                    HTTPS_UPLOAD_FILE_ERROR_TIME.timing(start);
                    HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);

                    return Some((req, res, 
                                Err(HttpsError::new(IOError::new(ErrorKind::Other, "upload file error, absolute path overflow")))));
                }
            }
            if is_remove {
                if !path.exists() {
                    //文件不存在，则忽略
                    async_reply_ok(req, res, 0, start);
                } else {
                    async_remove_file(req, res, path, start);
                }
            } else {
                //不是移除文件
                if !dir.exists() {
                    //路径不存在
                    if create_dir_all(dir).is_err() {
                        //创建子目录失败
                        HTTPS_UPLOAD_FILE_ERROR_TIME.timing(start);
                        HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);

                        return Some((req, res, 
                                    Err(HttpsError::new(IOError::new(ErrorKind::Other, "upload file error, make relative path failed")))));
                    }
                }
                async_save_file(req, res, path, content, start);
            }
            None
        } else {
            //无效的绝对路径
            HTTPS_UPLOAD_FILE_ERROR_TIME.timing(start);
            HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);

            Some((req, res, 
                Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "upload file error, invalid absolute path")))))
        }
    }
}

//异步移除文件，并设置回应
fn async_remove_file(req: Request, res: Response, path: PathBuf, time: Instant) {
    let remove = Box::new(move |r: IOResult<()>| {
        match r {
            Err(e) => {
                //移除失败
                async_reply_error(req, res, e, 0, time);
            },
            Ok(_) => {
                //移除成功
                async_reply_ok(req, res, 0, time);
            },
        }
    });
    AsyncFile::remove(path, remove);
}

//异步存储文件，并设置回应
fn async_save_file(req: Request, res: Response, path: PathBuf, content: Vec<u8>, time: Instant) {
    let open = Box::new(move |f: IOResult<AsyncFile>| {
        match f {
            Err(e) => {
                //打开失败
                async_reply_error(req, res, e, content.len(), time);
            },
            Ok(r) => {
                //打开成功
                let content_size = content.len();
                let file = Arc::new(r);
                let write = Box::new(move |_: SharedFile, result: IOResult<usize>| {
                    match result {
                        Err(e) => {
                            //写失败
                            async_reply_error(req, res, e, content_size, time);
                        },
                        Ok(size) => {
                            //写成功
                            async_reply_ok(req, res, size, time);
                        },
                    }
                });
                file.pwrite(WriteOptions::Flush, 0, content, write);
            },
        }
    });
    AsyncFile::open(path, AsyncFileOptions::TruncateWrite(1), open);
}

//异步操作文件错误
fn async_reply_error(req: Request, mut res: Response, err: IOError, size: usize, time: Instant) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回错误
                    let func = Box::new(move |_lock| {
                        async_reply_error(req, res, err, size, time);
                    });
                    cast_store_task(TaskType::Async(false), HTTPS_ASYNC_FILE_UPLOAD_PRIORITY, None, func, Atom::from("async reply failed task"));
                },
                _ => {
                    if size == 0 {
                        HTTPS_REMOVE_FILE_ERROR_TIME.timing(time);
                        HTTPS_REMOVE_FILE_ERROR_COUNT.sum(1);
                    } else {
                        HTTPS_UPLOAD_FILE_ERROR_TIME.timing(time);
                        HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);
                    }

                    println!("!!!> Upload File Error, task wakeup failed, task id: {}, err: {:?}", req.uid, err)
                },
            }
        },
        Ok(waker) => {
            let sender = res.sender.as_ref().unwrap().clone();
            let mut http_res = HttpResponse::<Body>::new(Body::empty());
            res = res.set(StatusCode::INTERNAL_SERVER_ERROR);
            res.write_back(&mut http_res);
            sender.produce(Ok(http_res)).is_ok();
            waker.notify();

            if size == 0 {
                HTTPS_REMOVE_FILE_ERROR_TIME.timing(time);
                HTTPS_REMOVE_FILE_ERROR_COUNT.sum(1);
            } else {
                HTTPS_UPLOAD_FILE_ERROR_TIME.timing(time);
                HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);
            }
        },
    }
}

//异步操作文件成功
fn async_reply_ok(req: Request, mut res: Response, size: usize, time: Instant) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回成功
                    let func = Box::new(move |_lock| {
                        async_reply_ok(req, res, size, time);
                    });
                    cast_store_task(TaskType::Async(false), HTTPS_ASYNC_FILE_UPLOAD_PRIORITY, None, func, Atom::from("async reply ok task"));
                },
                _ => {
                    if size == 0 {
                        HTTPS_REMOVE_FILE_ERROR_TIME.timing(time);
                        HTTPS_REMOVE_FILE_ERROR_COUNT.sum(1);
                    } else {
                        HTTPS_UPLOAD_FILE_ERROR_TIME.timing(time);
                        HTTPS_UPLOAD_FILE_ERROR_COUNT.sum(1);
                    }

                    println!("!!!> Upload File Ok, task wakeup failed, task id: {}, e: {:?}", req.uid, e)
                },
            }
        },
        Ok(waker) => {
            let sender = res.sender.as_ref().unwrap().clone();
            let mut http_res = HttpResponse::<Body>::new(Body::empty());
            res = res.set(StatusCode::OK);
            res.write_back(&mut http_res);
            sender.produce(Ok(http_res)).is_ok();
            waker.notify();

            if size == 0 {
                HTTPS_REMOVE_FILE_OK_TIME.timing(time);
                HTTPS_REMOVE_FILE_OK_COUNT.sum(1);
            } else {
                HTTPS_UPLOAD_FILE_OK_TIME.timing(time);
                HTTPS_UPLOAD_FILE_BYTE_COUNT.sum(size);
                HTTPS_UPLOAD_FILE_OK_COUNT.sum(1);
            }
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

        HTTPS_UPLOAD_FILE_HANDLER_COUNT.sum(1);

        FileUpload {
            root: dir,
        }
    }
}

