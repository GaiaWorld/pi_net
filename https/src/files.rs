use std::sync::Arc;
use std::str::Chars;
use std::ffi::OsStr;
use std::fs::DirEntry;
use std::path::{Path, PathBuf};
use std::io::{Error as IOError, Result as IOResult, ErrorKind};

use http::StatusCode;
use hyper::body::Body;
use modifier::Set;

use pi_base::file::{AsynFileOptions, Shared, AsyncFile, SharedFile};

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
* 批量静态资源文件
*/
#[derive(Clone)]
pub struct StaticFileBatch {
    root: PathBuf,
    parse_files: fn(Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>),
}

impl Set for StaticFileBatch {}

impl Handler for StaticFileBatch {
    fn handle(&self, mut req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        if req.url.path().len() > 1 || req.url.path()[0] != "" {
            //无效的路径，则忽略
            return Some((req, res, 
                        Err(HttpsError::new(IOError::new(ErrorKind::NotFound, "load batch file error, invalid file path")))));
        }

        let mut ds = "";
        let mut fs = "";
        let mut size = 0;
        {
            let map = req.get_ref::<Params>().unwrap();
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
        
        let mut dir_vec: Vec<(u64, PathBuf)> = Vec::new();
        let mut dirs: Vec<String> = Vec::new();
        let mut file_vec: Vec<(u64, PathBuf)>;
        let mut files: Vec<String> = Vec::new();
        match decode(&mut ds.chars(), &mut vec![], &mut vec![], &mut dirs, 0) {
            Err(pos) => {            
                let desc = format!("load batch file error, decode dir failed, dir: {}, pos: {}", ds, pos);
                return Some((req, res, Err(HttpsError::new(IOError::new(ErrorKind::NotFound, desc)))));
            },
            Ok(_) => {
                decode_dir(self.parse_files, &mut dirs, &self.root, &mut dir_vec);
            },
        }
        match decode(&mut fs.chars(), &mut vec![], &mut vec![], &mut files, 0) {
            Err(pos) => {
                let desc = format!("load batch file error, decode file failed, file: {}, pos: {}", fs, pos);
                return Some((req, res, Err(HttpsError::new(IOError::new(ErrorKind::NotFound, desc)))));
            },
            Ok(_) => {
                file_vec = files.into_iter().map(|file| {
                        let path = self.root.join(file);
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
        async_load_files(req, res, dir_vec, 0, Vec::new(), 0);
        None
    }
}

impl StaticFileBatch {
    //指定文件根目录，构建指定的批量静态资源文件，可以是绝对路径或相对路径，如果为空串，则表示以当前运行时路径作为文件根目录
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        StaticFileBatch {
            root: root.into(),
            parse_files: disk_files,
        }
    }

    //指定文件根目录和递归解析文件函数，构建指定的批量静态资源文件，可以是绝对路径或相对路径，如果为空串，则表示以当前运行时路径作为文件根目录
    pub fn with<P: Into<PathBuf>>(root: P, func: fn(Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>)) -> Self {
        StaticFileBatch {
            root: root.into(),
            parse_files: func,
        }
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
        for dir in dirs {
            let path = root.join(dir);
            if !path.is_file() && !path.is_dir() {
                //解析目录下指定后缀的文件
                if let Some(s) = path.file_name() {
                    if let Some(str) = s.to_str() {
                        let vec: Vec<&str> = str.split(".").collect();
                        if vec.len() > 1 {
                            parse_files(Some(&OsStr::new(vec[1])), &path.parent().unwrap().to_path_buf(), result);
                            continue;
                        }
                    }
                }
                parse_files(None, &path.parent().unwrap().to_path_buf(), result);
            } else {
                //解析目录下所有文件
                parse_files(None, &path, result);
            }
        }
}

//从硬盘上递归读取文件名和大小
fn disk_files(suffix: Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>) {
    if !path.exists() {
        return;
    }

    for e in path.read_dir().unwrap() {
        if let Ok(entry) = e {
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
}

//过滤指定后缀和大小的文件
fn filter_file(suffix: Option<&OsStr>, entry: DirEntry, result: &mut Vec<(u64, PathBuf)>) {
    if let Some(s0) = suffix {
        //过滤指定后缀名的文件
        match Path::new(&entry.file_name()).extension() {
            Some(s1) if s0 == s1 => {
                //后缀名相同
                if let Ok(meta) = entry.metadata() {
                    result.push((meta.len(), entry.path()));
                } else {
                    result.push((0, entry.path()));
                }
            },
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
fn async_load_files(req: Request, mut res: Response, files: Vec<(u64, PathBuf)>, index: usize, mut data: Vec<u8>, mut size: u64) {
    let file_len: u64;
    let file_path: PathBuf;
    if let Some((len, file)) = files.get(index) {
        if len == &0 {
            //无效文件，则忽略，并继续加载下一个文件
            return async_load_files(req, res, files, index + 1, data, size);
        }

        file_len = len.clone();
        file_path = file.clone();
    } else {
        //加载完成，则回应
        match res.receiver.as_ref().unwrap().consume() {
            Err(e) => println!("!!!> Https Async Open File Task Wakeup Failed, task id: {}, e: {:?}", req.uid, e),
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
            },
        }
        return;
    }

    let open = Box::new(move |f: IOResult<AsyncFile>| {
        match f {
            Err(e) => {
                //打开失败
                match res.receiver.as_ref().unwrap().consume() {
                    Err(_) => println!("!!!> Https Async Open File Task Wakeup Failed, task id: {}, e: {:?}", req.uid, e),
                    Ok(waker) => {
                        let sender = res.sender.as_ref().unwrap().clone();
                        let mut http_res = HttpResponse::<Body>::new(Body::empty());
                        res = res.set(StatusCode::from_u16(HTTPS_ASYNC_OPEN_FILE_FAILED_STATUS).ok().unwrap());
                        res.write_back(&mut http_res);
                        sender.produce(Ok(http_res)).is_ok();
                        waker.notify();
                    },
                }
            }
            Ok(r) => {
                //打开成功
                let file = Arc::new(r);
                let read = Box::new(move |_: SharedFile, result: IOResult<Vec<u8>>| {
                    match result {
                        Err(e) => {
                            //读失败
                            match res.receiver.as_ref().unwrap().consume() {
                                Err(_) => println!("!!!> Https Async Read File Task Wakeup Failed, task id: {}, e: {:?}", req.uid, e),
                                Ok(waker) => {
                                    let sender = res.sender.as_ref().unwrap().clone();
                                    let mut http_res = HttpResponse::<Body>::new(Body::empty());
                                    res = res.set(StatusCode::from_u16(HTTPS_ASYNC_READ_FILE_FAILED_STATUS).ok().unwrap());
                                    res.write_back(&mut http_res);
                                    sender.produce(Ok(http_res)).is_ok();
                                    waker.notify();
                                },
                            }
                        },
                        Ok(mut bin) => {
                            //读成功，则继续异步加载下一个文件
                            size += file_len;
                            data.append(&mut bin);
                            async_load_files(req, res, files, index + 1, data, size);
                        },
                    }
                });
                file.pread(0, file_len as usize, read);
            },
        }
    });
    AsyncFile::open(file_path, AsynFileOptions::OnlyRead(1), open);
}