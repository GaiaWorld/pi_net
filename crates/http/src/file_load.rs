//! 单文件响应中间件：保存外部注入的普通Multi句柄，原请求路径负责缓存及文件读取。
//! FileLoad::new只在初始化冷路径登记所用核心，不创建运行时、时钟、采集器或业务任务。
//! 指标身份和强保活归pi_async_rt，宿主协调共享核心的注销/关闭，本模块无Drop注销。
//! 本轮登记边界见本地D17/D18、FL-MET-BOUNDARY-001；参数/Ok验证见下方独立单元。

use std::ffi::OsStr;
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
use log::{debug, warn};
use mime_guess;

use pi_async_file::file::{AsyncFile, AsyncFileOptions};
use pi_async_rt::rt::{multi_thread::MultiTaskRuntime, AsyncRuntime};
use pi_atom::Atom;

use pi_handler::SGenType;
use tcp::Socket;
use url::Host;

use crate::response::ResponseHandler;
use crate::{
    gateway::GatewayContext,
    middleware::{Middleware, MiddlewareResult},
    request::HttpRequest,
    response::HttpResponse,
    static_cache::{
        is_modified, is_unmodified, request_get_cache, set_cache_resp_headers, simple_sign,
        CacheRes, StaticCache,
    },
    utils::{trim_path, HttpRecvResult, DEAFULT_CHUNK_SIZE},
};

// D18仅单元测试观测：记录真实登记参数和原Result，不持运行时，不提供生产查询。
// 同步构造线程写入、独立单元取走；借用不跨await/线程，普通生产构建完全移除。
// 验证入口：runtime_metrics_tests::test_file_load_registers_runtime_metrics。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static FILE_LOAD_METRICS_TEST_RESULT: std::cell::RefCell<Option<(
        usize,
        String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>,
    )>> = const { std::cell::RefCell::new(None) };
}

/// 使用外部普通Multi加载文件并生成HTTP响应的中间件。
///
/// 通过`new`保存运行时句柄和原缓存策略，请求/响应及分块派发保持原逻辑。
/// 运行时可以与其它组件共享，销毁本中间件不注销或关闭该核心；成功指标登记的
/// 强保活须由宿主统一协调。登记只在构造冷路径，不发生于单个文件/分块发送。
/// 关联验证：`runtime_metrics_tests::test_file_load_registers_runtime_metrics`仅验证登记参数/Ok，
/// 不代表原文件业务、缓存或现有unsafe实现已获本专项完整验证。
pub struct FileLoad {
    files_async_runtime: MultiTaskRuntime<()>, //异步文件运行时
    root: PathBuf,                             //文件根路径
    cache: Option<Arc<StaticCache>>,           //文件缓存
    is_cache: bool,                            //为false时，原缓存响应头逻辑追加no-cache
    is_store: bool,                            //设置是否缓存资源
    is_transform: bool,                        //设置是否允许客户端更改前端资源内容
    is_only_if_cached: bool,                   //设置是否要求代理有缓存，则只由代理向客户端提供资源
    max_age: u64,                              //缓存有效时长
    min_block_size: Option<u64>,               //最小分块大小
    chunk_size: Option<u64>,                   //每个块大小
    interval: Option<usize>,                   //分块加载块间隔时长
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
            // 判断root配置是否是新结构(支持多项目资源加载)
            let tmp = OsStr::new("*");
            if Some(tmp) == file_path.file_name() {
                // 弹出*目录
                file_path.pop();
                // 判断请求中是否携带路径参数r
                if let Some(SGenType::Str(p)) = context.as_params().borrow().get("r") {
                    file_path.push(p.as_str());
                } else {
                    // 目前情况使用主机头作为目录
                    match req.url().host() {
                        Some(Host::Domain(domain)) => {
                            let parts: Vec<&str> = domain.split('.').collect();
                            let subdomain = parts.first().unwrap();
                            file_path.push(subdomain)
                        }
                        Some(Host::Ipv4(ipv4)) => file_path.push(ipv4.to_string()),
                        Some(Host::Ipv6(ipv6)) => {
                            file_path.push(ipv6.to_string().replace(":", "."))
                        }
                        None => {
                            //无法获取host
                            return MiddlewareResult::Throw(Error::new(
                                ErrorKind::Other,
                                "File load failed, reason: invaild host",
                            ));
                        }
                    }
                }
            }

