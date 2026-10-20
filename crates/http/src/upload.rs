//! HTTP文件上传；仅在原根目录准备成功后/发布前登记外部普通Multi。
//! 不改变上传/删除、目录创建、原错误或派发逻辑，无生产结果/ID或生命周期管理。
//! F01/B-UPLOAD；参数/Ok专项：batch_runtime_metrics_tests::test_upload_registers_runtime_metrics。

use std::fs::create_dir_all;
use std::io::{Error, ErrorKind, Result};
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::{FutureExt, LocalBoxFuture};
use https::StatusCode;
use path_absolutize::Absolutize;

use pi_async_file::file::{remove_file, AsyncFile, AsyncFileOptions, WriteOptions};
use pi_handler::SGenType;
use log::warn;
use pi_async_rt::rt::{AsyncRuntime, multi_thread::MultiTaskRuntime};
use tcp::Socket;
use serde_json::{self, Value};
use pi_hash::XHashMap;

use crate::{
    gateway::GatewayContext,
    middleware::{Middleware, MiddlewareResult},
    request::HttpRequest,
    response::HttpResponse,
    utils::{trim_path, HttpRecvResult},
};

///
/// https文件上传任务优先级
///
const HTTPS_ASYNC_FILE_UPLOAD_PRIORITY: usize = 100;

///
/// 文件移除方法标记
///
const FILE_REMOVE_METHOD: &str = "_$remove";

///
/// Http文件上传处理器
///
pub struct UploadFile {
    files_async_runtime:    MultiTaskRuntime<()>,   //异步文件运行时
    root:                   PathBuf,                //文件上传根路径
}

unsafe impl Send for UploadFile {}
unsafe impl Sync for UploadFile {}

