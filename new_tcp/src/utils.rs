use std::ptr;
use std::fs::File;
use std::pin::Pin;
use std::path::Path;
use std::fmt::format;
use std::result::Result;
use std::future::Future;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::cell::{RefCell, Ref, RefMut};
use std::task::{Poll, Context, Waker};
use std::sync::{Arc, atomic::{AtomicU8, Ordering}};
use std::io::{Read, BufReader, Result as IOResult, Error, ErrorKind};

use crossbeam_channel::Sender;
use parking_lot::RwLock;
use mio::Token;
use rustls::{ALL_CIPHER_SUITES, ProtocolVersion, RootCertStore, Certificate, PrivateKey, ClientConfig, ServerConfig,
             server::{NoClientAuth,
                      AllowAnyAuthenticatedClient,
                      AllowAnyAnonymousOrAuthenticatedClient,
                      ServerSessionMemoryCache}};
use rustls_pemfile;

use pi_async::{lock::spin_lock::SpinLock,
               rt::{AsyncRuntime, AsyncWaitResult,
                    worker_thread::WorkerRuntime}};
use pi_hash::XHashMap;

use crate::{Socket, SocketHandle, Stream};

///
/// Tcp连接池发送器表
///
lazy_static! {
    pub static ref TCP_SOCKET_POOL_SENDER_TAB: Arc<RwLock<XHashMap<u8, Sender<(Token, IOResult<()>)>>>> = Arc::new(RwLock::new(XHashMap::default()));
}

/// 线程安全的注册Tcp连接池的关闭事件发送器
pub fn register_close_sender(uid: u8, sender: Sender<(Token, IOResult<()>)>) {
    TCP_SOCKET_POOL_SENDER_TAB.write().insert(uid, sender);
}

/// 线程安全的关闭指定唯一id的Tcp连接
pub fn close_socket(uid: usize, reason: IOResult<()>) -> bool {
    let pool_uid = (uid >> 24 & 0xff) as u8;
    let token = Token(uid & 0xffffff);
    if let Some(sender) = TCP_SOCKET_POOL_SENDER_TAB.read().get(&pool_uid) {
        sender.send((token, reason));
        return true;
    }

    false
}

///
/// 传输层安全协议配置
///
#[derive(Clone)]
pub enum TlsConfig {
    Empty,                      //空
    Server(Arc<ServerConfig>),  //服务器配置
    Client(Arc<ClientConfig>),  //客户端配置
}

impl TlsConfig {
    //构建空配置
    pub fn empty() -> Self {
        TlsConfig::Empty
    }

    //构建指定的传输层安全协议的服务器配置
    pub fn new_server(client_auth_path: &str,       //授权客户端证书路径
                      is_client_auth: bool,         //是否强制进行客户端身份认证
                      server_certs_path: &str,      //服务器证书路径
                      server_key_path: &str,        //服务器私钥路径
                      server_ocsp_path: &str,       //服务器OCSP响应文件路径
                      server_suite: &str,           //服务器使用的密码套件名称列表
                      server_versions: &str,        //服务器使用的Tls版本名称列表
                      server_session_size: usize,   //服务器Tls会话缓存的最大容量
                      is_server_tickets: bool,      //服务器是否分配客户端会话票据
                      server_alpns: &str            //服务器支持的ALPN协议名称列表
    ) -> IOResult<Self> {
        let client_auth_path = if client_auth_path.is_empty() {
            None
        } else {
            Some(Path::new(client_auth_path))
        };
        let server_certs_path = Path::new(server_certs_path);
        let server_key_path = Path::new(server_key_path);
        let server_ocsp_path = if server_ocsp_path.is_empty() {
            None
        } else {
            Some(Path::new(server_ocsp_path))
        };
        let server_suite = if server_suite.is_empty() {
            vec![]
        } else {
            server_suite.split(",").into_iter().map(|suite| {
                suite.to_string()
            }).collect::<Vec<String>>()
        };
        let server_versions = if server_versions.is_empty() {
            vec![]
        } else {
            server_versions.split(",").into_iter().map(|version| {
                version.to_string()
            }).collect::<Vec<String>>()
        };
        let server_alpns = if server_alpns.is_empty() {
            vec![]
        } else {
            server_alpns.split(",").into_iter().map(|protocol| {
                protocol.to_string()
            }).collect::<Vec<String>>()
        };

        match make_server_config(client_auth_path,
                                 is_client_auth,
                                 server_certs_path,
                                 server_key_path,
                                 server_ocsp_path,
                                 server_suite,
                                 server_versions,
                                 server_session_size,
                                 is_server_tickets,
                                 server_alpns) {
            Err(e) => Err(e),
            Ok(config) => Ok(TlsConfig::Server(config)),
        }
    }

