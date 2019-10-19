use std::sync::Arc;
use std::str::Chars;
use std::ffi::OsStr;
use std::fs::DirEntry;
use std::time::Instant;
use std::path::{Path, PathBuf};
use std::io::{Error as IOError, Result as IOResult, ErrorKind};

use http::header::HeaderName;
use http::{HttpTryFrom, StatusCode, HeaderMap};
use hyper::body::Body;
use modifier::Set;
use npnc::ConsumeError;
use bytes::BufMut;

use worker::task::TaskType;
use worker::impls::cast_store_task;
use lib_file::file::{AsyncFileOptions, Shared, AsyncFile, SharedFile};
use atom::Atom;
use apm::counter::{GLOBAL_PREF_COLLECT, PrefCounter, PrefTimer};

use mime;
use Plugin;
use headers;
use HttpResponse;
use request::Request;
use response::Response;
use handler::{HttpsError, HttpsResult, Handler};
use params::{Params, Value};
use file::{HTTPS_ASYNC_OPEN_FILE_FAILED_STATUS, HTTPS_ASYNC_READ_FILE_FAILED_STATUS};

/*
* https文件加载异步任务优先级
*/
const HTTPS_ASYNC_FILE_LOAD_PRIORITY: usize = 100;

lazy_static! {
    //http服务器批量文件加载处理器数量
    static ref HTTPS_BATCH_LOAD_FILE_HANDLER_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_batch_load_file_handler_count"), 0).unwrap();
    //http服务器批量文件加载成功数量
    static ref HTTPS_BATCH_LOAD_FILE_OK_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_batch_load_file_ok_count"), 0).unwrap();
    //http服务器批量文件加载失败数量
    static ref HTTPS_BATCH_LOAD_FILE_ERROR_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_batch_load_file_error_count"), 0).unwrap();
    //http服务器批量文件加载字节数量
    static ref HTTPS_BATCH_LOAD_FILE_BYTE_COUNT: PrefCounter = GLOBAL_PREF_COLLECT.new_static_counter(Atom::from("https_batch_load_file_byte_count"), 0).unwrap();
    //http服务器批量文件加载成功总时长
    static ref HTTPS_BATCH_LOAD_FILE_OK_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_batch_load_file_ok_time"), 0).unwrap();
    //http服务器批量文件加载失败总时长
    static ref HTTPS_BATCH_LOAD_FILE_ERROR_TIME: PrefTimer = GLOBAL_PREF_COLLECT.new_static_timer(Atom::from("https_batch_load_file_error_time"), 0).unwrap();
}

/**
* 批量静态资源文件
*/
#[derive(Clone)]
pub struct FileBatch {
    root: PathBuf,
    gen_res_headers: HeaderMap,
    parse_files: fn(Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>),
}

impl Set for FileBatch {}

