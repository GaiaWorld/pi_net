use std::pin::Pin;
use std::sync::Arc;
use std::cell::RefCell;
use std::future::Future;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::task::{Context, Poll, Waker};
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::{FutureExt, BoxFuture};
use crossbeam_channel::{Sender, Receiver, unbounded, TryRecvError};
use bytes::{Buf, BufMut, IntoBuf, BytesMut};
use url::Url;
use httparse::{EMPTY_HEADER, Status, Header, Request};
use https::{Result as HttpResult, Response, Version,
            header::{HeaderName, HeaderValue, HeaderMap},
            status::StatusCode};
use log::warn;

use tcp::driver::{Socket, AsyncIOWait, SocketHandle, AsyncReadTask, AsyncWriteTask};

use crate::{util::{DEFAULT_SUPPORT_HTTP_VERSION, HttpSender, HttpReceiver, channel}};
use tcp::connect::TcpSocket;
use tcp::server::AsyncWaitsHandle;

/*
* 默认读取Http请求的字节长度
*/
pub const DEFAULT_READ_READY_HTTP_REQUEST_BYTE_LEN: usize = 0;

/*
* 上行请求头
*/
pub struct UpStreamHeader;

unsafe impl Send for UpStreamHeader {}
unsafe impl Sync for UpStreamHeader {}

/*
* 上行请求头异步方法
*/
impl UpStreamHeader {
    //读请求，并解析报文头
    pub async fn read_header<'h, 'b, S, W>(handle: SocketHandle<S>,
                                           waits: W,
                                           mut req: Request<'h, 'b>,
                                           mut headers: HeaderMap) -> Option<(Request<'h, 'b>, HeaderMap, Vec<u8>, usize)>
        where S: Socket,
              W: AsyncIOWait {
        let buf = Box::into_raw(Box::new(Vec::<u8>::new())) as usize;
        loop {
            match AsyncReadTask::async_read(handle.clone(), waits.clone(), DEFAULT_READ_READY_HTTP_REQUEST_BYTE_LEN).await {
                Err(e) => {
                    handle.close(Err(Error::new(ErrorKind::Other, format!("http server read header failed, reason: {:?}", e))));
                    return None;
                },
                Ok(bin) => {
                    unsafe { (&mut *(buf as *mut Vec<u8>)).put(bin); }
                    match req.parse(unsafe { (&*(buf as *const Vec<u8>)).as_slice() }) {
                        Err(e) => {
                            //解析Http头错误
                            handle.close(Err(Error::new(ErrorKind::Other, format!("http server parse header failed, reason: {:?}", e))));
                            return None;
                        },
                        Ok(ref status) if status.is_partial() => {
                            //部分头数据已到达
                            match req.version {
                                Some(ver) if ver != DEFAULT_SUPPORT_HTTP_VERSION => {
                                    //不合法的Http版本号
                                    handle.close(Err(Error::new(ErrorKind::Other, format!("http server parse header failed, version: {}, reason: not support http version", ver))));
                                    return None;
                                },
                                _ => {
                                    //头数据不完整，则继续读
                                    continue;
                                }
                            }
                        },
                        Ok(status) => {
                            //全部头数据已到达，则继续读取，并解析体数据
                            if let Status::Complete(len) = status {
                                if let Err(e) = fill_headers(&mut headers, &mut req) {
                                    handle.close(Err(e));
                                    return None;
                                }

                                let vec = unsafe {
                                    Box::from_raw(buf as *mut Vec<u8>)
                                };
                                return Some((req, headers, *vec, len));
                            }

                            return None;
                        },
                    }
                },
            }
        }
    }
}

//将报文中的Http头填充到头映射表中
pub fn fill_headers<'h, 'b>(headers: &mut HeaderMap, req: & mut Request<'h, 'b>) -> Result<usize> {
    let mut count = 0;
    for header in req.headers.iter() {
        match HeaderName::try_from(header.name) {
            Err(e) => {
                return Err(Error::new(ErrorKind::InvalidData, e));
            },
            Ok(key) => {
                match HeaderValue::from_bytes(header.value) {
                    Err(e) => {
                        return Err(Error::new(ErrorKind::InvalidData, e));
                    },
                    Ok(value) => {
                        //构建值成功，则写入头信息表
                        headers.insert(key, value);
                        count += 1;
                    },
                }
            },
        }
    }

    Ok(count)
}