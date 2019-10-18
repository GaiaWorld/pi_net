use std::io;
use std::fs;
use std::fmt;
use std::sync::Arc;
use std::error::Error;
use std::time::{Instant, Duration};
use std::path::PathBuf;
use std::io::{Error as IOError, Result as IOResult};

use url;
use http::header::HeaderName;
use http::{HttpTryFrom, StatusCode, HeaderMap};
use hyper::body::Body;
use modifier::Set;
use modifier::Modifier;
use npnc::ConsumeError;

use worker::task::TaskType;
use worker::impls::cast_store_task;
use lib_file::file::{AsyncFileOptions, Shared, AsyncFile, SharedFile};
use atom::Atom;
use apm::counter::{GLOBAL_PREF_COLLECT, PrefCounter, PrefTimer};

use request::{Url, Request};
use response::Response;
use mount::OriginalUrl;
use file_path::RequestedPath;
use modifiers::{Redirect, mime_for_path};
use headers;
use handler::{HttpsError, HttpsResult, Handler};

use HttpResponse;

/*
* https文件加载异步任务优先级
*/
const HTTPS_ASYNC_FILE_LOAD_PRIORITY: usize = 100;

//文件异步打开失败
pub const HTTPS_ASYNC_OPEN_FILE_FAILED_STATUS: u16 = 515;
//文件异步读取失败
pub const HTTPS_ASYNC_READ_FILE_FAILED_STATUS: u16 = 516;

lazy_static! {
    //http服务器文件加载处理器数量
    static ref HTTPS_LOAD_FILE_HANDLER_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_load_file_handler_count"), 0).unwrap();
    //http服务器文件加载成功数量
    static ref HTTPS_LOAD_FILE_OK_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_load_file_ok_count"), 0).unwrap();
    //http服务器文件加载失败数量
    static ref HTTPS_LOAD_FILE_ERROR_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_load_file_error_count"), 0).unwrap();
    //http服务器文件加载字节数量
    static ref HTTPS_LOAD_FILE_BYTE_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_load_file_byte_count"), 0).unwrap();
    //http服务器文件加载成功总时长
    static ref HTTPS_LOAD_FILE_OK_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_load_file_ok_time"), 0).unwrap();
    //http服务器文件加载失败总时长
    static ref HTTPS_LOAD_FILE_ERROR_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_load_file_error_time"), 0).unwrap();
}

/*
* 文件未找到
*/
#[derive(Debug)]
pub struct NoFile;

impl Error for NoFile {
    fn description(&self) -> &str { "File not found" }
}

impl fmt::Display for NoFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.description())
    }
}

/*
* 简单基于时间的文件缓存
*/
#[derive(Clone)]
pub struct Cache {
    pub duration: Duration,
}

impl Modifier<StaticFile> for Cache {
    fn modify(self, handler: &mut StaticFile) {
        handler.cache = Some(self);
    }
}

/**
* 有缓存的静态资源文件
*/
#[derive(Clone)]
pub struct StaticFile {
    root: PathBuf,
    gen_res_headers: HeaderMap,
    cache: Option<Cache>,
}

impl Set for StaticFile {}

impl Handler for StaticFile {
    fn handle(&self, req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        let start = HTTPS_LOAD_FILE_OK_TIME.start();

        let requested_path = RequestedPath::new(&self.root, &req);
        let path = &requested_path.path.clone();
        let metadata = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) => {
                HTTPS_LOAD_FILE_ERROR_TIME.timing(start);
                HTTPS_LOAD_FILE_ERROR_COUNT.sum(1);

                let status = match e.kind() {
                    io::ErrorKind::NotFound => StatusCode::NOT_FOUND,   //文件未找到
                    io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,   //禁止访问文件
                    _ => StatusCode::INTERNAL_SERVER_ERROR, //服务器内部错误
                };
                return Some((req, res.set(status), 
                            Err(HttpsError::new(io::Error::new(e.kind(), 
                                format!("load file error, path: {:?}, e: {}", path, e.to_string()))))));
            },
        };

        //如果url以/结束，则需要提供文件，否则重定向到与url对应的目录
        if requested_path.should_redirect(&metadata, &req) {
            //执行301重定向
            let mut original_url: url::Url = match req.extensions.get::<OriginalUrl>() {
                None => &req.url,
                Some(original_url) => original_url,
            }.clone().into();

            //rust-url自动将路径中最后一个槽中的空字符串转换为尾部斜杠
            original_url.path_segments_mut().unwrap().push("");
            let redirect_path = Url::from_generic_url(original_url).unwrap();

            return Some((req, res.set((StatusCode::MOVED_PERMANENTLY,
                                      format!("Redirecting to {}", redirect_path.format()),
                                      Redirect(redirect_path))), Ok(())));
        }

        match requested_path.get_file(&metadata) {
            None => {
                HTTPS_LOAD_FILE_ERROR_TIME.timing(start);
                HTTPS_LOAD_FILE_ERROR_COUNT.sum(1);

                Some((req, res.set(StatusCode::NOT_FOUND),
                      Err(HttpsError::new(io::Error::new(io::ErrorKind::NotFound,
                                                         format!("load file error, get file metadata failed, path: {:?}", path))))))
            },    //文件未找到
            Some(path) => {
                //异步加载指定文件
                async_load_file(req, self.fill_gen_resp_headers(res), path, start);
                None
            },
        }
    }
}