impl Handler for FileBatch {
    fn handle(&self, mut req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        let start = HTTPS_BATCH_LOAD_FILE_OK_TIME.start();


        let mut ds = "";
        let mut fs = "";
        let mut rpath = PathBuf::new();
        let mut size = 0;
        {
            let map = req.get_ref::<Params>().unwrap();
            if let Some(Value::String(r)) = map.find(&["r"]) {
                rpath.push(&self.root);
                rpath.push(r.as_str());
            }
            if let Some(Value::String(d)) = map.find(&["d"]) {
                ds = d.as_str();
            }
            if let Some(Value::String(f)) = map.find(&["f"]) {
                fs = f.as_str();
            }
            if let Some(Value::String(s)) = map.find(&["s"]) {
                size = s.parse().or::<usize>(Ok(0)).unwrap();
            }
        }
        let root = if rpath.as_path().to_str().unwrap().as_bytes().len() > 0 {
            &rpath
        }else{
            &self.root
        };
        warn!("---------------------batch, {:?}", root);
        let mut dir_vec: Vec<(u64, PathBuf)> = Vec::new();
        let mut dirs: Vec<String> = Vec::new();
        let mut file_vec: Vec<(u64, PathBuf)>;
        let mut files: Vec<String> = Vec::new();
        match decode(&mut ds.chars(), &mut vec![], &mut vec![], &mut dirs, 0) {
            Err(pos) => {
                HTTPS_BATCH_LOAD_FILE_ERROR_TIME.timing(start);
                HTTPS_BATCH_LOAD_FILE_ERROR_COUNT.sum(1);

                let desc = format!("load batch file error, decode dir failed, dir: {}, pos: {}", ds, pos);
                return Some((req, res, Err(HttpsError::new(IOError::new(ErrorKind::NotFound, desc)))));
            },
            Ok(_) => {
                decode_dir(self.parse_files, &mut dirs, root, &mut dir_vec);
            },
        }
        match decode(&mut fs.chars(), &mut vec![], &mut vec![], &mut files, 0) {
            Err(pos) => {
                HTTPS_BATCH_LOAD_FILE_ERROR_TIME.timing(start);
                HTTPS_BATCH_LOAD_FILE_ERROR_COUNT.sum(1);

                let desc = format!("load batch file error, decode file failed, file: {}, pos: {}", fs, pos);
                return Some((req, res, Err(HttpsError::new(IOError::new(ErrorKind::NotFound, desc)))));
            },
            Ok(_) => {
                file_vec = files.into_iter().map(|file| {
                        let path = root.join(file);
                        if let Ok(meta) = path.metadata() {
                            (meta.len(), path)
                        } else {
                            (0, path)
                        }
                    }).collect();
            },
        }

        //合并解析的所有文件，并异步加载所有文件
        dir_vec.append(&mut file_vec);
        // println!("!!!!!!files: {:?}", dir_vec.to_vec().iter_mut().map(|(_, x)| x).collect::<Vec<&mut PathBuf>>());
        async_load_files(req, self.fill_gen_resp_headers(res), dir_vec, root.to_str().unwrap().as_bytes().len(), 0, Vec::new(), 0, start);
        None
    }
}

impl FileBatch {
    /**
    * 指定文件根目录，构建指定的批量静态资源文件，可以是绝对路径或相对路径，如果为空串，则表示以当前运行时路径作为文件根目录
    * @param root 批量静态资源文件所在根路径
    * @returns 返回批量静态资源文件
    */
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        HTTPS_BATCH_LOAD_FILE_HANDLER_COUNT.sum(1);

