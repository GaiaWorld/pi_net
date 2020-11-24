use std::sync::Arc;
use std::str::Chars;
use std::ffi::OsStr;
use std::fs::DirEntry;
use std::str::FromStr;
use std::result::Result as GenResult;
use std::io::{Error, Result, ErrorKind};
use std::path::{Component, PathBuf, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use mime::APPLICATION_OCTET_STREAM;
use https::{StatusCode, header::{CONTENT_DISPOSITION, CONTENT_TYPE, CONTENT_LENGTH}};
use futures::future::{FutureExt, BoxFuture, MapErr};
use path_absolutize::Absolutize;
use bytes::BufMut;
use log::warn;

use tcp::driver::{Socket, AsyncIOWait};
use handler::SGenType;
use atom::Atom;
use file::file::{AsyncFileOptions, WriteOptions, Shared, SharedFile, AsyncFile};

use crate::{gateway::GatewayContext,
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::HttpResponse,
            static_cache::{is_unmodified, is_modified, request_get_cache, set_cache_resp_headers, CacheRes, StaticCache},
            util::{HttpRecvResult, trim_path}};
use std::time::SystemTime;

/*
* 默认的附件名
*/
const DEFAULT_CONTENT_DISPOSITION: &str = "attachment;filename=batch";

/*
* Http文件改进的批量加载器
*/
pub struct BatchLoad {
    root:               PathBuf,                    //文件根路径
    cache:              Option<Arc<StaticCache>>,   //文件缓存
    is_cache:           bool,                       //是否要求客户端每次请求强制验证资源是否过期
    is_store:           bool,                       //设置是否缓存资源
    is_transform:       bool,                       //设置是否允许客户端更改前端资源内容
    is_only_if_cached:  bool,                       //设置是否要求代理有缓存，则只由代理向客户端提供资源
    max_age:            u64,                        //缓存有效时长
}

unsafe impl Send for BatchLoad {}
unsafe impl Sync for BatchLoad {}

impl<S: Socket, W: AsyncIOWait> Middleware<S, W, GatewayContext> for BatchLoad {
    fn request<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>)
                   -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let future = async move {
            //获取请求参数
            let mut ds = String::from("");
            let mut fs = String::from("");
            let mut rp = PathBuf::new();
            if let Some(SGenType::Str(r)) = context.as_params().borrow().get("r") {
                rp.push(&self.root);
                rp.push(r.as_str());
            }
            if let Some(SGenType::Str(d)) = context.as_params().borrow().get("d") {
                ds = d.clone();
            }
            if let Some(SGenType::Str(f)) = context.as_params().borrow().get("f") {
                fs = f.clone();
            }
            let files_id = Atom::from(ds.to_string() + "&" + fs.as_str());
            let root = if rp.as_path().to_str().unwrap().as_bytes().len() > 0 {
                &rp
            } else {
                &self.root
            };

            //访问指定的批量文件的内存缓存
            if let Some(cache) = &self.cache {
                //设置了文件缓存
                match is_unmodified(cache.as_ref(), &req, None, files_id.clone()) {
                    Err(e) => {
                        return MiddlewareResult::Throw(e);
                    },
                    Ok(None) => (), //忽略这个判断
                    Ok(Some(false)) => {
                        //验证指定文件的缓存已修改，则立即返回指定错误
                        let mut resp = HttpResponse::new(req.get_handle().clone(), req.get_waits().clone(), 1);
                        resp.status(StatusCode::PRECONDITION_FAILED.as_u16());
                        resp.header(CONTENT_LENGTH.as_str(), "0");
                        return MiddlewareResult::Break(resp);
                    },
                    Ok(Some(true)) => {
                        //验证指定文件的缓存未修改，则立即返回
                        let mut resp = HttpResponse::new(req.get_handle().clone(), req.get_waits().clone(), 1);
                        resp.status(StatusCode::NOT_MODIFIED.as_u16());
                        return MiddlewareResult::Break(resp);
                    },
                }

                match is_modified(cache.as_ref(), &req, None, files_id.clone()) {
                    Err(e) => {
                        return MiddlewareResult::Throw(e);
                    },
                    Ok(true) => (), //验证指定文件的缓存已修改，则继续
                    Ok(false) => {
                        //验证指定文件的缓存未修改，则立即返回
                        let mut resp = HttpResponse::new(req.get_handle().clone(), req.get_waits().clone(), 1);
                        resp.status(StatusCode::NOT_MODIFIED.as_u16());
                        return MiddlewareResult::Break(resp);
                    },
                }

                match request_get_cache(cache.as_ref(), &req, None, files_id.clone()) {
                    Err(e) => {
                        //获取指定文件的缓存错误，则立即抛出错误
                        return MiddlewareResult::Throw(e);
                    },
                    Ok((is_store, max_age, res)) => {
                        match res {
                            CacheRes::Cache((_last_modified, mime, sign, bin)) => {
                                //指定文件存在有效的缓存，则设置缓存响应头，响应体文件类型和响应体长度，并立即返回响应
                                let mut resp = HttpResponse::new(req.get_handle().clone(), req.get_waits().clone(), 1);
                                set_cache_resp_headers(&mut resp, false, self.is_cache, is_store, self.is_transform, self.is_only_if_cached, max_age, None, sign);
                                resp.header(CONTENT_DISPOSITION.as_str(), DEFAULT_CONTENT_DISPOSITION);
                                resp.header(CONTENT_TYPE.as_str(), mime.as_ref());
                                if let Some(body) = resp.as_mut_body() {
                                    //将缓存数据写入响应体
                                    body.init();
                                    body.push(bin.as_slice());
                                }
                                return MiddlewareResult::Finish((req, resp)); //立即结束请求处理，继续进行响应处理
                            },
                            _ => (), //无效的缓存，则从磁盘加载指定文件
                        }
                    },
                }
            }

            //访问指定的批量文件的磁盘资源
            let mut dir_vec: Vec<(u64, PathBuf)> = Vec::new();
            let mut dirs: Vec<String> = Vec::new();
            match decode(&mut ds.chars(), &mut vec![], &mut vec![], &mut dirs, 0) {
                Err(pos) => {
                    //解析请求的目录参数错误，则立即中止请求处理，并返回响应
                    return MiddlewareResult::Throw(Error::new(ErrorKind::NotFound, format!("files load failed, dir: {}, pos: {}, reason: decode dir failed", ds, pos)));
                },
                Ok(_) => {
                    //解析请求的目录参数成功，则开始解析指定目录下的文件
                    if let Err(e) = decode_dir(&mut dirs, root, &mut dir_vec) {
                        //解析指定目录下的文件错误，则立即中止请求处理，并返回响应
                        return MiddlewareResult::Throw(e);
                    }
                },
            }

            let mut file_vec: Vec<(u64, PathBuf)>;
            let mut files: Vec<String> = Vec::new();
            match decode(&mut fs.chars(), &mut vec![], &mut vec![], &mut files, 0) {
                Err(pos) => {
                    //解析请求的文件参数错误，则立即中止请求处理，并返回响应
                    return MiddlewareResult::Throw(Error::new(ErrorKind::NotFound, format!("files load failed, file: {}, pos: {}, reason: decode file failed", fs, pos)));
                },
                Ok(_) => {
                    //解析请求的文件参数成功，则开始解析指定的文件
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

            //合并解析的所有文件，并根据文件数量构建Http响应
            dir_vec.append(&mut file_vec);
            let resp = HttpResponse::new(req.get_handle().clone(), req.get_waits().clone(), dir_vec.len());

            //异步加载所有文件
            match async_load_files(&resp, dir_vec, root.to_str().unwrap().as_bytes().len() + 1) {
                Err(e) => {
                    //异步批量加载文件错误，则立即中止请求处理，并返回响应
                    return MiddlewareResult::Throw(e);
                },
                Ok((files_size, files_len)) => {
                    //设置本次批量文件加载的缓存id、Mime、加载时间、异步批量加载文件大小和数量到网关上下文
                    context.set_cache_args(Some((files_id.as_ref().to_string(), APPLICATION_OCTET_STREAM, SystemTime::now())));
                    context.set_files_size(files_size);
                    context.set_files_len(files_len);
                },
            }

            //完成请求处理
            MiddlewareResult::Finish((req, resp))
        };
        future.boxed()
    }

    fn response<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>, resp: HttpResponse<S, W>)
                    -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let total_size = context.get_files_size(); //需要异步加载文件的总大小
        let total_len = context.get_files_len(); //需要异步加载文件的总数量
        let mut loaded_size = 0; //已加载成功的文件大小
        let mut loaded_len = 0; //已加载成功的文件数量
        let mut body_buf: Vec<(u64, Vec<u8>)> = Vec::with_capacity(total_len); //文件内容缓冲
        let mut response = resp;

        let future = async move {
            if let Some(body) = response.as_mut_body() {
                //当前响应有响应体
                if body.check_init() {
                    //当前响应体已初始化，则中止当前响应处理，并继续后继响应处理
                    return MiddlewareResult::ContinueResponse((req, response));
                }

                if total_len == 0 {
                    //当前没有加载任何文件，则中止当前响应处理，并立即响应文件未找到
                    response.status(StatusCode::NOT_FOUND.as_u16());
                    response.header(CONTENT_LENGTH.as_str(), "0");
                    return MiddlewareResult::Break(response);
                }

                //持续获取响应体的内容
                loop {
                    match body.body().await {
                        HttpRecvResult::Err(e) => {
                            //获取Http响应体错误
                            return MiddlewareResult::Throw(e);
                        },
                        HttpRecvResult::Ok(bodys) => {
                            //获取到的是Http响应体块的后继
                            for (index, bin) in bodys {
                                loaded_size += bin.len() as u64;
                                loaded_len += 1;
                                body_buf.push((index, bin));
                            }
                        },
                        HttpRecvResult::Fin(bodys) => {
                            //获取到的是Http响应体块的尾部，处理后退出循环
                            body.init(); //未初始化，则初始化响应体
                            for (index, bin) in bodys {
                                loaded_size += bin.len() as u64;
                                loaded_len += 1;
                                body_buf.push((index, bin));
                            }

                            if loaded_len < total_len {
                                //文件数量不匹配
                                return MiddlewareResult::Throw(Error::new(ErrorKind::Other, format!("files load failed, require len: {:?}, loaded len: {:?}, reason: invalid file len", total_len, loaded_len)));
                            }

                            //文件已加载完成，检查文件大小是否匹配
                            if loaded_size != total_size {
                                //文件大小不匹配
                                return MiddlewareResult::Throw(Error::new(ErrorKind::Other, format!("files load failed, require size: {:?}, loaded size: {:?}, reason: invalid file size", total_size, loaded_size)));
                            }

                            //异步批量加载文件成功，则将文件内容缓冲的数据写入Http响应体
                            body_buf.sort();
                            for (_, buf) in body_buf {
                                body.push(buf.as_slice());
                            }
                            break;
                        },
                    }
                }
            }

            if let Some((files_id, mime, last_modified)) = context.get_cache_args() {
                //设置响应体类型
                response.header(CONTENT_DISPOSITION.as_str(), DEFAULT_CONTENT_DISPOSITION);
                response.header(CONTENT_TYPE.as_str(), mime.as_ref());

                if self.is_store {
                    //需要缓存文件
                    if let Some(body) = response.as_body() {
                        //响应有响应体
                        if let Some(body_bin) = body.as_slice() {
                            //有文件内容
                            if let Some(cache) = &self.cache {
                                //需要缓存加载的文件
                                let value = Arc::new(Vec::from(body_bin));
                                match cache.insert(None, self.max_age, last_modified.clone(), mime, true, Atom::from(files_id.clone()), value.clone()) {
                                    Err(e) => {
                                        //缓存指定的批量文件错误
                                        warn!("!!!> Files Load Ok, But Cache Failed, file: {:?}, reason: {:?}", files_id, e);
                                    },
                                    Ok((sign, _)) => {
                                        //缓存指定的批量文件成功，则设置响应的缓存头
                                        set_cache_resp_headers(&mut response, false, self.is_cache, self.is_store, self.is_transform, self.is_only_if_cached, self.max_age, None, sign);
                                    },
                                }
                            }
                        }
                    }
                }
            }

            //继续响应处理
            MiddlewareResult::ContinueResponse((req, response))
        };
        future.boxed()
    }
}

