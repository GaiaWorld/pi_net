use std::fs::{self, Metadata};
use std::io::{Error, ErrorKind, Result};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use futures::future::{FutureExt, LocalBoxFuture};
use https::{
    header::{CONTENT_LENGTH, CONTENT_TYPE},
    StatusCode,
};
use log::warn;
use mime_guess;

use pi_async_file::file::{AsyncFile, AsyncFileOptions};
use pi_atom::Atom;
use pi_async_rt::rt::{AsyncRuntime, multi_thread::MultiTaskRuntime};

use tcp::Socket;

use crate::{
    gateway::GatewayContext,
    middleware::{Middleware, MiddlewareResult},
    request::HttpRequest,
    response::HttpResponse,
    static_cache::{
        is_modified, is_unmodified, request_get_cache, set_cache_resp_headers, CacheRes,
        StaticCache,
    },
    utils::{DEAFULT_CHUNK_SIZE, HttpRecvResult, trim_path},
};
use crate::response::ResponseHandler;

///
/// Http文件加载
///
pub struct FileLoad {
    files_async_runtime:    MultiTaskRuntime<()>,       //异步文件运行时
    root:                   PathBuf,                    //文件根路径
    cache:                  Option<Arc<StaticCache>>,   //文件缓存
    is_cache:               bool,                       //是否要求客户端每次请求强制验证资源是否过期
    is_store:               bool,                       //设置是否缓存资源
    is_transform:           bool,                       //设置是否允许客户端更改前端资源内容
    is_only_if_cached:      bool,                       //设置是否要求代理有缓存，则只由代理向客户端提供资源
    max_age:                u64,                        //缓存有效时长
    min_block_size:         Option<u64>,                //最小分块大小
    chunk_size:             Option<u64>,                //每个块大小
    interval:               Option<usize>,              //分块加载块间隔时长
}

unsafe impl Send for FileLoad {}

unsafe impl Sync for FileLoad {}