impl<S: Socket> Middleware<S, GatewayContext> for UploadFile {
    fn request<'a>(
        &'a self,
        context: &'a mut GatewayContext,
        req: HttpRequest<S>,
    ) -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let future = async move {
            let mut is_remove = false;
            let mut file = String::from("");
            let mut content = vec![];

            let mut json_map = None;
            if !context.as_mut_parts().contains_key("method")
                && !context.as_mut_parts().contains_key("file_name")
                && !context.as_mut_parts().contains_key("filename")
                && !context.as_mut_parts().contains_key("content") {
                if let Some(SGenType::Str(str)) = context.as_params().borrow().get("") {
                    if let Ok(value) = serde_json::from_str::<Value>(str) {
                        let mut map = XHashMap::default();
                        for (key, value) in value.as_object().unwrap() {
                            if value.is_string() {
                                map
                                    .insert(key.to_string(),
                                            SGenType::Str(value.as_str().unwrap().to_string()));
                            }
                        }
                        json_map = Some(map);
                    }
                }
            }
            let map = if let Some(map) = &mut json_map {
                map
            } else {
                context.as_mut_parts()
            };

            if let Some(SGenType::Str(method)) = map.get("method") {
                if method == FILE_REMOVE_METHOD {
                    //文件移除
                    is_remove = true;
                    if let Some(SGenType::Str(file_name)) = map.get("file_name") {
                        file = file_name.to_string();
                    } else {
                        return MiddlewareResult::Throw(Error::new(
                            ErrorKind::NotFound,
                            "Remove file error, reason: empty relative path",
                        ));
                    }
                }
            }

            if !is_remove {
                //不是文件移除，则为文件上传
                if let Some(SGenType::Str(file_name)) = map.get("filename") {
                    file = file_name.to_string();
                } else {
                    return MiddlewareResult::Throw(Error::new(
                        ErrorKind::NotFound,
                        "Upload file error, reason: empty relative path",
                    ));
                }

                //获取文件内容
                match map.remove("content") {
                    Some(SGenType::Str(str)) => {
                        content = str.into_bytes();
                    }
                    Some(SGenType::Bin(bin)) => {
                        content = bin;
                    }
                    _ => (),
                }
            }

            let file_path = PathBuf::from(&file);
            if !file_path.is_relative() {
                //不是相对路径
                return MiddlewareResult::Throw(Error::new(
                    ErrorKind::NotFound,
                    "Upload file error, reason: invalid relative path",
                ));
            }

            let path = self.root.join(file);
            let resp = HttpResponse::new(2);
            if let Some(dir) = path.parent() {
                if let Ok(p) = dir.absolutize() {
                    if !p.starts_with(PathBuf::from(&self.root).absolutize().ok().unwrap()) {
                        //标准化后根路径被改变
                        return MiddlewareResult::Throw(Error::new(
                            ErrorKind::Other,
                            "Upload file error, reason: absolute path overflow",
                        ));
                    }
                }

                if is_remove {
                    //移除文件
                    if !path.exists() {
                        //文件不存在，则立即中止文件上传处理，并返回响应
                        return MiddlewareResult::Throw(Error::new(
                            ErrorKind::Other,
                            "Remove file error, reason: invalid file",
                        ));
                    } else {
                        if let Err(e) =
                            async_remove_file(self.files_async_runtime.clone(),
                                              &resp,
                                              path).await
                        {
                            //移除文件失败
                            return MiddlewareResult::Throw(e);
                        }
                    }
                } else {
                    //上传文件
                    if !dir.exists() {
                        //路径不存在
                        if create_dir_all(dir).is_err() {
                            //创建子目录失败
                            return MiddlewareResult::Throw(Error::new(
                                ErrorKind::Other,
                                "Upload file error, reason: make relative path failed",
                            ));
                        }
                    }
                    if let Err(e) =
                        async_save_file(self.files_async_runtime.clone(),
                                        &resp,
                                        path,
                                        content).await
                    {
                        //存储文件失败
                        return MiddlewareResult::Throw(e);
                    }
                }
            } else {
                //无效的绝对路径
                return MiddlewareResult::Throw(Error::new(
                    ErrorKind::Other,
                    "Upload file error, reason: invalid absolute path",
                ));
            }

            //完成请求处理
            MiddlewareResult::Finish((req, resp))
        };
        future.boxed_local()
    }

    fn response<'a>(
        &'a self,
        _context: &'a mut GatewayContext,
        req: HttpRequest<S>,
        resp: HttpResponse,
    ) -> LocalBoxFuture<'a, MiddlewareResult<S>> {
        let mut body_bufs: Vec<Vec<u8>> = Vec::new();
        let mut response = resp;
        let future = async move {
            if let Some(body) = response.as_mut_body() {
                //当前响应有响应体，则持续获取响应体的内容
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
                                Some(body_size) if body_size > 0 => {
                                    //异步上传或移除文件失败，则设置响应状态为错误
                                    response.status(StatusCode::INTERNAL_SERVER_ERROR.as_u16());
                                }
                                _ => (), //异步上传或移除文件成功
                            }
                            break;
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

// 仅测试观测真实参数/原Result；不持核心，不生成生产状态或公开查询。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static UPLOAD_METRICS_TEST_RESULT: std::cell::RefCell<Option<(usize, String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod batch_runtime_metrics_tests {
    //! 只验证UploadFile::new实际自动登记的原Multi/名称参数及Ok，不以Self成功代替登记。
    //! 使用测试自有真实空Multi，无网络/采集/SDK/容量/去重测试，不代表文件业务通过。
    //! 槽仅纯测试可见，断言前显式注销本测试自己的ID；原worker沿测试进程生命周期退出。

    use super::*;

    /// 被测生产入口：UploadFile::new的原准备完成/发布前冷登记。
    /// "."只用于原构造，UploadFile可能读取目录元数据但不新建目录，不执行文件任务；
    /// 命名核对O(L)时间/空间，无跨await借用，严格串行且只断言实际参数与原Ok。
    #[test]
    fn test_upload_registers_runtime_metrics() {
        UPLOAD_METRICS_TEST_RESULT.with(|value| { value.borrow_mut().take(); });
        let _clock = pi_async_rt::rt::startup_global_time_loop(10);
        let runtime = pi_async_rt::rt::AsyncRuntimeBuilder::default_multi_thread(
            Some("http-cold-metrics-unit"), None, Some(1), Some(1),
        );
        let expected_id = runtime.get_id();
        let _middleware = UploadFile::new(runtime.clone(), ".");
        let observed = UPLOAD_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (actual_id, actual_name, registration) = observed.expect("原构造必须实际执行冷登记");
        assert_eq!(actual_id, expected_id, "必须使用实际注入核心");
        assert_eq!(actual_name, format!("pi_http-multi-{}", expected_id));
        assert!(registration.is_ok(), "真实登记必须成功: {:?}", registration);
    }
}