    //构建指定的传输层安全协议的客户端配置
    pub fn new_client() -> Result<Self, String> {
        unimplemented!();
    }

    //判断是否是空配置
    pub fn is_empty(&self) -> bool {
        if let TlsConfig::Empty = self {
            return true;
        }

        false
    }

    //判断是否是服务器配置
    pub fn is_server(&self) -> bool {
        if let TlsConfig::Server(_) = self {
            return true;
        }

        false
    }

    //判断是否是客户端配置
    pub fn is_client(&self) -> bool {
        if let TlsConfig::Client(_) = self {
            return true;
        }

        false
    }

    //获取支持的所有密码套件名称列表
    pub fn all_suites(&self) -> Vec<String> {
        let mut vec = Vec::new();
        for suite in ALL_CIPHER_SUITES {
            vec.push(format!("{:?}", suite).to_lowercase());
        }

        return vec;
    }

    //获取支持的所有Tls协议名称列表
    pub fn all_versions(&self) -> Vec<String> {
        vec![format!("{:?}", ProtocolVersion::TLSv1_2).to_lowercase(), format!("{:?}", ProtocolVersion::TLSv1_3).to_lowercase()]
    }
}

//生成服务器配置
fn make_server_config(client_auth_path: Option<&Path>,
                      is_client_auth: bool,
                      server_certs_path: &Path,
                      server_key_path: &Path,
                      server_ocsp_path: Option<&Path>,
                      server_suite: Vec<String>,
                      server_versions: Vec<String>,
                      server_session_size: usize,
                      is_server_tickets: bool,
                      server_alpns: Vec<String>) -> IOResult<Arc<rustls::ServerConfig>> {
    let client_auth = if let Some(path) = client_auth_path {
        //配置了客户端授权路径，则构建客户端验证器
        match load_certs(path) {
            Err(e) => {
                //加载客户端授权路径的根证书错误，则立即返回错误原因
                return Err(e);
            },
            Ok(roots) => {
                //加载客户端授权路径的证书成功，则继续
                let mut client_auth_roots = RootCertStore::empty();
                for root in roots {
                    if let Err(e) = client_auth_roots.add(&root) {
                        //缓存客户端授权的根证书错误，则立即返回错误原因
                        return Err(Error::new(ErrorKind::Other,
                                              format!("Save client auth root failed, reason: {:?}", e)));
                    }
                }

                if is_client_auth {
                    //强制验证客户端身份
                    AllowAnyAuthenticatedClient::new(client_auth_roots)
                } else {
                    //不强制验证客户端身份
                    AllowAnyAnonymousOrAuthenticatedClient::new(client_auth_roots)
                }
            },
        }
    } else {
        //未配置客户端授权路径
        NoClientAuth::new()
    };

    let certs = match load_certs(server_certs_path) {
        Err(e) => {
            //加载服务器证错误，则立即返回错误原因
            return Err(e);
        },
        Ok(certs) => {
            //加载服务器证书成功，则继续
            certs
        }
    };

    let pk = match load_private_key(server_key_path) {
        Err(e) => {
            //加载服务器私钥错误，则立即返回错误原因
            return Err(e);
        },
        Ok(pk) => {
            //加载服务器私钥成功，则继续
            pk
        },
    };

    let ocsp = load_ocsp(&server_ocsp_path)?;

    let mut config = match rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_client_cert_verifier(client_auth)
        .with_single_cert_with_ocsp_and_sct(certs, pk, ocsp, vec![]) //设置指定私钥和OCSP响应的单证书链
    {
        Err(e) => {
            //设置单证书链错误，则立即返回错误原因
            return Err(Error::new(ErrorKind::Other,
                                  format!("bad certificates or private key, reason: {:?}", e)));
        },
        Ok(cfg) => cfg,
    };
    config.key_log = Arc::new(rustls::KeyLogFile::new());

    if !server_suite.is_empty() {
        match select_suites(&server_suite) {
            Err(e) => {
                //选择指定名称的密码套件错误，则立即返回错误原因
                return Err(Error::new(ErrorKind::Other,
                                      format!("Select suites failed, reason: {:?}", e)));
            },
            Ok(suites) => {
                //选择指定名称的密码套件成功
                // config.ciphersuites = suites;
            },
        }
    }

    if !server_versions.is_empty() {
        match select_versions(&server_versions) {
            Err(e) => {
                //选择指定名称的Tls版本错误，则立即返回错误原因
                return Err(Error::new(ErrorKind::Other,
                                      format!("Select version failed, reason: {:?}", e)));
            },
            Ok(versions) => {
                //选择指定名称的Tls版本成功
                // config.versions = versions;
            },
        }
    }

    if server_session_size != 0 {
        //设置指定最大容量的服务器Tls会话缓存
        // config.set_persistence(ServerSessionMemoryCache::new(server_session_size));
    }

    if is_server_tickets {
        //设置服务器Tls会话的票证分配器，用于向客户端分配Tls会话票证，客户端用服务器分配的Tls会话票证，可以安全快速的握手
        // config.ticketer = rustls::Ticketer::new();
    }

    //设置ALPN协议列表，根据列表顺序确定协议优先级
    // config.set_protocols(&server_alpns
    //     .iter()
    //     .map(|protocol| protocol.as_bytes().to_vec())
    //     .collect::<Vec<_>>()[..]);

    Ok(Arc::new(config))
}