impl<S: Socket> Middleware<S, GatewayContext> for FileLoad {
    fn request<'a>(
        &'a self,
        context: &'a mut GatewayContext,
        req: HttpRequest<S>,
    ) -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let future = async move {
            let mut resp = HttpResponse::new(2);

            //获取Http请求的文件路径
            let mut file_path = self.root.to_path_buf();
            file_path.extend(&normalize_path(Path::new(req.url().path())));

            let mut file_path_id = Atom::from("");
            if let Some(path) = file_path.to_str() {
                file_path_id = Atom::from(path);
            } else {
                //无法获取有效文件路径的字符串，则立即抛出错误
                return MiddlewareResult::Throw(Error::new(
                    ErrorKind::Other,
                    "File load failed, reason: invaild file path",
                ));
            }

            //访问指定文件的内存缓存
            if let Some(cache) = &self.cache {
                //设置了文件缓存
                match is_unmodified(cache.as_ref(), &req, None, file_path_id.clone()) {
                    Err(e) => {
                        return MiddlewareResult::Throw(e);
                    }
                    Ok(None) => (), //忽略这个判断
                    Ok(Some(false)) => {
                        //验证指定文件的缓存已修改，则立即返回指定错误
                        resp.status(StatusCode::PRECONDITION_FAILED.as_u16());
                        resp.header(CONTENT_LENGTH.as_str(), "0");
                        return MiddlewareResult::Break(resp);
                    }
                    Ok(Some(true)) => {
                        //验证指定文件的缓存未修改，则立即返回
                        resp.status(StatusCode::NOT_MODIFIED.as_u16());
                        return MiddlewareResult::Break(resp);
                    }
                }

                match is_modified(cache.as_ref(), &req, None, file_path_id.clone()) {
                    Err(e) => {
                        return MiddlewareResult::Throw(e);
                    }
                    Ok(true) => (), //验证指定文件的缓存已修改，则继续
                    Ok(false) => {
                        //验证指定文件的缓存未修改，则立即返回
                        resp.status(StatusCode::NOT_MODIFIED.as_u16());
                        return MiddlewareResult::Break(resp);
                    }
                }

                match request_get_cache(cache.as_ref(), &req, None, file_path_id.clone()) {
                    Err(e) => {
                        //获取指定文件的缓存错误，则立即抛出错误
                        return MiddlewareResult::Throw(e);
                    }
                    Ok((is_store, max_age, res)) => {
                        match res {
                            CacheRes::Cache((last_modified, mime, sign, bin)) => {
                                //指定文件存在有效的缓存，则设置缓存响应头，响应体文件类型和响应体长度，并立即返回响应
                                set_cache_resp_headers(
                                    &mut resp,
                                    false,
                                    self.is_cache,
                                    is_store,
                                    self.is_transform,
                                    self.is_only_if_cached,
                                    max_age,
                                    Some(last_modified),
                                    sign,
                                );
                                resp.header(CONTENT_TYPE.as_str(), mime.as_ref());
                                if let Some(body) = resp.as_mut_body() {
                                    //将缓存数据写入响应体
                                    body.init();
                                    body.push(bin.as_slice());
                                }
                                return MiddlewareResult::Finish((req, resp)); //立即结束请求处理，继续进行响应处理
                            }
                            _ => (), //无效的缓存，则从磁盘加载指定文件
                        }
                    }
                }
            }

            //访问指定文件的磁盘资源
            let meta = match fs::metadata(&file_path) {
                Err(e) => {
                    //获取文件元信息错误
                    match e.kind() {
                        ErrorKind::NotFound => {
                            //文件未找到
                            resp.status(StatusCode::NOT_FOUND.as_u16());
                            resp.header(CONTENT_LENGTH.as_str(), "0");
                            return MiddlewareResult::Break(resp);
                        }
                        ErrorKind::PermissionDenied => {
                            //禁止访问文件
                            resp.status(StatusCode::FORBIDDEN.as_u16());
                            resp.header(CONTENT_LENGTH.as_str(), "0");
                            return MiddlewareResult::Break(resp);
                        }
                        _ => {
                            //服务器内部错误
                            return MiddlewareResult::Throw(e);
                        }
                    }
                }
                Ok(meta) => meta,
            };

            match get_file_path(file_path, &meta) {
                None => {
                    //文件未找到
                    resp.status(StatusCode::NOT_FOUND.as_u16());
                    resp.header(CONTENT_LENGTH.as_str(), "0");
                    return MiddlewareResult::Break(resp);
                }
                Some((path, size)) => {
                    //文件存在，则异步加载指定文件，并根据文件扩展名，设置Mime
                    let file_mime;
                    if let Some(mime) = mime_guess::from_path(path.as_path()).first() {
                        //解析出文件的Mime
                        file_mime = mime;
                        resp.header(CONTENT_TYPE.as_str(), file_mime.as_ref());
                    } else {
                        //未解析文件的Mime，则默认为文本明文
                        file_mime = mime::TEXT_PLAIN;
                        resp.header(CONTENT_TYPE.as_str(), file_mime.as_ref());
                    }

                    //缓存加载的文件名、文件的Mime和文件最近修改时间到网关上下文的参数表中
                    if let Ok(last_modified) = meta.modified() {
                        context.set_cache_args(Some((
                            file_path_id.to_string(),
                            file_mime,
                            last_modified,
                        )));
                    }

                    if let Some(min_block_size) = self.min_block_size {
                        //设置了最小分块大小
                        if size >= min_block_size {
                            //需要分块
                            let chunk_size = if let Some(chunk_size) = self.chunk_size {
                                //指定了每个块大小
                                if min_block_size > chunk_size {
                                    //最小分块大小大于指定的每个块大小
                                    chunk_size as usize
                                } else {
                                    //最小分块大小小于等于指定的每个块大小
                                    min_block_size as usize
                                }
                            } else {
                                //没有指定每个块大小
                                if min_block_size > DEAFULT_CHUNK_SIZE {
                                    //最小分块大小大于默认每个块大小
                                    DEAFULT_CHUNK_SIZE as usize
                                } else {
                                    //最小分块大小小于等于默认每个块大小
                                    min_block_size as usize
                                }
                            };
                            resp.enable_stream(); //将当前请求的块响应改为流响应

                            //异步分块加载指定文件
                            async_load_file_chunks(self.files_async_runtime.clone(),
                                                   &resp,
                                                   path,
                                                   chunk_size,
                                                   self.interval);

                            //立即完成请求处理，并立即返回当前请求的流响应
                            return MiddlewareResult::Break(resp)
                        } else {
                            //不需要分块
                            if let Err(e) = async_load_file(self.files_async_runtime.clone(),
                                                            &resp,
                                                            path).await {
                                return MiddlewareResult::Throw(e);
                            }
                        }
                    } else {
                        //未设置最小分块大小
                        if let Err(e) = async_load_file(self.files_async_runtime.clone(),
                                                        &resp,
                                                        path).await {
                            return MiddlewareResult::Throw(e);
                        }
                    }
                }
            }

            //完成请求处理
            MiddlewareResult::Finish((req, resp))
        };
        future.boxed_local()
    }

    fn response<'a>(
        &'a self,
        context: &'a mut GatewayContext,
        req: HttpRequest<S>,
        resp: HttpResponse,
    ) -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let mut body_bufs: Vec<Vec<u8>> = Vec::new();
        let mut response = resp;
        let future = async move {
            if let Some(body) = response.as_mut_body() {
                //当前响应有响应体
                if body.check_init() {
                    //当前响应体已初始化，则中止当前响应处理，并继续后继响应处理
                    return MiddlewareResult::ContinueResponse((req, response));
                }

                //持续获取响应体的内容
                loop {
                    match body.body().await {
                        HttpRecvResult::Err(e) => {
                            //获取Http响应体错误
                            return MiddlewareResult::Throw(e);
                        }
                        HttpRecvResult::Ok(bodys) => {
                            //获取到的是Http响应体块的后继
                            for (_index, bin) in bodys {
                                body_bufs.push(bin);
                            }
                        }
                        HttpRecvResult::Fin(bodys) => {
                            //获取到的是Http响应体块的尾部，处理后退出循环
                            body.init(); //未初始化，则初始化响应体
                            for buf in body_bufs {
                                body.push(buf.as_slice());
                            }
                            for (_index, bin) in bodys {
                                body.push(bin.as_slice());
                            }

                            match body.len() {
                                Some(body_len) if body_len > 0 => (), //加载指定文件成功
                                _ => {
                                    //加载指定文件失败，则设置响应状态为文件未找到
                                    response.status(StatusCode::NOT_FOUND.as_u16());
                                }
                            }
                            break;
                        }
                    }
                }
            }

            if self.is_store {
                //需要缓存文件
                if let Some(body) = response.as_body() {
                    //响应有响应体
                    if let Some(body_bin) = body.as_slice() {
                        //有文件内容
                        if let Some(cache) = &self.cache {
                            //需要缓存加载的文件
                            if let Some((file_path, mime, last_modified)) = context.get_cache_args()
                            {
                                match cache.insert(
                                    None,
                                    self.max_age,
                                    last_modified.clone(),
                                    mime,
                                    false,
                                    Atom::from(file_path.clone()),
                                    Arc::new(Vec::from(body_bin)),
                                ) {
                                    Err(e) => {
                                        //缓存指定文件错误
                                        warn!("!!!> File Load Ok, But Cache Failed, file: {:?}, reason: {:?}",
                                            file_path,
                                            e);
                                    }
                                    Ok((sign, _)) => {
                                        //缓存指定文件成功，则设置响应的缓存头
                                        set_cache_resp_headers(
                                            &mut response,
                                            false,
                                            self.is_cache,
                                            self.is_store,
                                            self.is_transform,
                                            self.is_only_if_cached,
                                            self.max_age,
                                            Some(last_modified),
                                            sign,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            //继续响应处理
            MiddlewareResult::ContinueResponse((req, response))
        };
        future.boxed_local()
    }
}

impl FileLoad {
    /// 构建指定根目录的文件加载器
    pub fn new<P: Into<PathBuf>>(
        files_async_runtime: MultiTaskRuntime<()>,
        dir: P,
        cache: Option<Arc<StaticCache>>,
        is_cache: bool,
        is_store: bool,
        is_transform: bool,
        is_only_if_cached: bool,
        max_age: usize,
    ) -> Self {
        match trim_path(dir) {
            Err(e) => {
                panic!("Create Http File Load Failed, reason: {:?}", e);
            }
            Ok(root) => FileLoad {
                files_async_runtime,
                root,
                cache,
                is_cache,
                is_store,
                is_transform,
                is_only_if_cached,
                max_age: max_age as u64,
                min_block_size: None,
                chunk_size: None,
                interval: None,
            },
        }
    }

    /// 设置最小分块大小，单位字节
    pub fn set_min_block_size(&mut self, min_block_size: Option<u64>) {
        self.min_block_size = min_block_size;
    }

    /// 设置每个块大小，单位字节
    pub fn set_chunk_size(&mut self, chunk_size: Option<u64>) {
        self.chunk_size = chunk_size;
    }

    /// 设置加载每个块的间隔时长
    pub fn set_interval(&mut self, interval: Option<usize>) {
        self.interval = interval;
    }
}

// 标准化路径
fn normalize_path(path: &Path) -> PathBuf {
    path.components()
        .fold(PathBuf::new(), |mut result, p| match p {
            Component::Normal(x) => {
                result.push(x);
                result
            }
            Component::ParentDir => {
                result.pop();
                result
            }
            _ => result,
        })
}

// 获取指定元信息的文件实际路径和大小
fn get_file_path(file_path: PathBuf, meta: &Metadata) -> Option<(PathBuf, u64)> {
    if meta.is_file() {
        //指定路径的文件存在，则返回
        return Some((file_path, meta.len()));
    }

    //指定路径的文件不存在，则默认访问指定路径下的index.html
    let index_path = file_path.join("index.html");
    match fs::metadata(&index_path) {
        Err(_) => None,
        Ok(m) => {
            if m.is_file() {
                //默认文件存在，则返回
                Some((index_path, m.len()))
            } else {
                None
            }
        }
    }
}

// 异步加载指定文件
async fn async_load_file(files_async_runtime: MultiTaskRuntime<()>,
                         resp: &HttpResponse,
                         file_path: PathBuf) -> Result<()> {
    if let Some(resp_handler) = resp.get_response_handler() {
        let path = file_path.clone();
        let files_async_runtime_copy = files_async_runtime.clone();
        if let Err(e) = files_async_runtime.spawn(async move {
            //调用底层open接口
            let file = AsyncFile::open(
                files_async_runtime_copy,
                path.clone(),
                AsyncFileOptions::OnlyRead,
            )
                .await;
            match file {
                Ok(file) => {
                    // 获取文件大小
                    let size = file.get_size() as usize;
                    let r = file.read(0, size).await;
                    match r {
                        Ok(bin) => {
                            //读文件成功
                            if let Err(e) = resp_handler.write(bin).await {
                                warn!("Http Body Mut Write Failed, file: {:?}, reason: {:?}", path, e);
                            } else {
                                if let Err(e) = resp_handler.finish().await {
                                    warn!("Http Body Mut Finish Failed, file: {:?}, reason: {:?}", path, e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                path, e
                            );
                            if let Err(e) = resp_handler.finish().await {
                                warn!(
                                    "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                    path, e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Http Async Open File Failed, file: {:?}, reason: {:?}",
                        path, e
                    );
                    if let Err(e) = resp_handler.finish().await {
                        warn!(
                            "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                            path, e
                        );
                    }
                }
            }
        }) {
            warn!(
                "Http Async Open File Failed, reason: {:?}",
                e
            );
        }
        return Ok(());
    }

    Err(Error::new(
        ErrorKind::NotFound,
        "Load file error, reason: invalid response body",
    ))
}

// 异步分块加载指定文件
fn async_load_file_chunks(files_async_runtime: MultiTaskRuntime<()>,
                          resp: &HttpResponse,
                          file_path: PathBuf,
                          chunk_size: usize,
                          interval: Option<usize>) -> Result<()> {
    if let Some(resp_handler) = resp.get_response_handler() {
        let path = file_path.clone();
        let files_async_runtime_copy = files_async_runtime.clone();
        if let Err(e) = files_async_runtime.spawn(async move {
            //调用底层open接口
            let file = AsyncFile::open(
                files_async_runtime_copy.clone(),
                path.clone(),
                AsyncFileOptions::OnlyRead,
            )
                .await;
            match file {
                Ok(file) => {
                    // 获取文件大小
                    async_load_file_chunk(files_async_runtime_copy,
                                          resp_handler,
                                          file_path,
                                          file,
                                          0,
                                          chunk_size,
                                          interval);
                }
                Err(e) => {
                    warn!("Open http async file failed, file: {:?}, reason: {:?}",
                        path,
                        e);
                    if let Err(e) = resp_handler.finish().await {
                        warn!("Http body mut finish failed, file: {:?}, reason: {:?}",
                            path,
                            e);
                    }
                }
            }
        }) {
            warn!("Open http async file failed, reason: {:?}", e);
        }

        return Ok(());
    }

    Err(Error::new(
        ErrorKind::NotFound,
        "Load file error, reason: invalid response body",
    ))
}

// 异步加载指定文件的指定块
fn async_load_file_chunk(files_async_runtime: MultiTaskRuntime<()>,
                         resp_handler: ResponseHandler,
                         file_path: PathBuf,
                         file: AsyncFile<()>,
                         chunk_offset: u64,
                         chunk_size: usize,
                         interval: Option<usize>) {
    let files_async_runtime_copy = files_async_runtime.clone();
    files_async_runtime.spawn(async move {
        let now = if let Some(timeout) = interval {
            //设置了加载间隔时间
            Some(Instant::now())
        } else {
            None
        };

        let r = file.read(chunk_offset, chunk_size).await;
        match r {
            Ok(bin) => {
                let bin_len = bin.len();
                if bin_len > 0 {
                    //读文件成功
                    if let Err(e) = resp_handler.write(bin).await {
                        warn!("Write http body failed for file chunk, file: {:?}, reason: {:?}",
                        file_path,
                        e);
                    } else {
                        //发送文件的块到响应流成功，则继续异步加载指定文件的下个块
                        if let Some(timeout) = interval {
                            //设置了加载间隔时间
                            if let Some(now) = now {
                                //休眠指定的时间
                                let real_timeout = timeout
                                    .checked_sub(now.elapsed().as_millis() as usize)
                                    .unwrap_or(0);
                                files_async_runtime_copy.timeout(real_timeout).await;
                            }
                        }

                        async_load_file_chunk(files_async_runtime_copy,
                                              resp_handler,
                                              file_path,
                                              file,
                                              chunk_offset + bin_len as u64,
                                              chunk_size,
                                              interval);
                    }
                } else {
                    //读文件结束
                    if let Err(e) = resp_handler.finish().await {
                        warn!("Finish http body failed for file chunk, file: {:?}, reason: {:?}",
                            file_path,
                            e);
                    }
                }
            }
            Err(e) => {
                warn!("Finish http body failed for file chunk, file: {:?}, reason: {:?}",
                    file_path,
                    e);
                if let Err(e) = resp_handler.finish().await {
                    warn!("Finish http body failed for file chunk, file: {:?}, reason: {:?}",
                        file_path,
                        e);
                }
            }
        }
    });
}


