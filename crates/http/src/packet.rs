use std::convert::TryFrom;
use std::io::{Error, Result, ErrorKind};
use bytes::Buf;
use httparse::{Status, Request};
use https::{header::{HeaderName, HeaderValue, HeaderMap}};

use tcp::{Socket, SocketHandle};

use crate::utils::DEFAULT_SUPPORT_HTTP_VERSION;

///
/// 默认读取Http请求的字节长度
///
pub const DEFAULT_READ_READY_HTTP_REQUEST_BYTE_LEN: usize = 0;

///
/// 上行请求头
///
pub struct UpStreamHeader;

unsafe impl Send for UpStreamHeader {}
unsafe impl Sync for UpStreamHeader {}

impl UpStreamHeader {
    /// 读请求，并解析报文头
    pub async fn read_header<'h, 'b, S>(handle: SocketHandle<S>,
                                        buf: &'b [u8],
                                        req: &mut Request<'h, 'b>,
                                        headers: &mut HeaderMap) -> Result<Option<usize>>
        where 'b: 'h,
              S: Socket {
        match req.parse(buf) {
            Err(e) => {
                //解析Http头错误
                let _ = handle.close(Err(Error::new(ErrorKind::Other,
                                            format!("Http server parse header failed, token: {:?}, remote: {:?}, local: {:?}, reason: parse request header error, buf_len: {:?}, buf: {:?}, reason: {:?}",
                                                    handle.get_token(),
                                                    handle.get_remote(),
                                                    handle.get_local(),
                                                    buf.len(),
                                                    buf,
                                                    e))));

                return Err(Error::new(ErrorKind::Other,
                                      format!("Http server parse header failed, token: {:?}, remote: {:?}, local: {:?}, reason: parse request header error, buf_len: {:?}, buf: {:?}",
                                              handle.get_token(),
                                              handle.get_remote(),
                                              handle.get_local(),
                                              buf.len(),
                                              buf)));
            },
            Ok(ref status) if status.is_partial() => {
                //部分头数据已到达
                match req.version {
                    Some(ver) if ver != DEFAULT_SUPPORT_HTTP_VERSION => {
                        //不合法的Http版本号
                        let _ = handle.close(Err(Error::new(ErrorKind::Other,
                                                    format!("Http server parse header failed, token: {:?}, remote: {:?}, local: {:?}, version: {}, reason: not support http version",
                                                            handle.get_token(),
                                                            handle.get_remote(),
                                                            handle.get_local(),
                                                            ver))));

                        return Err(Error::new(ErrorKind::Other,
                                              format!("Http server parse header failed, token: {:?}, remote: {:?}, local: {:?}, version: {}, reason: not support http version",
                                                      handle.get_token(),
                                                      handle.get_remote(),
                                                      handle.get_local(),
                                                      ver)));
                    },
                    _ => {
                        //头数据不完整，则继续读
                        return Ok(None);
                    }
                }
            },
            Ok(status) => {
                //全部头数据已到达，则继续读取，并解析体数据
                if let Status::Complete(len) = status {
                    if let Some(bin) = unsafe { &mut *handle.get_read_buffer().get() } {
                        let _ = bin.copy_to_bytes(len); //消耗请求头的数据
                    }

                    if let Err(e) = fill_headers(headers, req) {
                        let _ = handle.close(Err(e));

                        return Err(Error::new(ErrorKind::Other,
                                              format!("Http server fill header failed, token: {:?}, remote: {:?}, local: {:?}, reason: fill headers error",
                                                      handle.get_token(),
                                                      handle.get_remote(),
                                                      handle.get_local())));
                    }

                    return Ok(Some(len));
                }

                Err(Error::new(ErrorKind::Other,
                               format!("Http server parse header failed, token: {:?}, remote: {:?}, local: {:?}, reason: unknown error",
                                       handle.get_token(),
                                       handle.get_remote(),
                                       handle.get_local())))
            },
        }
    }
}

/// 将报文中的Http头填充到头映射表中
pub fn fill_headers<'h, 'b>(headers: &mut HeaderMap,
                            req: & mut Request<'h, 'b>) -> Result<usize> {
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