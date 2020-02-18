use std::sync::Arc;
use std::path::PathBuf;
use std::fs::create_dir_all;
use std::io::{Error, Result, ErrorKind};

use https::StatusCode;
use futures::future::{FutureExt, BoxFuture};
use path_absolutize::Absolutize;

use tcp::driver::{Socket, AsyncIOWait};
use handler::SGenType;
use file::file::{AsyncFileOptions, WriteOptions, Shared, SharedFile, AsyncFile};
use log::warn;

use crate::{gateway::GatewayContext,
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::HttpResponse,
            util::HttpRecvResult};

/*
* https文件上传任务优先级
*/
const HTTPS_ASYNC_FILE_UPLOAD_PRIORITY: usize = 100;

/*
* 文件移除方法标记
*/
const FILE_REMOVE_METHOD: &str = "_$remove";

/*
* Http文件上传处理器
*/
pub struct UploadFile {
    root: PathBuf,  //文件上传根路径
}

unsafe impl Send for UploadFile {}
unsafe impl Sync for UploadFile {}

impl<S: Socket, W: AsyncIOWait> Middleware<S, W, GatewayContext> for UploadFile {
    fn request<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>)
                   -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let future = async move {
            let mut is_remove = false;
            let mut file = String::from("");
            let mut content = vec![];

            let map = context.as_mut_parts();
            if let Some(SGenType::Str(method)) = map.get("method") {
                if method == FILE_REMOVE_METHOD {
                    //文件移除
                    is_remove = true;
                    if let Some(SGenType::Str(file_name)) = map.get("file_name") {
                        file = file_name.to_string();
                    } else {
                        return MiddlewareResult::Throw(Error::new(ErrorKind::NotFound, "remove file error, reason: empty relative path"));
                    }
                }
            }

            if !is_remove {
                //不是文件移除，则为文件上传
                if let Some(SGenType::Str(file_name)) = map.get("filename") {
                    file = file_name.to_string();
                } else {
                    return MiddlewareResult::Throw(Error::new(ErrorKind::NotFound, "upload file error, reason: empty relative path"));
                }

                //获取文件内容
                match map.remove("content") {
                    Some(SGenType::Str(str)) => {
                        content = str.into_bytes();
                    },
                    Some(SGenType::Bin(bin)) => {
                        content = bin;
                    },
                    _ => (),
                }
            }

            let file_path = PathBuf::from(&file);
            if !file_path.is_relative() {
                //不是相对路径
                return MiddlewareResult::Throw(Error::new(ErrorKind::NotFound, "upload file error, reason: invalid relative path"));
            }

            let path = self.root.join(file);
            let resp = HttpResponse::new(req.get_handle().clone(), req.get_waits().clone(), 1);
            if let Some(dir) = path.parent() {
                if let Ok(p) = dir.absolutize() {
                    if !p.starts_with(PathBuf::from(&self.root).absolutize().ok().unwrap()) {
                        //标准化后根路径被改变
                        return MiddlewareResult::Throw(Error::new(ErrorKind::Other, "upload file error, reason: absolute path overflow"));
                    }
                }

                if is_remove {
                    //移除文件
                    if !path.exists() {
                        //文件不存在，则立即中止文件上传处理，并返回响应
                        return MiddlewareResult::Throw(Error::new(ErrorKind::Other, "remove file error, reason: invalid file"));
                    } else {
                        if let Err(e) = async_remove_file(&resp, path) {
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
                            return MiddlewareResult::Throw(Error::new(ErrorKind::Other, "upload file error, reason: make relative path failed"));
                        }
                    }
                    if let Err(e) = async_save_file(&resp, path, content) {
                        //存储文件失败
                        return MiddlewareResult::Throw(e);
                    }
                }
            } else {
                //无效的绝对路径
                return MiddlewareResult::Throw(Error::new(ErrorKind::Other, "upload file error, reason: invalid absolute path"));
            }

            //完成请求处理
            MiddlewareResult::Finish((req, resp))
        };
        future.boxed()
    }

    fn response<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>, resp: HttpResponse<S, W>)
                    -> BoxFuture<'a, MiddlewareResult<S, W>> {
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
                        },
                        HttpRecvResult::Ok(bodys) => {
                            //获取到的是Http响应体块的后继
                            for (_index, bin) in bodys {
                                body_bufs.push(bin);
                            }
                        },
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
                                },
                                _ => (), //异步上传或移除文件成功
                            }
                            break;
                        },
                    }
                }
            }

            //继续响应处理
            MiddlewareResult::ContinueResponse((req, response))
        };
        future.boxed()
    }
}