impl UploadFile {
    /// 构建指定根目录的文件上传处理器
    ///
    /// files_async_runtime为外部共享普通Multi，dir按原Into<PathBuf>/trim_path重组；
    /// 目录不存在仍同步创建，失败沿原panic。准备成功后默认冷登记pi_http-multi-原编号，
    /// 指标失败只warn后返回原Self，不新增字段/业务错误/注销。简单装配：
    /// ```no_run
    /// # fn build(rt: pi_async_rt::rt::multi_thread::MultiTaskRuntime<()>) {
    /// let middleware = pi_http::upload::UploadFile::new(rt, ".");
    /// # }
    /// ```
    /// 原目录I/O可能阻塞；名称O(L)时间/空间，加上游短同步锁/分配和logger成本。
    /// 无新增await/异步锁/任务，非纯函数；重复构造不等于新核心，登记幂等归上游。
    /// 仅冷初始化使用，不进入请求热路径；强保活由上游持有，宿主负责共享核心生命周期，
    /// 本组件不在Drop/失败中注销，原unsafe/线程域不变。专项仅验证实际登记参数/Ok。
    pub fn new<P: Into<PathBuf>>(files_async_runtime: MultiTaskRuntime<()>,
                                 dir: P) -> Self {
        match trim_path(dir) {
            Err(e) => {
                panic!("Create Http Upload Failed, reason: {:?}", e);
            }
            Ok(root) => {
                if !root.exists() {
                    //不存在，则创建根目录
                    if create_dir_all(&root).is_err() {
                        //创建根目录失败
                        panic!("New UploadFile Failed, make root failed, root: {:?}", root);
                    }
                }

                #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
                {
                    let metric_name = format!("pi_http-multi-{}", files_async_runtime.get_id());
                    let registration = pi_async_rt::runtime_metrics::register_runtime_metrics(&files_async_runtime, &metric_name);
                    #[cfg(test)]
                    UPLOAD_METRICS_TEST_RESULT.with(|value| {
                        *value.borrow_mut() = Some((files_async_runtime.get_id(), metric_name.clone(), registration));
                    });
                    if let Err(error) = registration {
                        warn!("HTTP 上传运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
                    }
                }
                UploadFile {
                    files_async_runtime,
                    root,
                }
            }
        }
    }
}

// 异步移除文件
async fn async_remove_file(files_async_runtime: MultiTaskRuntime<()>,
                           resp: &HttpResponse,
                           path: PathBuf) -> Result<()> {
    let files_async_runtime_copy = files_async_runtime.clone();
    if let Some(resp_handler) = resp.get_response_handler() {
        let path_copy = path.clone();
        if let Err(e) = files_async_runtime.spawn(async move {
            // 调用底层open接口
            let r = remove_file(files_async_runtime_copy,
                                path_copy.clone()).await;
            match r {
                Ok(_) => {
                    //移除文件成功
                    if let Err(e) = resp_handler.finish().await {
                        warn!(
                            "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                            path_copy,
                            e
                        );
                    }
                }
                Err(e) => {
                    //移除文件失败
                    warn!(
                        "Http Async Remove File Failed, file: {:?}, reason: {:?}",
                        path_copy,
                        e
                    );
                    if let Err(e) = resp_handler.write(Vec::from(
                        format!("remove file error, reason: {:?}", e).as_bytes(),
                    )).await {
                        warn!(
                            "Http Body Mut Write Failed, file: {:?}, reason: {:?}",
                            path_copy,
                            e
                        );
                    } else {
                        if let Err(e) = resp_handler.finish().await {
                            warn!(
                                "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                path_copy,
                                e
                            );
                        }
                    }
                }
            }
        }) {
            warn!("Http Async Open File Failed, reason: {:?}",
                e);
        }

        return Ok(());
    }

    Err(Error::new(
        ErrorKind::Other,
        "Remove file error, reason: invalid response body",
    ))
}

// 异步存储文件
async fn async_save_file(files_async_runtime: MultiTaskRuntime<()>,
                         resp: &HttpResponse,
                         path: PathBuf,
                         content: Vec<u8>) -> Result<()> {
    if let Some(resp_handler) = resp.get_response_handler() {
        let path_copy = path.clone();
        let files_async_runtime_copy = files_async_runtime.clone();
        if let Err(e) = files_async_runtime.spawn(async move {
            // 调用底层open接口
            let file = AsyncFile::open(
                files_async_runtime_copy,
                path_copy.clone(),
                AsyncFileOptions::ReadWrite,
            ).await;
            match file {
                Ok(file) => {
                    // 打开文件成功
                    let file = Arc::new(file);
                    match file.write(0, content, WriteOptions::Flush).await {
                        Ok(_) => {
                            //写文件成功
                            if let Err(e) = resp_handler.finish().await {
                                warn!("Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                    path_copy,
                                    e);
                            }
                        }
                        Err(e) => {
                            //写文件失败
                            warn!(
                                "Http Async Write File Failed, file: {:?}, reason: {:?}",
                                path_copy,
                                e
                            );
                            if let Err(e) = resp_handler.write(Vec::from(
                                format!("Upload file error, reason: {:?}", e).as_bytes(),
                            )).await {
                                warn!(
                                    "Http Body Mut Write Failed, file: {:?}, reason: {:?}",
                                    path_copy,
                                    e
                                );
                            } else {
                                if let Err(e) = resp_handler.finish().await {
                                    warn!("Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                        path_copy,
                                        e);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    //打开文件失败
                    warn!(
                        "Http Async Open File Failed, file: {:?}, reason: {:?}",
                        path_copy,
                        e
                    );
                    if let Err(e) = resp_handler.write(Vec::from(
                        format!("Upload file error, reason: {:?}", e).as_bytes(),
                    )).await {
                        warn!(
                            "Http Body Mut Write Failed, file: {:?}, reason: {:?}",
                            path_copy,
                            e
                        );
                    } else {
                        if let Err(e) = resp_handler.finish().await {
                            warn!(
                                "Http Body Mut Finish Failed, file: {:?}, reason: {:?}",
                                path_copy,
                                e
                            );
                        }
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
        ErrorKind::Other,
        "Upload file error, reason: invalid response body",
    ))
}