        FileBatch {
            root: root.into(),
            gen_res_headers: HeaderMap::new(),
            parse_files: disk_files,
        }
    }

    //指定文件根目录和递归解析文件函数，构建指定的批量静态资源文件，可以是绝对路径或相对路径，如果为空串，则表示以当前运行时路径作为文件根目录
    pub fn with<P: Into<PathBuf>>(root: P, func: fn(Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>)) -> Self {
        FileBatch {
            root: root.into(),
            gen_res_headers: HeaderMap::new(),
            parse_files: func,
        }
    }

    /**
    * 增加批量静态资源文件访问时的指定通用响应头
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
    * 移除指定通用响应头
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

//解析后缀，失败返回在解析哪个字符时出错
fn decode(chars: &mut Chars, suffix: &mut Vec<char>, stack: &mut Vec<String>, result: &mut Vec<String>, mut pos: usize) -> Result<(), usize> {
    loop {
        pos += 1;
        let mut char = chars.next();

        if let None = char {
            //解析完成
            return Ok(());
        }

        if let Some('$') = char {
            //转义后推入后缀栈
            pos += 1;
            char = chars.next();

            if let Some('$') = char {
                suffix.push('$');
                continue;
            } else if let Some('1') = char {
                suffix.push('(');
                continue;
            } else if let Some('2') = char {
                suffix.push(')');
                continue;
            } else {
                return Err(pos);
            }
        }

        if let Some('(') = char {
            if suffix.len() == 0 {
                //无后缀继续解析文件
                return decode_file(chars, suffix, stack, result, pos);
            } else {
                //有后缀继续解析文件
                suffix.insert(0, '.');
                return decode_file(chars, suffix, stack, result, pos);
            }
        }

        if let Some(':') = char {
            //继续解析路径
            return decode_path(chars, stack, result, pos);
        }

        if let Some(c) = char {
            //其它字符推入后缀栈
            suffix.push(c);
            continue;
        }
    }
}

//解析指定后缀的文件
fn decode_file(chars: &mut Chars, suffix: &mut Vec<char>, stack: &mut Vec<String>, result: &mut Vec<String>, mut pos: usize) -> Result<(), usize> {
    let name: &mut Vec<char> = &mut vec![];
    
    loop {
        pos += 1;
        let mut char = chars.next();

        if let None = char {
            //解析文件完成
            return Ok(());
        }

        if let Some('$') = char {
            //转义后推入后缀栈
            pos += 1;
            char = chars.next();

            if let Some('$') = char {
                suffix.push('$');
                continue;
            } else if let Some('1') = char {
                suffix.push('(');
                continue;
            } else if let Some('2') = char {
                suffix.push(')');
                continue;
            } else {
                return Err(pos);
            }
        }

        if let Some(':') = char {
            //继续解析下一个文件
            let path_file = stack.last()
                                .or(Some(&mut "".to_string()))
                                .unwrap().clone() 
                                + name.to_vec().into_iter().collect::<String>().as_str() 
                                + suffix.to_vec().into_iter().collect::<String>().as_str(); //构建一个指定路径、文件名和后缀名的文件路径
            result.push(path_file); //推入结果集
            name.clear(); //清空文件名
            continue;
        }

        if let Some(')') = char {
            //文件解析完成，继续下一个解析
            let path_file = stack.last()
                                .or(Some(&mut "".to_string()))
                                .unwrap().clone() 
                                + name.to_vec().into_iter().collect::<String>().as_str() 
                                + suffix.to_vec().into_iter().collect::<String>().as_str(); //构建一个指定路径、文件名和后缀名的文件路径
            result.push(path_file); //推入结果集
            suffix.clear(); //清空后缀名
            return decode(chars, suffix, stack, result, pos);
        }

        if let Some(c) = char {
            //其它文件名字符推入文件名
            name.push(c);
            continue;
        }
    }
}

//解析文件路径
fn decode_path(chars: &mut Chars, stack: &mut Vec<String>, result: &mut Vec<String>, mut pos: usize) -> Result<(), usize> {
    let path: &mut Vec<char> = &mut vec![];

    loop {
        pos += 1;
        let mut char = chars.next();

        if let None = char {
            //解析路径完成
            return Ok(());
        }

        if let Some('$') = char {
            //转义后推入后缀栈
            pos += 1;
            char = chars.next();

            if let Some('$') = char {
                path.push('$');
                continue;
            } else if let Some('1') = char {
                path.push('(');
                continue;
            } else if let Some('2') = char {
                path.push(')');
                continue;
            } else {
                return Err(pos);
            }
        }

        if let Some('(') = char {
            //继续解析后缀
            let p = stack.last()
                        .or(Some(&mut "".to_string()))
                        .unwrap().clone() 
                        + path.to_vec().into_iter().collect::<String>().as_str() //构建一个路径
                        + "/";
            stack.push(p); //推入路径栈
            path.clear(); //清空路径
            return decode(chars, &mut vec![], stack, result, pos);
        }

        if let Some(')') = char {
            //当前路径解析完成，继续分析下一个路径
            stack.pop(); //弹出已分析完成的路径
            path.clear(); //清空路径
            continue;
        }

        if let Some(c) = char {
            //其它字符推入路径
            path.push(c);
            continue;
        }
    }
}

//解析指定目录下指定后缀的文件，没有后缀即目录下所有文件
fn decode_dir(parse_files: fn(Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>), 
    dirs: &mut Vec<String>, root: &PathBuf, result: &mut Vec<(u64, PathBuf)>) {
        let mut index: usize;
        let mut len: usize;
        for dir in dirs {
            let path = root.join(dir);
            //同path下有同名目录和文件，则会解析异常
            if !path.is_file() && !path.is_dir() {
                //解析目录下指定后缀的文件，形如*/.*和*/*.*
                if let Some(s) = path.file_name() {
                    if let Some(str) = s.to_str() {
                        let vec: Vec<&str> = str.split(".").collect();
                        if vec.len() > 1 {
                            index = result.len();
                            parse_files(Some(&OsStr::new(vec[1])), &path.parent().unwrap().to_path_buf().join(vec[0]), result);
                            len = result.len();
                            result[index..len].sort_by(|(_, x), (_, y)| x.to_str().as_mut().unwrap().replace("\\", "/").cmp(&y.to_str().as_mut().unwrap().replace("\\", "/")));
                            continue;
                        }
                    }
                }
                index = result.len();
                parse_files(None, &path.parent().unwrap().to_path_buf(), result);
                len = result.len();
                result[index..len].sort_by(|(_, x), (_, y)| x.to_str().as_ref().unwrap().replace("\\", "/").cmp(&y.to_str().as_ref().unwrap().replace("\\", "/")));
            } else {
                //解析目录下所有文件，形如*/
                index = result.len();
                parse_files(None, &path, result);
                len = result.len();
                result[index..len].sort_by(|(_, x), (_, y)| x.to_str().as_ref().unwrap().replace("\\", "/").cmp(&y.to_str().as_ref().unwrap().replace("\\", "/")));
            }
        }
}

