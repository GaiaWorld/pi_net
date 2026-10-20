//! 批量文件响应中间件：保存外部普通Multi句柄，原请求流程负责文件选择、读取及缓存。
//! FilesLoad::new仅在初始化冷路径登记所用核心，不依赖FileLoad先构建，不创建运行时。
//! 成功登记的身份/名称/强保活归pi_async_rt，共享核心的注销/关闭由宿主统一协调。
//! 原请求中的目录遍历/metadata含同步文件访问，本次不改动，也不新增采集器或任务。
//! 冻结边界见本地FLS-MET-DESIGN-001/D19；参数/Ok验证见下方独立单元FLS-MET-U01。

use std::ffi::OsStr;
use std::fs::DirEntry;
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::result::Result as GenResult;
use std::str::Chars;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::future::{FutureExt, LocalBoxFuture};
use https::{
    header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    StatusCode,
};
use log::warn;
use mime::APPLICATION_OCTET_STREAM;

use crate::{
    gateway::GatewayContext,
    middleware::{Middleware, MiddlewareResult},
    request::HttpRequest,
    response::HttpResponse,
    static_cache::{
        is_modified, is_unmodified, request_get_cache, set_cache_resp_headers, full_sign, CacheRes,
        StaticCache,
    },
    utils::{trim_path, HttpRecvResult},
};
use pi_async_file::file::{AsyncFile, AsyncFileOptions};
use pi_atom::Atom;
use pi_handler::SGenType;
use pi_async_rt::rt::{AsyncRuntime, multi_thread::MultiTaskRuntime};
use std::time::SystemTime;
use tcp::Socket;

// D19仅单元测试观测：保存真实登记的原编号、名称和原Result，不持运行时。
// 同步构造线程写入、单元取走，借用不跨await/线程；普通生产构建完全移除。
// 验证入口：runtime_metrics_tests::test_files_load_registers_runtime_metrics。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static FILES_LOAD_METRICS_TEST_RESULT: std::cell::RefCell<Option<(
        usize,
        String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>,
    )>> = const { std::cell::RefCell::new(None) };
}

///
/// 默认的附件名
///
const DEFAULT_CONTENT_DISPOSITION: &str = "attachment;filename=files";

// 批量加载的大小头信息关键字
pub(crate) const FILES_LOAD_SIZE_HEADER: &str = "PI-FILES-LOAD-SIZE";
// 批量加载的文件数量头信息关键字
pub(crate) const FILES_LOAD_COUNT_HEADER: &str = "PI-FILES-LOAD-COUNT";

/// 使用外部注入的普通Multi批量加载文件并生成HTTP响应的中间件。
///
/// `new`按原字段保存运行时/缓存句柄及响应策略，不创建或独占运行时核心。
/// 指标登记只发生于构造冷路径，不进入原目录扫描、逐文件读取或响应派发路径；
/// 组件释放不注销共享核心，成功登记的强保活须由宿主生命周期协调者处理。
/// 关联验证：`runtime_metrics_tests::test_files_load_registers_runtime_metrics`仅核对登记参数/Ok，
/// 不代表原文件业务、缓存及既有unsafe实现已经由本专项完整验证。
pub struct FilesLoad {
    files_async_runtime:    MultiTaskRuntime<()>,       //异步文件运行时
    root:                   PathBuf,                    //文件根路径
    cache:                  Option<Arc<StaticCache>>,   //文件缓存
    is_cache:               bool,                       //为false时，原缓存响应头逻辑追加no-cache
    is_store:               bool,                       //设置是否缓存资源
    is_transform:           bool,                       //设置是否允许客户端更改前端资源内容
    is_only_if_cached:      bool,                       //设置是否要求代理有缓存，则只由代理向客户端提供资源
    max_age:                u64,                        //缓存有效时长
}

unsafe impl Send for FilesLoad {}

unsafe impl Sync for FilesLoad {}