impl UploadFile {
    //构建指定根目录的文件上传处理器
    pub fn new<P: Into<PathBuf>>(dir: P) -> Self {
        let root = dir.into();
        if !root.exists() {
            //不存在，则创建根目录
            if create_dir_all(&root).is_err() {
                //创建根目录失败
                panic!("New UploadFile Failed, make root failed, root: {:?}", root);
            }
        }

        UploadFile {
            root,
        }
    }
}

//异步移除文件
fn async_remove_file<S: Socket, W: AsyncIOWait>(resp: &HttpResponse<S, W>, path: PathBuf) -> Result<()> {
    if let Some(resp_handler) = resp.get_response_handler() {
        let path_copy = path.clone();
        let remove = Box::new(move |r: Result<()>| {
            match r {
                Err(e) => {
                    //移除文件失败
                    warn!("!!!> Http Async Remove File Failed, file: {:?}, reason: {:?}", path_copy, e);
                    if let Err(e) = resp_handler.write(Vec::from(format!("remove file error, reason: {:?}", e).as_bytes())) {
                        warn!("!!!> Http Body Mut Write Failed, file: {:?}, reason: {:?}", path_copy, e);
                    } else {
                        if let Err(e) = resp_handler.finish() {
                            warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", path_copy, e);
                        }
                    }
                },
                Ok(_) => {
                    //移除文件成功
                    if let Err(e) = resp_handler.finish() {
                        warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", path_copy, e);
                    }
                },
            }
        });
        AsyncFile::remove(path, remove);

        return Ok(());
    }

    Err(Error::new(ErrorKind::Other, "remove file error, reason: invalid response body"))
}

//异步存储文件
fn async_save_file<S: Socket, W: AsyncIOWait>(resp: &HttpResponse<S, W>, path: PathBuf, content: Vec<u8>) -> Result<()> {
    if let Some(resp_handler) = resp.get_response_handler() {
        let path_copy = path.clone();
        let open = Box::new(move |f: Result<AsyncFile>| {
            match f {
                Err(e) => {
                    //打开文件失败
                    warn!("!!!> Http Async Open File Failed, file: {:?}, reason: {:?}", path_copy, e);
                    if let Err(e) = resp_handler.write(Vec::from(format!("upload file error, reason: {:?}", e).as_bytes())) {
                        warn!("!!!> Http Body Mut Write Failed, file: {:?}, reason: {:?}", path_copy, e);
                    } else {
                        if let Err(e) = resp_handler.finish() {
                            warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", path_copy, e);
                        }
                    }
                },
                Ok(r) => {
                    //打开文件成功
                    let content_size = content.len();
                    let file = Arc::new(r);
                    let write = Box::new(move |_: SharedFile, result: Result<usize>| {
                        match result {
                            Err(e) => {
                                //写文件失败
                                warn!("!!!> Http Async Write File Failed, file: {:?}, reason: {:?}", path_copy, e);
                                if let Err(e) = resp_handler.write(Vec::from(format!("upload file error, reason: {:?}", e).as_bytes())) {
                                    warn!("!!!> Http Body Mut Write Failed, file: {:?}, reason: {:?}", path_copy, e);
                                } else {
                                    if let Err(e) = resp_handler.finish() {
                                        warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", path_copy, e);
                                    }
                                }
                            },
                            Ok(size) => {
                                //写文件成功
                                if let Err(e) = resp_handler.finish() {
                                    warn!("!!!> Http Body Mut Finish Failed, file: {:?}, reason: {:?}", path_copy, e);
                                }
                            },
                        }
                    });
                    file.pwrite(WriteOptions::Flush, 0, content, write);
                },
            }
        });
        AsyncFile::open(path, AsyncFileOptions::TruncateWrite(1), open);

        return Ok(());
    }

    Err(Error::new(ErrorKind::Other, "upload file error, reason: invalid response body"))
}