//从硬盘上递归读取文件名和大小
fn disk_files(suffix: Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>) {
    if !path.exists() {
        return;
    }

    match path.read_dir() {
        Err(e) => {
            panic!("select disk files error, path: {:?}, reason: {:?}", path, e);
        },
        Ok(dirs) => {
            for dir in dirs {
                if let Ok(entry) = dir {
                    if let Ok(file_type) = entry.file_type() {
                        if file_type.is_dir() {
                            //是目录
                            disk_files(suffix, &entry.path(), result);
                        } else {
                            //是链接或文件
                            filter_file(suffix, entry, result);
                        }
                    }
                }
            }
        },
    }
}

//过滤指定后缀和大小的文件
fn filter_file(suffix: Option<&OsStr>, entry: DirEntry, result: &mut Vec<(u64, PathBuf)>) {
    if let Some(s0) = suffix {
        //过滤指定后缀名的文件
        match Path::new(&entry.file_name()).extension() {
            Some(s1) if s0 == s1 => {
                //后缀名相同，形如*.*的文件
                if let Ok(meta) = entry.metadata() {
                    result.push((meta.len(), entry.path()));
                } else {
                    result.push((0, entry.path()));
                }
            },
            None => {
                //继续检查以确认是否有后缀名
                let mut path = PathBuf::new();
                path.push(&entry.file_name().to_os_string());
                let vec: Vec<&str> = path.to_str().as_ref().unwrap().split(".").collect();
                if vec.len() > 1 && s0 == vec[1] {
                    //后缀名相同，形如.*的文件
                    if let Ok(meta) = entry.metadata() {
                        result.push((meta.len(), entry.path()));
                    } else {
                        result.push((0, entry.path()));
                    }
                }
            }
            _ => (), //后缀名不同，则忽略
        }
    } else {
        //不过滤
        if let Ok(meta) = entry.metadata() {
            result.push((meta.len(), entry.path()));
        } else {
            result.push((0, entry.path()));
        }
    }
}