// 加载指定Pem文件的证书
fn load_certs<P: AsRef<Path>>(file_path: P) -> IOResult<Vec<Certificate>> {
    let certfile = match File::open(&file_path) {
        Err(e) => {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Open certs failed, path: {:?}, reason: {:?}",
                                          file_path.as_ref(), e)));
        },
        Ok(file) => {
            file
        },
    };

    let mut reader = BufReader::new(certfile);
    match rustls_pemfile::certs(&mut reader) {
        Err(e) => {
            Err(Error::new(ErrorKind::Other,
                           format!("Load certs failed, path: {:?}, reason: {:?}",
                                   file_path.as_ref(), e)))
        },
        Ok(certs) => {
            Ok(certs.iter()
                .map(|v| {
                    Certificate(v.clone())
                })
                .collect())
        },
    }
}

// 加载指定Pem文件的私钥
fn load_private_key<P: AsRef<Path>>(file_path: P) -> IOResult<PrivateKey> {
    let keyfile = match File::open(&file_path) {
        Err(e) => {
            return Err(Error::new(ErrorKind::Other,
                                  format!("Open private key failed, path: {:?}, reason: {:?}",
                                          file_path.as_ref(), e)));
        },
        Ok(file) => {
            file
        },
    };

    let mut reader = BufReader::new(keyfile);
    loop {
        match rustls_pemfile::read_one(&mut reader) {
            Ok(Some(rustls_pemfile::Item::RSAKey(key))) => return Ok(PrivateKey(key)),
            Ok(Some(rustls_pemfile::Item::PKCS8Key(key))) => return Ok(PrivateKey(key)),
            Ok(Some(rustls_pemfile::Item::ECKey(key))) => return Ok(PrivateKey(key)),
            Ok(None) => break,
            Err(e) => {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Load private key failed, path: {:?}, reason: {:?}",
                                              file_path.as_ref(), e)));
            },
            _ => {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Load private key failed, path: {:?}, reason: cannot parse private key .pem file",
                                              file_path.as_ref())))
            },
        }
    }

    Err(Error::new(ErrorKind::Other,
                   format!("no keys found in {:?} (encrypted keys not supported)",
                           file_path.as_ref())))
}