            debug!("fle_load file path:{:?}", file_path);
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

                match request_get_cache(
                    cache.as_ref(),
                    &req,
                    None,
                    file_path_id.clone(),
                    self.max_age,
                ) {
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
                            async_load_file_chunks(
                                self.files_async_runtime.clone(),
                                &resp,
                                path,
                                chunk_size,
                                self.interval,
                            );

                            //立即完成请求处理，并立即返回当前请求的流响应
                            return MiddlewareResult::Break(resp);
                        } else {
                            //不需要分块
                            if let Err(e) =
                                async_load_file(self.files_async_runtime.clone(), &resp, path).await
                            {
                                return MiddlewareResult::Throw(e);
                            }
                        }
                    } else {
                        //未设置最小分块大小
                        if let Err(e) =
                            async_load_file(self.files_async_runtime.clone(), &resp, path).await
                        {
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

                                        let sign = simple_sign(last_modified);
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
    /// 使用外部注入的普通Multi构建文件加载器，必要路径准备后/返回Self前执行冷登记。
    ///
    /// # 参数与返回
    /// - `files_async_runtime`：宿主已构建并完成必要启动的普通Multi句柄，按值保存；
    ///   可传入clone与其它组件共享，不创建新核心，也不转移共享核心的独占关闭权。
    ///   后续原文件任务使用其clone；调用方负责保持原运行时的合法生命周期。
    /// - `dir`：按原`Into<PathBuf>`转换并由`trim_path`逐段重组后保存。空/相对路径和
    ///   原`*`路径用法不新增校验；不访问文件系统，不保证目录存在、可访问或安全隔离。
    ///   自定义`Into`的成本、外部副作用及panic由其实现负责。
    /// - `cache`：原静态缓存的共享Arc句柄，按值保存；None不使用本缓存，Some不独占缓存。
    ///   本构造不读取或更新缓存，也不启动缓存整理。
    /// - `is_cache`：原缓存响应策略；false时原响应头逻辑追加no-cache，true时不追加。
    /// - `is_store`：原缓存保存策略；false时原响应头追加no-store，且本中间件响应阶段
    ///   不执行受该标记控制的缓存写入；本构造仅保存标记。
    /// - `is_transform`：原响应策略；false时原响应头逻辑追加no-transform。
    /// - `is_only_if_cached`：保存并传给原缓存响应头逻辑的提示标记，不在本构造校验
    ///   缓存，也不保证后续未命中时禁止文件I/O；不改变既有报头格式。
    /// - `max_age`：原缓存有效时长，单位秒，按原as u64保存，0也原样保存；
    ///   不新增范围校验，实际缓存及响应策略由原请求/响应流程处理。
    /// 返回新FileLoad，持有传入运行时/缓存句柄和原字段。不是登记回执或健康检查；
    /// 登记失败仅使用既有warn日志，仍返回原对象，不新增业务Err、重试、关闭或注销。
    ///
    /// # 指标、生命周期与安全边界
    /// 默认runtime_metrics开启，非wasm32时以pi_http-multi-{原get_id}直接登记。
    /// 同核同名幂等由上游负责，原编号只用于命名；本构造本身仍创建新对象，非纯、非幂等。
    /// 成功后上游会强保活核心，不保存本库生产结果/ID；丢弃中间件不等于撤销登记。
    /// 宿主唯一生命周期协调者负责停轮/注销/在途采样收尾及原关闭，禁止组件擅自注销共享核心。
    /// 关闭feature或wasm32不生成名称/登记代码；不启trace/Provider，不创建时钟/采集循环。
    /// 同步无await/取消点，不投递任务；上游登记有同步冷锁/分配，失败logger也可能同步等待
    /// 或panic，因此应在服务初始化时调用，不在请求/分块/异步poll热路径反复构造。
    /// logger不得处理本告警时递归构造并再次触发相同告警；本入口不屏蔽用户日志重入。
    /// 原路径Err分支仍panic；当前trim_path总返回Ok。分配器/Into/logger的panic不被捕获。
    /// 并发构造仅使用局部状态及上游线程安全登记/原logger，不新增本库锁或unsafe，
    /// 无借用/锁跨await；原Multi、缓存和FileLoad整体使用仍须遵守各自既有线程/生命周期合同。
    ///
    /// # 成本与示例
    /// 设路径m字节、k个分量、名称n字节，原路径重组最坏O(k*m)时间/O(m)额外空间；
    /// 不含自定义Into成本。新增名称格式化O(n)，上游登记O(C+A*n+L+n)时间及
    /// O(C+A+L+n)新空间，C为历史核心数、A为本核名称数、L为旧合并名长度。
    /// 不保证同步锁争用、分配器或logger的等待上界，不声称构造零开销。
    /// 示例只装配中间件，运行时启动、全局时钟和进程采集仍由宿主负责：
    /// ```no_run
    /// use pi_async_rt::rt::multi_thread::MultiTaskRuntime;
    /// use pi_http::file_load::FileLoad;
    /// fn build_files(runtime: MultiTaskRuntime<()>) -> FileLoad {
    ///     FileLoad::new(runtime, "./htdocs", None, true, true, false, false, 60)
    /// }
    /// ```
    /// 验证：file_load::runtime_metrics_tests::test_file_load_registers_runtime_metrics
    /// 只核对实际运行时/名称参数及原Result为Ok，证据入口FL-MET-U01，不覆盖文件业务。
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
            Ok(root) => {
                #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
                {
                    // 借用外部原句柄登记，不创建核心；真实身份/强保活留给上游，所有权不变。
                    let metric_name = format!("pi_http-multi-{}", files_async_runtime.get_id());
                    let registration = pi_async_rt::runtime_metrics::register_runtime_metrics(
                        &files_async_runtime, &metric_name,
                    );
                    #[cfg(test)]
                    FILE_LOAD_METRICS_TEST_RESULT.with(|value| {
                        // 只复制本次真实参数和纯返回值；借用在原日志调用前结束。
                        *value.borrow_mut() = Some((files_async_runtime.get_id(), metric_name.clone(), registration));
                    });
                    if let Err(error) = registration {
                        // 指标降级不改变原构造或共享核心，不重试/关闭/注销；logger沿原宿主合同。
                        warn!("HTTP 文件加载运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
                    }
                }
                FileLoad {
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
                }
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
async fn async_load_file(
    files_async_runtime: MultiTaskRuntime<()>,
    resp: &HttpResponse,
    file_path: PathBuf,
) -> Result<()> {
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
                                warn!(
                                    "Http Body Mut Write Failed, file: {:?}, reason: {:?}",
                                    path, e
                                );
                            } else {
                                if let Err(e) = resp_handler.finish().await {
                                    warn!(
                                        "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                        path, e
                                    );
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
            warn!("Http Async Open File Failed, reason: {:?}", e);
        }
        return Ok(());
    }

    Err(Error::new(
        ErrorKind::NotFound,
        "Load file error, reason: invalid response body",
    ))
}

// 异步分块加载指定文件
fn async_load_file_chunks(
    files_async_runtime: MultiTaskRuntime<()>,
    resp: &HttpResponse,
    file_path: PathBuf,
    chunk_size: usize,
    interval: Option<usize>,
) -> Result<()> {
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
                    async_load_file_chunk(
                        files_async_runtime_copy,
                        resp_handler,
                        file_path,
                        file,
                        0,
                        chunk_size,
                        interval,
                    );
                }
                Err(e) => {
                    warn!(
                        "Open http async file failed, file: {:?}, reason: {:?}",
                        path, e
                    );
                    if let Err(e) = resp_handler.finish().await {
                        warn!(
                            "Http body mut finish failed, file: {:?}, reason: {:?}",
                            path, e
                        );
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
fn async_load_file_chunk(
    files_async_runtime: MultiTaskRuntime<()>,
    resp_handler: ResponseHandler,
    file_path: PathBuf,
    file: AsyncFile<()>,
    chunk_offset: u64,
    chunk_size: usize,
    interval: Option<usize>,
) {
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
                        warn!(
                            "Write http body failed for file chunk, file: {:?}, reason: {:?}",
                            file_path, e
                        );
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

                        async_load_file_chunk(
                            files_async_runtime_copy,
                            resp_handler,
                            file_path,
                            file,
                            chunk_offset + bin_len as u64,
                            chunk_size,
                            interval,
                        );
                    }
                } else {
                    //读文件结束
                    if let Err(e) = resp_handler.finish().await {
                        warn!(
                            "Finish http body failed for file chunk, file: {:?}, reason: {:?}",
                            file_path, e
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Finish http body failed for file chunk, file: {:?}, reason: {:?}",
                    file_path, e
                );
                if let Err(e) = resp_handler.finish().await {
                    warn!(
                        "Finish http body failed for file chunk, file: {:?}, reason: {:?}",
                        file_path, e
                    );
                }
            }
        }
    });
}

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod runtime_metrics_tests {
    //! 只验证FileLoad::new对外部注入普通Multi的真实冷登记参数及原Result为Ok。
    //! 记录由真实构造内的登记调用写入，不手动预登记、不模拟或替换上游能力。
    //! 不启trace/Provider/SDK、不采集或访问网络，不覆盖文件请求或上游登记算法。
    //! 定向命令须精确选择本单元并串行执行；设计/证据见本地FL-MET-VERIFY-001。

    use super::*;

    /// 被测生产入口：file_load::FileLoad::new，外部Multi参数在Ok(root)内发布前登记。
    /// 测试宿主启动原时钟并创建一个worker的真实空Multi，只构造一个无缓存FileLoad，
    /// 精确核对实际参数的原编号、固定三段名和原Result为Ok，不以Self成功替代登记成功。
    /// 仅测试拥有该新核心，取走记录后、断言前显式撤销自己的成功登记；不加Drop注销，
    /// 不调用返回false的原Multi::close伪造关闭，也不注销任何外部共享核心。
    /// 槽只持编号/名称/纯Result，取走和清理在当前线程同步完成，无跨await借用或采样；
    /// 核对O(n)时间/空间，n为短名称字节数，真实Builder/全局时钟沿原进程生命周期退出。
    /// 对上游SDK、容量、去重、计数及文件业务均不作测试或验收结论。
    #[test]
    fn test_file_load_registers_runtime_metrics() {
        FILE_LOAD_METRICS_TEST_RESULT.with(|value| { value.borrow_mut().take(); });
        let _timer = pi_async_rt::rt::startup_global_time_loop(10);
        let runtime = pi_async_rt::rt::AsyncRuntimeBuilder::default_multi_thread(
            Some("http-fileload-metrics-unit"), None, Some(1), Some(1),
        );
        let expected_runtime_id = runtime.get_id();
        let expected_name = format!("pi_http-multi-{}", expected_runtime_id);

        let _files = FileLoad::new(runtime.clone(), ".", None, false, false, false, false, 0);

        let observed = FILE_LOAD_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            // 测试明确拥有这个全新核心，清理先于断言；不把注销返回值当作上游测试。
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (actual_runtime_id, actual_name, registration) = observed
            .expect("FileLoad::new必须调用真实登记并记录本次原始结果");
        assert_eq!(actual_runtime_id, expected_runtime_id, "必须登记外部实际注入的运行时");
        assert_eq!(actual_name, expected_name, "登记名称必须精确使用库名、真实类型和原编号");
        assert!(registration.is_ok(), "实际登记必须成功，原始结果: {:?}", registration);
    }
}
