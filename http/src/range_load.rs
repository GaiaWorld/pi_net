use std::sync::Arc;
use std::io::{Error, Result, ErrorKind};

use https::{header::{RANGE, ACCEPT_RANGES, CONTENT_RANGE, CONTENT_LENGTH}, StatusCode};
use futures::future::{FutureExt, BoxFuture};

use tcp::driver::{Socket, AsyncIOWait};
use handler::SGenType;
use log::warn;

use crate::{gateway::GatewayContext,
            middleware::{MiddlewareResult, Middleware},
            request::HttpRequest,
            response::HttpResponse,
            util::HttpRecvResult};

/*
* Http静态资源范围加载器
*/
pub struct RangeLoad;

unsafe impl Send for RangeLoad {}
unsafe impl Sync for RangeLoad {}

impl<S: Socket, W: AsyncIOWait> Middleware<S, W, GatewayContext> for RangeLoad {
    fn request<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>)
                   -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let future = async move {
            //继续请求处理
            MiddlewareResult::ContinueRequest(req)
        };
        future.boxed()
    }

    fn response<'a>(&'a self, context: &'a mut GatewayContext, req: HttpRequest<S, W>, resp: HttpResponse<S, W>)
                    -> BoxFuture<'a, MiddlewareResult<S, W>> {
        let mut response = resp;
        let future = async move {
            if let Some(range_value) = req.headers().get(RANGE) {
                if let Ok(r) = range_value.to_str() {
                    if let None = r.find(',') {
                        //只支持单个指定范围
                        let str = r.trim().to_lowercase();
                        let tmp: Vec<&str> = str.split("=").collect();
                        let range_str = tmp[1].trim();
                        let mut vec: Vec<&str> = range_str.split("-").collect();
                        if let Ok(start) = vec[0].parse::<usize>() {
                            //获取开始范围成功，则获取响应体大小
                            let mut body_len = 0;
                            if let Some(body) = response.as_body() {
                                if let Some(len) = body.len() {
                                    body_len = len;
                                }
                            }

                            let mut end = body_len - 1; //未指定结束范围，则默认为所有数据
                            if let Ok(e) = vec[1].parse::<usize>() {
                                //已指定结束范围
                                end = e;
                            }

                            if start > end || end > body_len {
                                //客户端需要的静态资源范围越界，则立即返回错误
                                if let Some(body) = response.as_mut_body() {
                                    body.reset(&[]);
                                }
                                response
                                    .status(StatusCode::RANGE_NOT_SATISFIABLE.as_u16())
                                    .header(ACCEPT_RANGES.as_str(), "bytes")
                                    .header(CONTENT_LENGTH.as_str(), "0");
                                return MiddlewareResult::Break(response);
                            }

                            if let Some(body) = response.as_mut_body() {
                                let mut buf = Vec::with_capacity(0);
                                if let Some(bin) = body.as_slice() {
                                    if start == end {
                                        buf = Vec::from(&bin[start..(start + 1)]);
                                    } else {
                                        buf = Vec::from(&bin[start..(end - start + 1)]);
                                    }
                                }
                                body.reset(buf.as_slice()); //重置响应体为指定范围的数据
                            }

                            //设置范围响应状态码和响应头
                            response
                                .status(StatusCode::PARTIAL_CONTENT.as_u16())
                                .header(ACCEPT_RANGES.as_str(), "bytes")
                                .header(CONTENT_RANGE.as_str(), ["bytes", " ", range_str, "/", body_len.to_string().as_str()].concat().as_str());
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

impl RangeLoad {
    //构建指定根目录的文件上传处理器
    pub fn new() -> Self {
        RangeLoad
    }
}