// 加载在线证书状态协议的响应文件
fn load_ocsp<P: AsRef<Path>>(file_path: &Option<P>) -> IOResult<Vec<u8>> {
    let mut ret = Vec::new();

    if let Some(path) = &file_path {
        match File::open(path) {
            Err(e) => {
                return Err(Error::new(ErrorKind::Other,
                                      format!("Open ocsp failed, path: {:?}, reason: {:?}",
                                              path.as_ref(), e)));
            },
            Ok(mut file) => {
                file.read_to_end(&mut ret).unwrap();
            },
        }
    }

    Ok(ret)
}

//选择密码套件
fn select_suites(suites: &[String]) -> Result<Vec<&'static rustls::SupportedCipherSuite>, String> {
    let mut result = Vec::new();
    for cs_name in suites {
        if let Some(suite) = find_suite(cs_name) {
            result.push(suite);
        } else {
            return Err(format!("cannot select ciphersuite '{}'", cs_name));
        }
    }

    Ok(result)
}

//查找指定名称的密码套件
fn find_suite(cs_name: &str) -> Option<&'static rustls::SupportedCipherSuite> {
    for suite in rustls::ALL_CIPHER_SUITES {
        let sname = format!("{:?}", suite.suite()).to_lowercase();

        if sname == cs_name.to_string().to_lowercase() {
            return Some(suite);
        }
    }

    None
}

//选择Tls协议版本
fn select_versions(versions: &[String]) -> Result<Vec<rustls::ProtocolVersion>, String> {
    let mut result = Vec::new();
    for vname in versions {
        let version = match vname.as_ref() {
            "1.2" => ProtocolVersion::TLSv1_2,
            "1.3" => ProtocolVersion::TLSv1_3,
            _ => {
                return Err(format!("cannot select tls version '{}', valid are '1.2' and '1.3'", vname));
            },
        };
        result.push(version);
    }

    Ok(result)
}

///
/// 上下文句柄
///
pub struct ContextHandle<T: 'static>(Option<Arc<T>>);

unsafe impl<T: 'static> Send for ContextHandle<T> {}

impl<T: 'static> Drop for ContextHandle<T> {
    fn drop(&mut self) {
        if let Some(shared) = self.0.take() {
            //当前上下文指针存在，则释放
            Arc::into_raw(shared);
        }
    }
}

impl<T: 'static> ContextHandle<T> {
    /// 获取上下文只读引用
    pub fn as_ref(&self) -> &T {
        self.0.as_ref().unwrap().as_ref()
    }

    /// 获取上下文可写引用
    pub fn as_mut(&mut self) -> Option<&mut T> {
        if let Some(shared) = self.0.as_mut() {
            return Arc::get_mut(shared);
        }

        None
    }
}

///
/// 通用上下文
/// 注意，设置上下文后，需要移除当前上下文，上下文才会自动释放
///
pub struct SocketContext {
    inner: *const (), //内部上下文
}

unsafe impl Send for SocketContext {}

impl SocketContext {
    /// 创建空的上下文
    pub fn empty() -> Self {
        SocketContext {
            inner: ptr::null(),
        }
    }