impl BatchLoad {
    //构建指定根目录的文件改进的批量加载器
    pub fn new<P: Into<PathBuf>>(dir: P,
                                 cache: Option<Arc<StaticCache>>,
                                 is_cache: bool,
                                 is_store: bool,
                                 is_transform: bool,
                                 is_only_if_cached: bool,
                                 max_age: usize) -> Self {
        match trim_path(dir) {
            Err(e) => {
                panic!("Create Http Batch Load Failed, reason: {:?}", e);
            },
            Ok(root) => {
                BatchLoad {
                    root,
                    cache,
                    is_cache,
                    is_store,
                    is_transform,
                    is_only_if_cached,
                    max_age: max_age as u64,
                }
            },
        }
    }
}

//解析后缀，失败返回在解析哪个字符时出错
fn decode(chars: &mut Chars, suffix: &mut Vec<char>, stack: &mut Vec<String>, result: &mut Vec<String>, mut pos: usize) -> GenResult<(), usize> {
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
fn decode_file(chars: &mut Chars, suffix: &mut Vec<char>, stack: &mut Vec<String>, result: &mut Vec<String>, mut pos: usize) -> GenResult<(), usize> {
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
fn decode_path(chars: &mut Chars, stack: &mut Vec<String>, result: &mut Vec<String>, mut pos: usize) -> GenResult<(), usize> {
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
fn decode_dir(dirs: &mut Vec<String>, root: &PathBuf, result: &mut Vec<(u64, PathBuf)>) -> Result<()> {
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
                        if let Err(e) = disk_files(Some(&OsStr::new(vec[1])), &path.parent().unwrap().to_path_buf().join(vec[0]), result) {
                            return Err(e);
                        }
                        len = result.len();
                        result[index..len].sort_by(|(_, x), (_, y)| x.to_str().as_mut().unwrap().replace("\\", "/").cmp(&y.to_str().as_mut().unwrap().replace("\\", "/")));
                        continue;
                    }
                }
            }
            index = result.len();
            if let Err(e) = disk_files(None, &path.parent().unwrap().to_path_buf(), result) {
                return Err(e);
            }
            len = result.len();
            result[index..len].sort_by(|(_, x), (_, y)| x.to_str().as_ref().unwrap().replace("\\", "/").cmp(&y.to_str().as_ref().unwrap().replace("\\", "/")));
        } else {
            //解析目录下所有文件，形如*/
            index = result.len();
            if let Err(e) = disk_files(None, &path, result) {
                return Err(e);
            }
            len = result.len();
            result[index..len].sort_by(|(_, x), (_, y)| x.to_str().as_ref().unwrap().replace("\\", "/").cmp(&y.to_str().as_ref().unwrap().replace("\\", "/")));
        }
    }

    Ok(())
}