//异步加载批量文件，并设置回应
fn async_load_files(req: Request, res: Response, files: Vec<(u64, PathBuf)>, root_len: usize, index: usize, mut data: Vec<u8>, mut size: u64, time: Instant) {
    let file_len: u64;
    let file_path: PathBuf;
    if let Some((len, file)) = files.get(index) {
        if len == &0 {
            //无效文件，则忽略，并继续加载下一个文件
            return async_load_files(req, res, files, root_len, index + 1, data, size, time);
        }

        file_len = len.clone();
        file_path = file.clone();
    } else {
        //加载完成，则回应
        return async_load_files_ok(req, res, size, data, time);
    }
    let s = file_path.to_str().unwrap().as_bytes();
    data.put_u16_le((s.len() - root_len) as u16);
    data.put_slice(&s[root_len..]);
    data.put_u32_le(file_len as u32);
    let open = Box::new(move |f: IOResult<AsyncFile>| {
        match f {
            Err(e) => {
                //打开失败
                async_load_files_error(req, res, e, HTTPS_ASYNC_OPEN_FILE_FAILED_STATUS, time);
            },
            Ok(r) => {
                //打开成功
                let file = Arc::new(r);
                let read = Box::new(move |_: SharedFile, result: IOResult<Vec<u8>>| {
                    match result {
                        Err(e) => {
                            //读失败
                            async_load_files_error(req, res, e, HTTPS_ASYNC_READ_FILE_FAILED_STATUS, time);
                        },
                        Ok(mut bin) => {
                            //读成功，则继续异步加载下一个文件
                            size += file_len;
                            data.append(&mut bin);
                            async_load_files(req, res, files, root_len, index + 1, data, size, time);
                        },
                    }
                });
                file.pread(0, file_len as usize, read);
            },
        }
    });
    AsyncFile::open(file_path, AsyncFileOptions::OnlyRead(1), open);
}

//异步加载文件错误
fn async_load_files_error(req: Request, mut res: Response, err: IOError, err_no: u16, time: Instant) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回错误
                    let func = Box::new(move |_lock| {
                        async_load_files_error(req, res, err, err_no, time);
                    });
                    cast_store_task(TaskType::Async(false), HTTPS_ASYNC_FILE_LOAD_PRIORITY, None, func, Atom::from("async load files failed task"));
                },
                _ => {
                    HTTPS_BATCH_LOAD_FILE_ERROR_TIME.timing(time);
                    HTTPS_BATCH_LOAD_FILE_ERROR_COUNT.sum(1);

                    warn!("!!!> Https Async Load Files Error, task wakeup failed, task id: {}, err: {:?}, e: {:?}", req.uid, err, e)
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

            HTTPS_BATCH_LOAD_FILE_ERROR_TIME.timing(time);
            HTTPS_BATCH_LOAD_FILE_ERROR_COUNT.sum(1);
        },
    }
}

//异步加载文件成功
fn async_load_files_ok(req: Request, mut res: Response, size: u64, data: Vec<u8>, time: Instant) {
    match res.receiver.as_ref().unwrap().consume() {
        Err(e) => {
            match e {
                ConsumeError::Empty => {
                    //未准备好，则继续异步等待返回成功
                    let func = Box::new(move |_lock| {
                        async_load_files_ok(req, res, size, data, time);
                    });
                    cast_store_task(TaskType::Async(false), HTTPS_ASYNC_FILE_LOAD_PRIORITY, None, func, Atom::from("async load files ok task"));
                },
                _ => {
                    HTTPS_BATCH_LOAD_FILE_ERROR_TIME.timing(time);
                    HTTPS_BATCH_LOAD_FILE_ERROR_COUNT.sum(1);

                    warn!("!!!> Https Async Load Files Ok, task wakeup failed, task id: {}, e: {:?}", req.uid, e)
                },
            }
        },
        Ok(waker) => {
            let sender = res.sender.as_ref().unwrap().clone();
            let mut http_res = HttpResponse::<Body>::new(Body::empty());
            res = res.set(StatusCode::OK);
            res.set_mut(mime::APPLICATION_OCTET_STREAM);
            res.headers.insert(headers::CONTENT_LENGTH, size.into());
            res.body = Some(Box::new(data));
            res.write_back(&mut http_res);
            sender.produce(Ok(http_res)).is_ok();
            waker.notify();

            HTTPS_BATCH_LOAD_FILE_OK_TIME.timing(time);
            HTTPS_BATCH_LOAD_FILE_BYTE_COUNT.sum(size as usize);
            HTTPS_BATCH_LOAD_FILE_OK_COUNT.sum(1);
        },
    }
}