    /// 判断上下文是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.is_null()
    }

    /// 获取上下文的句柄
    pub fn get<T>(&self) -> Option<ContextHandle<T>> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { ContextHandle(Some(Arc::from_raw(self.inner as *const T))) })
    }

    /// 设置上下文，如果当前上下文不为空，则设置失败
    pub fn set<T>(&mut self, context: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner = Arc::into_raw(Arc::new(context)) as *const T as *const ();
        true
    }

    /// 移除上下文，如果当前还有未释放的上下文句柄，则返回移除错误，如果当前有上下文，则返回被移除的上下文，否则返回空
    pub fn remove<T>(&mut self) -> Result<Option<T>, &str> {
        if self.is_empty() {
            return Ok(None);
        }

        let inner = unsafe { Arc::from_raw(self.inner as *const T) };
        if Arc::strong_count(&inner) > 1 {
            Arc::into_raw(inner); //释放临时共享指针
            Err("Remove context failed, reason: context shared exist")
        } else {
            match Arc::try_unwrap(inner) {
                Err(inner) => {
                    Arc::into_raw(inner); //释放临时共享指针
                    Err("Remove context failed, reason: invalid shared")
                },
                Ok(context) => {
                    //将当前内部上下文设置为空，并返回上下文
                    self.inner = ptr::null();
                    Ok(Some(context))
                },
            }
        }
    }
}

///
/// 共享流
///
pub struct SharedStream<S: Stream>(Arc<RefCell<S>>);

unsafe impl<S: Stream> Send for SharedStream<S> {}

impl<S: Stream> Clone for SharedStream<S> {
    fn clone(&self) -> Self {
        SharedStream(self.0.clone())
    }
}

impl<S: Stream> SharedStream<S> {
    /// 构建一个共享流
    pub fn new(s: S) -> Self {
        SharedStream(Arc::new(RefCell::new(s)))
    }

    /// 使用内部共享流构建一个共享流
    pub fn with_inner(inner: &Arc<RefCell<S>>) -> Self {
        SharedStream(inner.clone())
    }

    /// 获取内部共享流的只读引用
    pub fn inner_ref(&self) -> &Arc<RefCell<S>> {
        &self.0
    }

    /// 获取共享流的只读引用
    #[inline]
    pub fn borrow<'a>(&'a self) -> SharedRef<'a, S> {
        SharedRef(self.0.borrow())
    }

    /// 获取共享流的可写引用
    #[inline]
    pub fn borrow_mut<'a>(&'a self) -> SharedRefMut<'a, S> {
        SharedRefMut(self.0.borrow_mut())
    }

    /// 获取共享流的指针
    #[inline]
    pub fn as_ptr(&self) -> *mut S {
        self.0.as_ptr()
    }
}

///
/// 共享流的只读引用
///
pub struct SharedRef<'a, S: Stream>(Ref<'a, S>);

unsafe impl<'a, S: Stream> Send for SharedRef<'a, S> {}

impl<'a, S: Stream> Deref for SharedRef<'a, S> {
    type Target = S;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

///
/// 共享流的可写引用
///
pub struct SharedRefMut<'a, S: Stream>(RefMut<'a, S>);

unsafe impl<'a, S: Stream> Send for SharedRefMut<'a, S> {}

impl<'a, S: Stream> Deref for SharedRefMut<'a, S> {
    type Target = S;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<'a, S: Stream> DerefMut for SharedRefMut<'a, S> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}

/// 线程安全的异步休眠对象
pub struct Hibernate<S: Socket>(Arc<InnerHibernate<S>>);

impl<S: Socket> Clone for Hibernate<S> {
    fn clone(&self) -> Self {
        Hibernate(self.0.clone())
    }
}

impl<S: Socket> Future for Hibernate<S> {
    type Output = IOResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut locked = self.0.waker.lock(); //由锁保护异步休眠对象的唤醒

        //获取唤醒的结果值
        let result = self
            .0
            .result
            .0
            .borrow_mut()
            .take();