impl<S: Socket> Middleware<S, GatewayContext> for FilesLoad {
    fn request<'a>(
        &'a self,
        context: &'a mut GatewayContext,
        req: HttpRequest<S>,
    ) -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let future = async move {
            //获取请求参数
            let mut ds = String::from("");
            let mut fs = String::from("");
            if let Some(SGenType::Str(d)) = context.as_params().borrow().get("d") {
                ds = d.clone();
            }
            if let Some(SGenType::Str(f)) = context.as_params().borrow().get("f") {
                fs = f.clone();
            }
            let files_id = Atom::from(ds.to_string() + "&" + fs.as_str());

            //访问指定的批量文件的内存缓存
            if let Some(cache) = &self.cache {
                //设置了文件缓存
                match is_unmodified(cache.as_ref(),
                                    &req,
                                    None,
                                    files_id.clone()) {
                    Err(e) => {
                        return MiddlewareResult::Throw(e);
                    }
                    Ok(None) => (), //忽略这个判断
                    Ok(Some(false)) => {
                        //验证指定文件的缓存已修改，则立即返回指定错误
                        let mut resp = HttpResponse::new(2);
                        resp.status(StatusCode::PRECONDITION_FAILED.as_u16());
                        resp.header(CONTENT_LENGTH.as_str(), "0");
                        return MiddlewareResult::Break(resp);
                    }
                    Ok(Some(true)) => {
                        //验证指定文件的缓存未修改，则立即返回
                        let mut resp = HttpResponse::new(2);
                        resp.status(StatusCode::NOT_MODIFIED.as_u16());
                        return MiddlewareResult::Break(resp);
                    }
                }

                match is_modified(cache.as_ref(),
                                  &req,
                                  None,
                                  files_id.clone()) {
                    Err(e) => {
                        return MiddlewareResult::Throw(e);
                    }
                    Ok(true) => (), //验证指定文件的缓存已修改，则继续
                    Ok(false) => {
                        //验证指定文件的缓存未修改，则立即返回
                        let mut resp = HttpResponse::new(2);
                        resp.status(StatusCode::NOT_MODIFIED.as_u16());
                        return MiddlewareResult::Break(resp);
                    }
                }

                match request_get_cache(cache.as_ref(),
                                        &req,
                                        None,
                                        files_id.clone(),
                                        self.max_age) {
                    Err(e) => {
                        //获取指定文件的缓存错误，则立即抛出错误
                        return MiddlewareResult::Throw(e);
                    }
                    Ok((is_store, max_age, res)) => {
                        match res {
                            CacheRes::Cache((_last_modified, mime, sign, bin)) => {
                                //指定文件存在有效的缓存，则设置缓存响应头，响应体文件类型和响应体长度，并立即返回响应
                                let mut resp = HttpResponse::new(2);
                                set_cache_resp_headers(
                                    &mut resp,
                                    false,
                                    self.is_cache,
                                    is_store,
                                    self.is_transform,
                                    self.is_only_if_cached,
                                    max_age,
                                    None,
                                    sign,
                                );
                                resp.header(
                                    CONTENT_DISPOSITION.as_str(),
                                    DEFAULT_CONTENT_DISPOSITION,
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

            //访问指定的批量文件的磁盘资源
            let mut dir_vec: Vec<(u64, PathBuf)> = Vec::new();
            let mut dirs: Vec<String> = Vec::new();
            match decode(&mut ds.chars(),
                         &mut vec![],
                         &mut vec![],
                         &mut dirs,
                         0) {
                Err(pos) => {
                    //解析请求的目录参数错误，则立即中止请求处理，并返回响应
                    return MiddlewareResult::Throw(Error::new(
                        ErrorKind::NotFound,
                        format!(
                            "Files load failed, dir: {}, pos: {}, reason: decode dir failed",
                            ds,
                            pos
                        ),
                    ));
                }
                Ok(_) => {
                    //解析请求的目录参数成功，则开始解析指定目录下的文件
                    if let Err(e) = decode_dir(&mut dirs, &self.root, &mut dir_vec) {
                        //解析指定目录下的文件错误，则立即中止请求处理，并返回响应
                        return MiddlewareResult::Throw(e);
                    }
                }
            }

            let mut file_vec: Vec<(u64, PathBuf)>;
            let mut files: Vec<String> = Vec::new();
            match decode(&mut fs.chars(), &mut vec![], &mut vec![], &mut files, 0) {
                Err(pos) => {
                    //解析请求的文件参数错误，则立即中止请求处理，并返回响应
                    return MiddlewareResult::Throw(Error::new(
                        ErrorKind::NotFound,
                        format!(
                            "Files load failed, file: {}, pos: {}, reason: decode file failed",
                            fs,
                            pos
                        ),
                    ));
                }
                Ok(_) => {
                    //解析请求的文件参数成功，则开始解析指定的文件
                    file_vec = files
                        .into_iter()
                        .map(|file| {
                            let path = self.root.join(file);
                            if let Ok(meta) = path.metadata() {
                                (meta.len(), path)
                            } else {
                                (0, path)
                            }
                        })
                        .collect();
                }
            }

            //合并解析的所有文件，并根据文件数量构建Http响应
            dir_vec.append(&mut file_vec);
            let resp = HttpResponse::new(dir_vec.len() + 1);

            //异步加载所有文件
            match async_load_files(self.files_async_runtime.clone(),
                                   &resp,
                                   dir_vec).await {
                Err(e) => {
                    //异步批量加载文件错误，则立即中止请求处理，并返回响应
                    return MiddlewareResult::Throw(e);
                }
                Ok((files_size, files_len)) => {
                    //设置本次批量文件加载的缓存id、Mime、加载时间、异步批量加载文件大小和数量到网关上下文
                    context.set_cache_args(Some((
                        files_id.as_ref().to_string(),
                        APPLICATION_OCTET_STREAM,
                        SystemTime::now(),
                    )));
                    context.set_files_size(files_size);
                    context.set_files_len(files_len);
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
                        }
                        HttpRecvResult::Ok(bodys) => {
                            //获取到的是Http响应体块的后继
                            for (index, bin) in bodys {
                                loaded_size += bin.len() as u64;
                                loaded_len += 1;
                                body_buf.push((index, bin));
                            }
                        }
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
                                return MiddlewareResult::Throw(Error::new(ErrorKind::Other,
                                                                          format!("Files load failed, require len: {:?}, loaded len: {:?}, reason: invalid file len",
                                                                                  total_len,
                                                                                  loaded_len)));
                            }

                            //文件已加载完成，检查文件大小是否匹配
                            if loaded_size != total_size {
                                //文件大小不匹配
                                return MiddlewareResult::Throw(Error::new(ErrorKind::Other,
                                                                          format!("Files load failed, require size: {:?}, loaded size: {:?}, reason: invalid file size",
                                                                                  total_size,
                                                                                  loaded_size)));
                            }

                            //异步批量加载文件成功，则将文件内容缓冲的数据写入Http响应体
                            body_buf.sort();
                            for (_, buf) in body_buf {
                                body.push(buf.as_slice());
                            }
                            break;
                        }
                    }
                }
            }

            // 增加批量加载的特定头信息
            let _ = response.header(FILES_LOAD_SIZE_HEADER, total_size.to_string().as_str());
            let _ = response.header(FILES_LOAD_COUNT_HEADER, total_len.to_string().as_str());

            if let Some((files_id, mime, last_modified)) = context.get_cache_args() {
                //设置响应体类型
                response
                    .header(CONTENT_DISPOSITION.as_str(),
                            DEFAULT_CONTENT_DISPOSITION);
                response
                    .header(CONTENT_TYPE.as_str(),
                            mime.as_ref());

                if self.is_store {
                    //需要缓存文件
                    if let Some(body) = response.as_body() {
                        //响应有响应体
                        if let Some(body_bin) = body.as_slice() {
                            //有文件内容
                            if let Some(cache) = &self.cache {
                                //需要缓存加载的文件
                                let value = Arc::new(Vec::from(body_bin));
                                match cache.insert(
                                    None,
                                    self.max_age,
                                    last_modified.clone(),
                                    mime,
                                    true,
                                    Atom::from(files_id.clone()),
                                    value.clone(),
                                ) {
                                    Err(e) => {
                                        //缓存指定的批量文件错误
                                        warn!("Files Load Ok, But Cache Failed, file: {:?}, reason: {:?}",
                                            files_id,
                                            e);

                                        let sign = full_sign(value.as_slice());
                                        set_cache_resp_headers(
                                            &mut response,
                                            false,
                                            self.is_cache,
                                            self.is_store,
                                            self.is_transform,
                                            self.is_only_if_cached,
                                            self.max_age,
                                            None,
                                            sign,
                                        );
                                    }
                                    Ok((sign, _)) => {
                                        //缓存指定的批量文件成功，则设置响应的缓存头
                                        set_cache_resp_headers(
                                            &mut response,
                                            false,
                                            self.is_cache,
                                            self.is_store,
                                            self.is_transform,
                                            self.is_only_if_cached,
                                            self.max_age,
                                            None,
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

impl FilesLoad {
    /// 使用外部普通Multi构造批量文件加载器，原路径准备后/返回Self前执行冷登记。
    ///
    /// # 参数与返回
    /// - `files_async_runtime`：外部已构建并完成必要启动的普通Multi句柄，按值保存；
    ///   可传clone与其它组件共享，原文件任务继续使用其clone，不创建或独占新核心。
    ///   调用方负责维持原运行时的合法线程/资源生命周期。
    /// - `dir`：按原Into<PathBuf>转换并逐分量重组后保存，不新增空/相对路径检查；
    ///   内置路径处理不访问文件系统，不保证目录存在、权限、符号链接或安全隔离。
    ///   自定义Into的成本、副作用、阻塞及panic由其实现负责。
    /// - `cache`：按值保存原Option<Arc<StaticCache>>；None不使用本缓存，Some不独占缓存。
    ///   本构造不查询/写入缓存，不启动缓存整理或文件读取。
    /// - `is_cache`：原响应策略，false时原缓存头helper追加no-cache，true时不追加。
    /// - `is_store`：保存原缓存写入/响应策略标记；false不执行本中间件中受该标记
    ///   控制的缓存写入，原缓存头helper在对应路径追加no-store，本构造只保存标记。
    /// - `is_transform`：原响应策略，false时原缓存头helper追加no-transform。
    /// - `is_only_if_cached`：保存并传给原缓存头helper的提示标记，不在本构造查缓存，
    ///   不保证后续缓存未命中禁止文件I/O，也不修正原报头格式。
    /// - `max_age`：原缓存有效秒数，按原as u64保存，0原样保留，不新增范围校验。
    /// 返回新的FilesLoad，持有传入句柄和原策略；不是登记回执或健康/任务完成证明。
    /// 登记失败仅沿已有warn设施告警，仍返回原对象，不新增业务Err、重试、关闭或注销。
    ///
    /// # 登记、生命周期与安全边界
    /// runtime_metrics默认开启，非wasm32时以pi_http-multi-{原get_id}直接登记。
    /// 同核同名复用由上游负责，不要求先构造FileLoad，原编号不替代指标身份；
    /// 构造仍创建新对象且登记改变外部状态，非纯、非幂等，不能当作登记续租。
    /// 成功后上游强保活核心，本库不保存生产ID/结果，不新增Drop注销或管理API。
    /// 宿主唯一协调者负责停轮/禁止重登记、注销、在途采样收尾及原关闭；
    /// 销毁一个共享中间件不授予整体注销权，原Multi::close也不提供停止worker能力。
    /// 关闭feature或wasm32不编译名称/登记；不启trace/Provider、时钟或周期采集。
    /// 构造同步无await/取消点，不派发任务；上游有同步冷锁/分配，失败logger可能
    /// 同步等待或panic，应在服务初始化调用，不在请求/逐文件或异步poll中反复构造。
    /// logger不得处理本告警时递归构造并再次触发同一告警；不捕获用户日志重入或panic。
    /// 原路径Err分支保留panic，当前trim_path只重组并返回Ok；Into/分配器边界不变。
    /// 并发构造只使用局部状态、上游线程安全登记及原logger，不增本库锁/unsafe，
    /// 无借用或锁跨await；原FilesLoad/缓存/运行时的整体使用仍遵守其既有安全合同。
    /// 原请求目录遍历/metadata存在同步I/O，不因登记而成为全流程非阻塞。
    ///
    /// # 成本与示例
    /// 路径m字节/k分量，原重组最坏O(k*m)时间/O(m)额外空间，自定义Into成本另计。
    /// 新增名称n字节格式化为O(n)时间/空间；上游登记O(C+A*n+L+n)时间/额外空间，
    /// C为历史核心数、A为本核原名数、L为旧合并名长度。锁争用、分配器/logger等待
    /// 没有本入口保证的时长上限，不声称构造零成本，也不改变原文件热路径。
    /// 示例仅装配中间件；实际运行时启动、时钟、进程采集及最终注销由宿主负责：
    /// ```no_run
    /// use pi_async_rt::rt::multi_thread::MultiTaskRuntime;
    /// use pi_http::files_load::FilesLoad;
    /// fn build_files(runtime: MultiTaskRuntime<()>) -> FilesLoad {
    ///     FilesLoad::new(runtime, "./htdocs", None, true, true, false, false, 60)
    /// }
    /// ```
    /// 验证：files_load::runtime_metrics_tests::test_files_load_registers_runtime_metrics，
    /// FLS-MET-U01只验证实际参数/原Ok，边界与冻结来源为本地FLS-MET-DESIGN-001。
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
                panic!("Create Http Files Load Failed, reason: {:?}", e);
            }
            Ok(root) => {
                #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
                {
                    // FLS-REGISTER：只借原外部句柄冷登记，身份/强保活归上游，不取得注销权。
                    let metric_name = format!("pi_http-multi-{}", files_async_runtime.get_id());
                    let registration = pi_async_rt::runtime_metrics::register_runtime_metrics(
                        &files_async_runtime, &metric_name,
                    );
                    #[cfg(test)]
                    FILES_LOAD_METRICS_TEST_RESULT.with(|value| {
                        // FLS-OBSERVE：仅复制本次实际参数/纯Result，借用在原日志调用前结束。
                        *value.borrow_mut() = Some((files_async_runtime.get_id(), metric_name.clone(), registration));
                    });
                    if let Err(error) = registration {
                        // 指标失败只告警继续原构造，不重试/关闭/注销共享核心或改变业务返回。
                        warn!("HTTP 批量文件加载运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
                    }
                }
                FilesLoad {
                    files_async_runtime,
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

// 解析后缀，失败返回在解析哪个字符时出错
fn decode(
    chars: &mut Chars,
    suffix: &mut Vec<char>,
    stack: &mut Vec<String>,
    result: &mut Vec<String>,
    mut pos: usize,
) -> GenResult<(), usize> {
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

// 解析指定后缀的文件
fn decode_file(
    chars: &mut Chars,
    suffix: &mut Vec<char>,
    stack: &mut Vec<String>,
    result: &mut Vec<String>,
    mut pos: usize,
) -> GenResult<(), usize> {
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
            let path_file = stack.last().or(Some(&mut "".to_string())).unwrap().clone()
                + name.to_vec().into_iter().collect::<String>().as_str()
                + suffix.to_vec().into_iter().collect::<String>().as_str(); //构建一个指定路径、文件名和后缀名的文件路径
            result.push(path_file); //推入结果集
            name.clear(); //清空文件名
            continue;
        }

        if let Some(')') = char {
            //文件解析完成，继续下一个解析
            let path_file = stack.last().or(Some(&mut "".to_string())).unwrap().clone()
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

// 解析文件路径
fn decode_path(
    chars: &mut Chars,
    stack: &mut Vec<String>,
    result: &mut Vec<String>,
    mut pos: usize,
) -> GenResult<(), usize> {
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

// 解析指定目录下指定后缀的文件，没有后缀即目录下所有文件
fn decode_dir(
    dirs: &mut Vec<String>,
    root: &PathBuf,
    result: &mut Vec<(u64, PathBuf)>,
) -> Result<()> {
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
                        if let Err(e) = disk_files(
                            Some(&OsStr::new(vec[1])),
                            &path.parent().unwrap().to_path_buf().join(vec[0]),
                            result,
                        ) {
                            return Err(e);
                        }
                        len = result.len();
                        result[index..len].sort_by(|(_, x), (_, y)| {
                            x.to_str()
                                .as_mut()
                                .unwrap()
                                .replace("\\", "/")
                                .cmp(&y.to_str().as_mut().unwrap().replace("\\", "/"))
                        });
                        continue;
                    }
                }
            }
            index = result.len();
            if let Err(e) = disk_files(None, &path.parent().unwrap().to_path_buf(), result) {
                return Err(e);
            }
            len = result.len();
            result[index..len].sort_by(|(_, x), (_, y)| {
                x.to_str()
                    .as_ref()
                    .unwrap()
                    .replace("\\", "/")
                    .cmp(&y.to_str().as_ref().unwrap().replace("\\", "/"))
            });
        } else {
            //解析目录下所有文件，形如*/
            index = result.len();
            if let Err(e) = disk_files(None, &path, result) {
                return Err(e);
            }
            len = result.len();
            result[index..len].sort_by(|(_, x), (_, y)| {
                x.to_str()
                    .as_ref()
                    .unwrap()
                    .replace("\\", "/")
                    .cmp(&y.to_str().as_ref().unwrap().replace("\\", "/"))
            });
        }
    }

    Ok(())
}

// 从硬盘上递归读取文件名和大小
fn disk_files(
    suffix: Option<&OsStr>,
    path: &PathBuf,
    result: &mut Vec<(u64, PathBuf)>,
) -> Result<()> {
    if !path.exists() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "Select disk files failed, path: {:?}, reason: path not exist",
                path
            ),
        ));
    }

    match path.read_dir() {
        Err(e) => Err(Error::new(
            ErrorKind::Other,
            format!(
                "Select disk files failed, path: {:?}, reason: {:?}",
                path, e
            ),
        )),
        Ok(dirs) => {
            for dir in dirs {
                if let Ok(entry) = dir {
                    if let Ok(file_type) = entry.file_type() {
                        if file_type.is_dir() {
                            //是目录
                            let _ = disk_files(suffix, &entry.path(), result);
                        } else {
                            //是链接或文件
                            filter_file(suffix, entry, result);
                        }
                    }
                }
            }

            Ok(())
        }
    }
}

// 过滤指定后缀和大小的文件
fn filter_file(suffix: Option<&OsStr>,
               entry: DirEntry,
               result: &mut Vec<(u64, PathBuf)>) {
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
            }
            None => {
                //继续检查以确认是否有后缀名
                let mut path = PathBuf::new();
                path.push(&entry.file_name().to_os_string());
                let vec: Vec<&str> = path
                    .to_str()
                    .as_ref()
                    .unwrap()
                    .split(".")
                    .collect();
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

// 异步批量加载文件，并返回批量加载文件的总大小和总数量
async fn async_load_files(files_async_runtime: MultiTaskRuntime<()>,
                          resp: &HttpResponse,
                          files: Vec<(u64, PathBuf)>) -> Result<(u64, usize)> {
    let mut total_size = 0;
    let mut index: u64 = 0;

    //统计批量加载文件的总大小
    for (size, _) in &files {
        total_size += *size;
    }

    if let Some(resp_handler) = resp.get_response_handler() {
        let unload_size = Arc::new(AtomicU64::new(total_size)); //未加载文件大小

        for (size, file_path) in files {
            if size == 0 {
                //空文件，则忽略
                continue;
            }
            let unload_size_copy = unload_size.clone();
            let resp_handler_copy = resp_handler.clone();
            let path = file_path.clone();
            let files_async_runtime_copy = files_async_runtime.clone();
            if let Err(e) = files_async_runtime.spawn(async move {
                // 调用底层open接口
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
                                let bin_size = bin.len() as u64;
                                if let Err(e) = resp_handler_copy.write_index(index, bin).await {
                                    warn!("Http Body Mut Write Index Failed, index: {:?}, file: {:?}, reason: {:?}",
                                        index,
                                        path,
                                        e);
                                } else {
                                    //发送文件成功，则减去当前文件的大小
                                    let last_size =
                                        unload_size_copy.fetch_sub(bin_size, Ordering::Relaxed);
                                    if last_size == bin_size {
                                        //所有文件已加载完成，则结束响应体的异步写
                                        if let Err(e) = resp_handler_copy.finish().await {
                                            warn!("Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                                path,
                                                e);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                    path,
                                    e
                                );
                                if let Err(e) = resp_handler_copy.finish().await {
                                    warn!(
                                        "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                        path,
                                        e
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
                        if let Err(e) = resp_handler_copy.finish().await {
                            warn!(
                                "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                path,
                                e
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
            //顺序增加文件序号，并统计文件总数量
            index += 1;
        }

        return Ok((total_size, index as usize));
    }

    Err(Error::new(
        ErrorKind::NotFound,
        "Load files error, reason: invalid response body",
    ))
}

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod runtime_metrics_tests {
    //! 只验证FilesLoad::new对外部普通Multi的真实冷登记参数及原Result为Ok。
    //! 记录来自本构造内的实际调用，不依赖先构建FileLoad、不手动预登记或模拟上游。
    //! 不启trace/Provider/SDK、不采集或访问网络/文件，不覆盖原文件业务与上游算法。
    //! 只精确选择本单元串行执行；冻结设计与证据入口为本地FLS-MET-DESIGN-001。

    use super::*;

    /// 被测生产入口：files_load::FilesLoad::new，原Ok(root)内发布前登记外部Multi。
    /// 测试宿主启动原时钟并创建一个worker的真实空Multi，只构造一次无缓存FilesLoad，
    /// 核对真实参数原编号、精确三段名和原Result为Ok，不以返回Self替代登记成功。
    /// 该新核心归测试独占，取走记录后、参数断言前显式注销自己的成功ID；
    /// 不加Drop注销，不测试上游注销算法，不使用原Multi::close伪造停止worker。
    /// 槽只持纯元数据，当前线程同步取走，无跨await借用或在途采样；
    /// 参数核对O(n)时间/空间，n为短名称字节数，原Builder/时钟沿测试进程生命周期退出。
    /// 不承诺原文件解析/缓存/异步读取、SDK、容量、去重或其它HTTP入口已被本单元验证。
    #[test]
    fn test_files_load_registers_runtime_metrics() {
        FILES_LOAD_METRICS_TEST_RESULT.with(|value| { value.borrow_mut().take(); });
        let _timer = pi_async_rt::rt::startup_global_time_loop(10);
        let runtime = pi_async_rt::rt::AsyncRuntimeBuilder::default_multi_thread(
            Some("http-filesload-metrics-unit"), None, Some(1), Some(1),
        );
        let expected_runtime_id = runtime.get_id();
        let expected_name = format!("pi_http-multi-{}", expected_runtime_id);

        let _files = FilesLoad::new(runtime.clone(), ".", None, false, false, false, false, 0);

        let observed = FILES_LOAD_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            // 本测试明确拥有新核心，清理先于断言；不授予业务组件注销共享核心的权力。
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (actual_runtime_id, actual_name, registration) = observed
            .expect("FilesLoad::new必须调用真实登记并记录本次原始结果");
        assert_eq!(actual_runtime_id, expected_runtime_id, "必须登记外部实际注入的运行时");
        assert_eq!(actual_name, expected_name, "登记名称必须精确使用库名、真实类型和原编号");
        assert!(registration.is_ok(), "实际登记必须成功，原始结果: {:?}", registration);
    }
}