impl StaticFile {
    /**
    * 指定文件根目录，构建指定的静态资源文件，可以是绝对路径或相对路径，如果为空串，则表示以当前运行时路径作为文件根目录
    * @param root 静态资源文件所在根路径
    * @returns 返回有缓存的静态资源文件
    */
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        HTTPS_LOAD_FILE_HANDLER_COUNT.sum(1);

        StaticFile {
            root: root.into(),
            gen_res_headers: HeaderMap::new(),
            cache: None
        }
    }

    /**
    * 增加静态资源访问时的指定通用响应头
    * @param key 关键字
    * @param value 值
    * @returns 返回通用响应头数量
    */
    pub fn add_gen_resp_header(&mut self, key: &str, value: &str) -> usize {
        match HeaderName::try_from(key) {
            Err(e) => panic!("add gen response header failed, key: {:?}, value: {:?}, e: {:?}", key, value, e),
            Ok(k) => {
                self.gen_res_headers.append(k, (&value).parse().unwrap());
                self.gen_res_headers.len()
            },
        }
    }

    /**
    * 移除静态资源访问时的指定通用响应头
    * @param key 关键字
    * @returns 返回通用响应头数量
    */
    pub fn remove_gen_resp_header(&mut self, key: &str) -> usize {
        match HeaderName::try_from(key) {
            Err(e) => panic!("remove gen response header failed, key: {:?}, e: {:?}", key, e),
            Ok(k) => {
                self.gen_res_headers.remove(k);
                self.gen_res_headers.len()
            },
        }
    }

    //填充通用响应头
    fn fill_gen_resp_headers(&self, mut res: Response) -> Response {
        for (key, value) in self.gen_res_headers.iter() {
            res.headers.append(key.clone(), value.clone());
        }
        res
    }
}

//异步加载指定文件，并设置回应
fn async_load_file(req: Request, res: Response, file_path: PathBuf, time: Instant) {
    let path = file_path.clone();
    let open = Box::new(move |f: IOResult<AsyncFile>| {
        match f {
            Err(e) => {
                //打开失败
                async_load_file_error(req, res, e, HTTPS_ASYNC_OPEN_FILE_FAILED_STATUS, time);
            },
            Ok(r) => {
                //打开成功
                let file = Arc::new(r);
                let size = file.get_size();
                let read = Box::new(move |_: SharedFile, result: IOResult<Vec<u8>>| {
                    match result {
                        Err(e) => {
                            //读失败
                            async_load_file_error(req, res, e, HTTPS_ASYNC_READ_FILE_FAILED_STATUS, time);
                        },
                        Ok(data) => {
                            //读成功
                            async_load_file_ok(req, res, path, size, data, time);
                        },
                    }
                });
                file.pread(0, size as usize, read);
            },
        }
    });
    AsyncFile::open(file_path, AsyncFileOptions::OnlyRead(1), open);
}

//异步加载文件错误
fn async_load_file_error(req: Request, mut res: Response, err: IOError, err_no: u16, time: Instant) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回错误
                    let func = Box::new(move |_lock| {
                        async_load_file_error(req, res, err, err_no, time);
                    });
                    cast_store_task(TaskType::Async(false), HTTPS_ASYNC_FILE_LOAD_PRIORITY, None, func, Atom::from("async load file failed task"));
                },
                _ => {
                    HTTPS_LOAD_FILE_ERROR_TIME.timing(time);
                    HTTPS_LOAD_FILE_ERROR_COUNT.sum(1);

                    warn!("!!!> Https Async Load File Error, task wakeup failed, task id: {}, err: {:?}, e: {:?}", req.uid, err, e)
                },
            }
        },
        Ok(waker) => {
            let sender = res.sender.as_ref().unwrap().clone();
            let mut http_res = HttpResponse::<Body>::new(Body::empty());
            res = res.set(StatusCode::from_u16(err_no).ok().unwrap());
            res.write_back(&mut http_res);
            sender.produce(Ok(http_res)).is_ok();
            waker.notify();

            HTTPS_LOAD_FILE_ERROR_TIME.timing(time);
            HTTPS_LOAD_FILE_ERROR_COUNT.sum(1);
        },
    }
}

//异步加载文件成功
fn async_load_file_ok(req: Request, mut res: Response, path: PathBuf, size: u64, data: Vec<u8>, time: Instant) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回成功
                    let func = Box::new(move |_lock| {
                        async_load_file_ok(req, res, path, size, data, time);
                    });
                    cast_store_task(TaskType::Async(false), HTTPS_ASYNC_FILE_LOAD_PRIORITY, None, func, Atom::from("async load file ok task"));
                },
                _ => {
                    HTTPS_LOAD_FILE_ERROR_TIME.timing(time);
                    HTTPS_LOAD_FILE_ERROR_COUNT.sum(1);

                    warn!("!!!> Https Async Load File Ok, task wakeup failed, task id: {}, e: {:?}", req.uid, e)
                },
            }
        },
        Ok(waker) => {
            let sender = res.sender.as_ref().unwrap().clone();
            let mut http_res = HttpResponse::<Body>::new(Body::empty());
            res = res.set(StatusCode::OK);
            let mime = mime_for_path(path.as_path());
            res.set_mut(mime);
            res.headers.insert(headers::CONTENT_LENGTH, size.into());
            res.body = Some(Box::new(data));
            res.write_back(&mut http_res);
            sender.produce(Ok(http_res)).is_ok();
            waker.notify();

            HTTPS_LOAD_FILE_OK_TIME.timing(time);
            HTTPS_LOAD_FILE_BYTE_COUNT.sum(size as usize);
            HTTPS_LOAD_FILE_OK_COUNT.sum(1);
        },
    }
}