        if let Some(result) = result {
            //已设置唤醒的结果值，则立即返回就绪
            if let Err(e) = self
                .0
                .handle
                .reregister_interest(self.0.ready.clone()) {
                //在唤醒后重置当前流感兴趣的事件失败，则立即返回错误原因
                Poll::Ready(Err(e))
            } else {
                //在唤醒后重置当前流感兴趣的事件成功
                self.0.handle.run_hibernated_tasks(); //立即异步运行连接休眠时的所有任务
                Poll::Ready(result)
            }
        } else {
            //需要休眠，则立即返回挂起
            if let Err(e) = self
                .0
                .handle
                .reregister_interest(self.0.ready.clone()) {
                //在休眠时重置当前流感兴趣的事件为只写失败，则立即返回错误原因
                Poll::Ready(Err(e))
            } else {
                //在休眠时重置当前流感兴趣的事件为只写成功
                if self.0.handle.set_hibernate(self.clone()) {
                    //设置当前连接的休眠对象成功，则设置当前休眠的唤醒器
                    *locked = Some(cx.waker().clone());
                } else {
                    //当前连接已设置休眠对象，则将当前休眠对象写入当前连接的休眠对象的唤醒器
                    self
                        .0
                        .handle
                        .set_hibernate_wakers(cx.waker().clone());
                    self.0.waker_status.store(1, Ordering::Relaxed); //设置唤醒器状态为被阻塞
                }

                Poll::Pending
            }
        }
    }
}

impl<S: Socket> Hibernate<S> {
    /// 构建一个异步休眠对象
    pub fn new(handle: SocketHandle<S>,
               ready: Ready) -> Self {
        let inner = InnerHibernate {
            handle,
            ready,
            waker: SpinLock::new(None),
            waker_status: AtomicU8::new(0),
            result: AsyncWaitResult(Arc::new(RefCell::new(None))),
        };

        Hibernate(Arc::new(inner))
    }

    /// 线程安全的设置结果值，并唤醒正在休眠的异步休眠对象，如果当前异步休眠对象未休眠则忽略，唤醒成功返回真
    /// 唤醒过程可能会被阻塞，这不会导致线程阻塞而是返回假，调用者可以继续尝试唤醒，直到返回真
    pub(crate) fn wakeup(&self, result: IOResult<()>) -> bool {
        let mut locked = self
            .0
            .waker
            .lock(); //由锁保护异步休眠对象的唤醒

        if let Some(waker) = locked.take() {
            //当前异步休眠对象正在休眠中，则设置结果值，并立即唤醒当前异步休眠对象
            if self.0.waker_status.load(Ordering::Relaxed) > 0 {
                //唤醒过程被阻塞，则立即返回假，需要调用者继续尝试唤醒
                self.0.waker_status.store(0, Ordering::Relaxed); //设置唤醒器状态为可以完成唤醒
                waker.wake();

                false
            } else {
                //唤醒过程完成，则立即返回真
                *self.0.result.0.borrow_mut() = Some(result);
                waker.wake();

                true
            }
        } else {
            if self.0.result.0.borrow().is_none() {
                //当前异步休眠对象还未休眠，则立即返回假，需要调用者继续尝试唤醒
                false
            } else {
                //当前异步休眠对象已被唤醒，则立即返回真
                true
            }
        }
    }
}

// 内部线程安全的异步休眠对象
struct InnerHibernate<S: Socket> {
    handle:         SocketHandle<S>,            //当前连接的句柄
    ready:          Ready,                      //唤醒后感兴趣的事件
    waker:          SpinLock<Option<Waker>>,    //休眠唤醒器
    waker_status:   AtomicU8,                   //唤醒状态
    result:         AsyncWaitResult<()>,        //结果值
}

///
/// 就绪状态
///
#[derive(Debug, Clone)]
pub enum Ready {
    Empty,      //空
    Readable,   //可读
    Writable,   //可写
    OnlyRead,   //只读
    OnlyWrite,  //只写
    ReadWrite,  //可读可写
}