//从硬盘上递归读取文件名和大小
fn disk_files(suffix: Option<&OsStr>, path: &PathBuf, result: &mut Vec<(u64, PathBuf)>) -> Result<()> {
    if !path.exists() {
        return Err(Error::new(ErrorKind::NotFound, format!("select disk files failed, path: {:?}, reason: path not exist", path)));
    }

    match path.read_dir() {
        Err(e) => {
            Err(Error::new(ErrorKind::Other, format!("select disk files failed, path: {:?}, reason: {:?}", path, e)))
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

            Ok(())
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

//异步批量加载文件，并返回批量加载文件的总大小和总数量
fn async_load_files<S: Socket, W: AsyncIOWait>(resp: &HttpResponse<S, W>, files: Vec<(u64, PathBuf)>, root_len: usize) -> Result<(u64, usize)> {
    let mut total_size = 0;
    let mut index: u64 = 0;

    //统计批量加载文件的总大小
    for (size, file_path) in &files {
        if size == &0 {
            //空文件，则忽略
            continue;
        }

        let file_path_bin = file_path.to_str().unwrap().as_bytes();
        total_size += 6 + (file_path_bin.len() - root_len) as u64 + *size;
    }

    if let Some(resp_handler) = resp.get_response_handler() {
        let unload_size = Arc::new(AtomicU64::new(total_size)); //未加载文件大小

        for (size, file_path) in files {
            if size == 0 {
                //空文件，则忽略
                continue;
            }

            let resp_handler_copy = resp_handler.clone();
            let file_path_copy = file_path.clone();
            let unload_size_copy = unload_size.clone();
            let open = Box::new(move |f: Result<AsyncFile>| {
                match f {
                    Err(e) => {
                        //打开文件失败
                        warn!("!!!> Http Aasync Open Files Failed, file: {:?}, reason: {:?}", file_path_copy, e);
                        if let Err(e) = resp_handler_copy.finish() {
                            warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", file_path_copy, e);
                        }
                    },
                    Ok(r) => {
                        //打开文件成功
                        let file = Arc::new(r);
                        let read = Box::new(move |_: SharedFile, result: Result<Vec<u8>>| {
                            match result {
                                Err(e) => {
                                    //读文件失败
                                    warn!("!!!> Http Aasync Read Files Failed, file: {:?}, reason: {:?}", file_path_copy, e);
                                    if let Err(e) = resp_handler_copy.finish() {
                                        warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", file_path_copy, e);
                                    }
                                },
                                Ok(mut bin) => {
                                    //读文件成功
                                    let bin_size = bin.len() as u64;

                                    //为文件内容增加元信息
                                    let file_path_bin = file_path_copy.to_str().unwrap().as_bytes();
                                    let file_path_bin_len = file_path_bin.len() - root_len;
                                    let mut part = Vec::with_capacity(file_path_bin_len + 6);
                                    part.put_u16_le(file_path_bin_len as u16);
                                    part.put_slice(&file_path_bin[root_len..]);
                                    part.put_u32_le(bin_size as u32);
                                    let part_len = 6 + file_path_bin_len as u64 + bin_size;
                                    part.put(bin.as_slice());

                                    if let Err(e) = resp_handler_copy.write_index(index, part) {
                                        warn!("!!!> Http Body Mut Write Index Failed, index: {:?}, file: {:?}, reason: {:?}", index, file_path_copy, e);
                                    } else {
                                        //发送文件成功，则减去当前文件的大小
                                        let last_size = unload_size_copy.fetch_sub(part_len, Ordering::Relaxed);
                                        if last_size == part_len {
                                            //所有文件已加载完成，则结束响应体的异步写
                                            if let Err(e) = resp_handler_copy.finish() {
                                                warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", file_path_copy, e);
                                            }
                                        }
                                    }
                                },
                            }
                        });
                        file.pread(0, size as usize, read);
                    },
                }
            });
            AsyncFile::open(file_path, AsyncFileOptions::OnlyRead(1), open);

            //顺序增加文件序号，并统计文件总数量
            index += 1;
        }

        return Ok((total_size, index as usize));
    }

    Err(Error::new(ErrorKind::NotFound, "load files error, reason: invalid response body"))
}
