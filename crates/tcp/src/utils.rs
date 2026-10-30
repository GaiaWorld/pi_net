use std::any::Any;
use std::rc::Rc;
use std::fs::File;
use std::pin::Pin;
use std::path::Path;
use std::result::Result;
use std::future::Future;
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
                      AllowAnyAnonymousOrAuthenticatedClient}};
use rustls_pemfile;

use pi_async_rt::{lock::spin_lock::SpinLock,
               rt::{serial::AsyncWaitResult,
                    serial_local_thread::LocalTaskRuntime}};
use pi_hash::XHashMap;

use crate::{Socket, SocketHandle, Stream};

#[cfg(test)]
#[path = "context_storage_regression.rs"]
mod context_storage_regression;

#[cfg(all(test, target_os = "linux"))]
#[path = "hibernate_regression.rs"]
mod hibernate_regression;

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
                      _server_alpns: Vec<String>) -> IOResult<Arc<ServerConfig>> {
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
                                              format!("Save client auth root failed, reason: {:?}",
                                                      e)));
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
                                  format!("bad certificates or private key, reason: {:?}",
                                          e)));
        },
        Ok(cfg) => cfg,
    };
    config.key_log = Arc::new(rustls::KeyLogFile::new());

    if !server_suite.is_empty() {
        match select_suites(&server_suite) {
            Err(e) => {
                //选择指定名称的密码套件错误，则立即返回错误原因
                return Err(Error::new(ErrorKind::Other,
                                      format!("Select suites failed, reason: {:?}",
                                              e)));
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
                                      format!("Select version failed, reason: {:?}",
                                              e)));
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
                                          file_path.as_ref(),
                                          e)));
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
                                              file_path.as_ref(),
                                              e)));
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
                                              path.as_ref(),
                                              e)));
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
            return Err(format!("Cannot select ciphersuite '{}'", cs_name));
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
                return Err(format!("Cannot select tls version '{}', valid are '1.2' and '1.3'", vname));
            },
        };
        result.push(version);
    }

    Ok(result)
}

/// 业务对象的短访问许可（access permit）错误；不分配、不等待，也不改变原值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAccessError {
    /// 同一对象已有读或写许可（access permit），本次未取得访问权。
    Busy,
    /// 原对象已由try_remove移出；旧句柄（context handle）不会转向后来安装的对象。
    Closed,
}

/// try_remove的失败结果；两种错误均保持原槽（slot）及业务值不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRemoveError {
    /// 当前已安装对象的实际类型（type）与请求的T不符；优先于数据占用检查。
    TypeMismatch,
    /// 同一对象的数据许可（access permit）尚未归还，本次不移出、不等待。
    Busy,
}

// 实现依据：docs/tcp_context_20261029/CORE_SHELL.md#ctx-core-branches。
// 槽（slot）只保留当前Entry的真实Rc，类型擦除（type erasure）不擦除引用计数。
// Entry的Some(T)/None分别表示对象仍在/已移出；新set总是创建另一个Entry。
// get：短借用槽 → 核类型 → 克隆Rc → 结束槽借用 → 安全向下转换（downcast）。
// 移出：核类型/持有数 → 取得数据许可 → 取T → 归还数据许可 → 摘除槽Rc
//       → 归还槽借用 → 归还摘出的Rc → 返回T。
// 分配（allocation）、失败输入和最后持有的用户析构（Drop）不放在内部借用中。
struct ContextEntry<T: 'static> {
    value: RefCell<Option<T>>,
}

/// 对一次安装对象的真实持有（ownership），由SocketContext::get取得。
///
/// 仅在所属线程（owner thread）使用，可跨本线程的异步等待（await）保活；本对象
/// 不占用数据许可（access permit），不直接导出T引用（reference）。同一Entry
/// 可有多个普通句柄（context handle），但同时只允许一份读或写许可（access permit）。
/// 容器（container）归还不撤销本持有；try_remove成功后本对象只能取得Closed。
///
/// 归还本对象释放其真实Rc；若为最后持有且值未移出，会同步执行用户析构（Drop）。
/// 无自动网络关闭、同步锁（synchronous lock）、任务派发（task dispatch）或等待；
/// 析构（Drop）成本另计，遗忘对象或用户引用环（reference cycle）可保留资源。
/// 关联测试（test）：CTX-U03/U06/U10～U12，编译约束（compile-time constraint）CTX-C02。
///
/// `ContextHandle`不能发送到其它线程（thread）：
/// ```compile_fail,E0277
/// use pi_tcp::utils::ContextHandle;
/// fn require_send<T: Send>() {}
/// require_send::<ContextHandle<u8>>();
/// ```
///
/// `ContextHandle`不能在线程（thread）间共享：
/// ```compile_fail,E0277
/// use pi_tcp::utils::ContextHandle;
/// fn require_sync<T: Sync>() {}
/// require_sync::<ContextHandle<u8>>();
/// ```
///
/// 业务引用（reference）必须来自许可（access permit），不提供原直接读取出口：
/// ```compile_fail,E0599
/// use pi_tcp::utils::SocketContext;
/// let context = SocketContext::empty();
/// context.set(1_u8);
/// let handle = context.get::<u8>().unwrap();
/// let _ = handle.as_ref();
/// ```
///
/// 同样不提供原直接可变访问出口：
/// ```compile_fail,E0599
/// use pi_tcp::utils::SocketContext;
/// let context = SocketContext::empty();
/// context.set(1_u8);
/// let mut handle = context.get::<u8>().unwrap();
/// let _ = handle.as_mut();
/// ```
pub struct ContextHandle<T: 'static> {
    entry: Rc<ContextEntry<T>>,
}

impl<T: 'static> ContextHandle<T> {
    /// 尝试取得一份只读许可（read permit）；读读同样互斥（mutually exclusive）。
    ///
    /// 无额外入参；只借用本句柄（context handle），成功许可（access permit）不得比它
    /// 活得更久。已有任意许可（access permit）时立即Busy；原对象已移出则Closed，
    /// 两者不改变值、不自动重试。取得/归还O(1)时间和空间，无分配、用户代码或等待。
    /// 许可（access permit）及其引用（reference）必须在异步等待（await）、网络操作、
    /// 未知阻塞（blocking）或可重入回调（reentrant callback）前归还；展开会归还资格。
    /// 关联测试（test）：CTX-U04/U05/U09/U10/U14及CTX-C04/C05。
    ///
    /// 许可（access permit）不能超出普通句柄（context handle）的借用期（borrow lifetime）：
    /// ```compile_fail,E0597
    /// use pi_tcp::utils::SocketContext;
    /// let context = SocketContext::empty();
    /// context.set(1_u8);
    /// let permit = {
    ///     let handle = context.get::<u8>().unwrap();
    ///     handle.try_read().unwrap()
    /// };
    /// assert_eq!(*permit.as_ref(), 1);
    /// ```
    pub fn try_read(&self) -> Result<ContextRead<'_, T>, ContextAccessError> {
        let value = self.entry.value.try_borrow_mut().map_err(|_| ContextAccessError::Busy)?;
        match RefMut::filter_map(value, |value| value.as_mut()) {
            Ok(value) => Ok(ContextRead { value }),
            Err(value) => {
                drop(value);
                Err(ContextAccessError::Closed)
            },
        }
    }

    /// 尝试取得一份写许可（write permit），不要求其它普通句柄（context handle）归还。
    ///
    /// 无额外入参；成功许可（access permit）借用本句柄（context handle），已有任意
    /// 许可（access permit）立即Busy，值已移出则Closed，不改值、不等待或重试。
    /// 取得/归还O(1)时间和空间，无分配或用户代码；普通归还不回滚已完成修改。
    /// 线程（thread）、异步等待（await）、重入（reentrancy）禁忌与try_read相同。
    /// 关联测试（test）：CTX-U03～U05/U09/U10/U14及CTX-C04/C05。
    ///
    /// 写许可（write permit）也不能超出普通句柄（context handle）：
    /// ```compile_fail,E0597
    /// use pi_tcp::utils::SocketContext;
    /// let context = SocketContext::empty();
    /// context.set(1_u8);
    /// let permit = {
    ///     let handle = context.get::<u8>().unwrap();
    ///     handle.try_write().unwrap()
    /// };
    /// assert_eq!(*permit.as_ref(), 1);
    /// ```
    pub fn try_write(&self) -> Result<ContextWrite<'_, T>, ContextAccessError> {
        let value = self.entry.value.try_borrow_mut().map_err(|_| ContextAccessError::Busy)?;
        match RefMut::filter_map(value, |value| value.as_mut()) {
            Ok(value) => Ok(ContextWrite { value }),
            Err(value) => {
                drop(value);
                Err(ContextAccessError::Closed)
            },
        }
    }
}

/// 所属线程（owner thread）的一份短只读许可（read permit），仅由try_read取得。
///
/// 归还时仅释放数据借用（data borrow），无分配、等待、回滚或用户析构（Drop），O(1)。
/// 不可遗忘后继续依赖及时解除占用；不得跨异步等待（await）或未知回调（callback）。
/// 关联测试（test）：CTX-U04/U09/U14，编译约束（compile-time constraint）CTX-C03/C05。
///
/// 不能发送到其它线程（thread）：
/// ```compile_fail,E0277
/// use pi_tcp::utils::ContextRead;
/// fn require_send<T: Send>() {}
/// require_send::<ContextRead<'static, u8>>();
/// ```
///
/// 不能在线程（thread）间共享：
/// ```compile_fail,E0277
/// use pi_tcp::utils::ContextRead;
/// fn require_sync<T: Sync>() {}
/// require_sync::<ContextRead<'static, u8>>();
/// ```
pub struct ContextRead<'a, T: 'static> {
    value: RefMut<'a, T>,
}

impl<'a, T: 'static> ContextRead<'a, T> {
    /// 返回仅在本次许可（access permit）借用期间有效的共享引用（shared reference）。
    ///
    /// 不转移所有权（ownership）、不授予第二份许可（access permit），无副作用；
    /// O(1)时间和空间，无分配、等待或用户代码。继承本许可（access permit）的线程
    /// （thread）及异步（async）限制。关联测试（test）：CTX-U03/U05，约束CTX-C05。
    ///
    /// 引用（reference）不能超出许可（access permit）：
    /// ```compile_fail,E0597
    /// use pi_tcp::utils::SocketContext;
    /// let context = SocketContext::empty();
    /// context.set(1_u8);
    /// let handle = context.get::<u8>().unwrap();
    /// let value;
    /// {
    ///     let permit = handle.try_read().unwrap();
    ///     value = permit.as_ref();
    /// }
    /// assert_eq!(*value, 1);
    /// ```
    pub fn as_ref(&self) -> &T {
        &self.value
    }
}

/// 所属线程（owner thread）的一份短写许可（write permit），仅由try_write取得。
///
/// 可在自身借用期（borrow lifetime）内读取/修改T，不能分拆出冲突引用（reference）。
/// 归还O(1)，只释放数据借用（data borrow），不回滚修改、不等待或执行用户析构（Drop）；
/// 不得跨异步等待（await）或未知回调（callback），不能依赖遗忘许可（access permit）后恢复。
/// 关联测试（test）：CTX-U03/U04/U09/U14，约束CTX-C03/C05。
///
/// 不能发送到其它线程（thread）：
/// ```compile_fail,E0277
/// use pi_tcp::utils::ContextWrite;
/// fn require_send<T: Send>() {}
/// require_send::<ContextWrite<'static, u8>>();
/// ```
///
/// 不能在线程（thread）间共享：
/// ```compile_fail,E0277
/// use pi_tcp::utils::ContextWrite;
/// fn require_sync<T: Sync>() {}
/// require_sync::<ContextWrite<'static, u8>>();
/// ```
pub struct ContextWrite<'a, T: 'static> {
    value: RefMut<'a, T>,
}

impl<'a, T: 'static> ContextWrite<'a, T> {
    /// 借用本许可（access permit）取得共享引用（shared reference），不释放写资格。
    ///
    /// 无额外入参或状态（state）变化，不克隆T；返回值不能超出本次借用，也不能与
    /// 冲突可变借用（mutable borrow）并存。O(1)时间/空间，无等待、分配或用户代码；
    /// 继承本许可（access permit）限制。关联测试（test）：CTX-U03/U04及CTX-C05。
    pub fn as_ref(&self) -> &T {
        &self.value
    }

    /// 借用本许可（access permit）取得唯一可变引用（mutable reference）。
    ///
    /// 返回值同时受本次可变借用（mutable borrow）及许可（access permit）期限限制；
    /// 不转移T或新增访问权，O(1)时间/空间，无分配、等待或用户代码。不允许跨异步等待
    /// （await）或未知回调（callback）持有。关联测试（test）：CTX-U03/U04/U14及CTX-C05。
    ///
    /// 引用（reference）不能超出许可（access permit）：
    /// ```compile_fail,E0597
    /// use pi_tcp::utils::SocketContext;
    /// let context = SocketContext::empty();
    /// context.set(1_u8);
    /// let handle = context.get::<u8>().unwrap();
    /// let value;
    /// {
    ///     let mut permit = handle.try_write().unwrap();
    ///     value = permit.as_mut();
    /// }
    /// *value = 2;
    /// ```
    ///
    /// 同一写许可（write permit）不能导出两个同时使用的可变引用（mutable reference）：
    /// ```compile_fail,E0499
    /// use pi_tcp::utils::SocketContext;
    /// let context = SocketContext::empty();
    /// context.set(1_u8);
    /// let handle = context.get::<u8>().unwrap();
    /// let mut permit = handle.try_write().unwrap();
    /// let first = permit.as_mut();
    /// let second = permit.as_mut();
    /// *first += *second;
    /// ```
    pub fn as_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

/// 可复用的线程内上下文（Context）容器（container），一次只安装一个实际类型（type）。
///
/// T须为`'static`，不要求`Send/Sync/Clone`；本对象及其普通句柄（context handle）、
/// 许可（access permit）均不能发送到其它线程（thread）或在线程（thread）间共享。
/// 普通句柄（context handle）真实保活安装对象，容器（container）归还不撤销它们。
/// 最后真实持有结束才同步析构（Drop）尚未移出的T；用户引用环（reference cycle）、
/// 遗忘持有及运行时（runtime）停止不在及时回收保证中。
///
/// 这些同步操作（synchronous operation）不派发任务（task）、不操作网络（network）、
/// 不等待同步锁（synchronous lock）或数据占用；普通数据冲突返回Busy。私有槽（slot）
/// 的非法借用重入（borrow reentrancy）诊断为panic，不伪装成空值或业务Busy。
/// 分配（allocation）及用户析构（Drop）时间另计，不承诺固定墙钟截止（deadline）；
/// 失败输入及最后对象的析构（Drop）均在内部借用（borrow）外执行，可重入本地公开出口。
/// 不提供网络连接（connection）的准备/解除或整体停止保证，绑定（binding）另行管理。
/// 关联测试（test）：`context_storage_regression`中的CTX-U01～U17及下列CTX-C01/C02示例。
///
/// 非Send、非Clone的本地值可使用短许可（access permit），普通句柄（context handle）
/// 不妨碍单份写资格，移出后旧句柄（context handle）只看到Closed：
/// ```
/// use std::{cell::Cell, rc::Rc};
/// use pi_tcp::utils::{SocketContext, ContextAccessError};
/// struct LocalValue(Cell<u8>, Rc<()>);
/// let context = SocketContext::empty();
/// assert!(context.set(LocalValue(Cell::new(1), Rc::new(()))));
/// let first = context.get::<LocalValue>().unwrap();
/// let second = context.get::<LocalValue>().unwrap();
/// {
///     let mut write = first.try_write().unwrap();
///     write.as_mut().0.set(2);
///     assert!(matches!(second.try_read(), Err(ContextAccessError::Busy)));
/// }
/// {
///     let read = second.try_read().unwrap();
///     assert_eq!(read.as_ref().0.get(), 2);
/// }
/// assert!(context.remove::<LocalValue>().is_err());
/// let value = context.try_remove::<LocalValue>().unwrap().unwrap();
/// assert_eq!(value.0.get(), 2);
/// assert!(matches!(first.try_read(), Err(ContextAccessError::Closed)));
/// assert!(context.is_empty());
/// ```
///
/// 容器（container）不能发送到其它线程（thread）：
/// ```compile_fail,E0277
/// use pi_tcp::utils::SocketContext;
/// fn require_send<T: Send>() {}
/// require_send::<SocketContext>();
/// ```
///
/// 容器（container）不能在线程（thread）间共享：
/// ```compile_fail,E0277
/// use pi_tcp::utils::SocketContext;
/// fn require_sync<T: Sync>() {}
/// require_sync::<SocketContext>();
/// ```
pub struct SocketContext {
    slot: RefCell<Option<Rc<dyn Any>>>,
}

impl SocketContext {
    /// 创建可立即设置值的空槽（slot），不分配Entry、不持有T。
    ///
    /// 无入参，无外部状态（state）或用户副作用；每次返回独立容器（container），
    /// O(1)时间和空间，无锁（lock）/等待/任务（task）。仅限所属线程（owner thread），
    /// 分配器（allocator）及网络（network）均不参与。关联测试（test）：CTX-U01。
    pub fn empty() -> Self {
        SocketContext { slot: RefCell::new(None) }
    }

    /// 观察当前槽（slot）是否为空；true不阻止后续set，也不反映旧句柄（context handle）。
    ///
    /// 无额外入参或状态（state）变化，仅同线程（thread）即时观察，O(1)时间和空间，
    /// 无分配、等待或用户代码。非法私有槽借用（slot borrow）冲突为panic；
    /// 数据许可（access permit）占用不影响本操作。关联测试（test）：CTX-U01/U10/U13。
    pub fn is_empty(&self) -> bool {
        self.slot.try_borrow().expect("Context slot borrow conflict").is_none()
    }

    /// 空槽（slot）安装owned T并返回true；非空返回false且不覆盖原对象。
    ///
    /// value总是移入；失败输入会在内部借用（borrow）外同步析构（Drop）。T须为
    /// `'static`，无需`Send/Sync/Clone`。成功安装创建一份Rc Entry，T内联保存；
    /// 管理O(1)，值移动及Entry空间O(sizeof(T))，分配（allocation）/用户析构（Drop）
    /// 成本另计，无等待/重试。非法私有槽重入（slot reentrancy）为panic，分配
    /// （allocation）失败沿分配器（allocator）策略，不保证用户析构（Drop）不展开。
    /// 仅限所属线程（owner thread），不幂等（idempotent）地复制安装，不做异步工作
    /// （async work）。关联测试（test）：CTX-U02/U13/U16/U17。
    pub fn set<T: 'static>(&self, value: T) -> bool {
        let occupied = {
            let slot = self.slot.try_borrow().expect("Context slot borrow conflict");
            slot.is_some()
        };
        if occupied {
            return false;
        }

        let entry: Rc<dyn Any> = Rc::new(ContextEntry { value: RefCell::new(Some(value)) });
        let mut candidate = Some(entry);
        let installed = {
            let mut slot = self.slot.try_borrow_mut().expect("Context slot borrow conflict");
            // 分配（allocation）可能允许外部重入（reentrancy），提交前再核空槽（slot）。
            // 未安装的候选（candidate）保留至借用（borrow）结束，不在此处析构（Drop）T。
            if slot.is_some() {
                false
            } else {
                *slot = candidate.take();
                true
            }
        };
        drop(candidate);
        installed
    }

    /// 按实际T取得普通句柄（context handle）；空槽（slot）或类型（type）不符返回None。
    ///
    /// T须为`'static`，成功只增加原Entry的一份真实Rc，不复制T、不取得数据许可
    /// （access permit）；已有数据借用（data borrow）不妨碍本操作。O(1)时间/空间，
    /// 无分配、等待、用户代码或状态（state）转换，重复取得产生各自真实持有。
    /// 仅限所属线程（owner thread）；非法私有槽重入（slot reentrancy）为panic。
    /// 关联测试（test）：CTX-U01/U03/U05/U08/U13。
    pub fn get<T: 'static>(&self) -> Option<ContextHandle<T>> {
        let erased = {
            let slot = self.slot.try_borrow().expect("Context slot borrow conflict");
            let entry = slot.as_ref()?;
            if !entry.is::<ContextEntry<T>>() {
                return None;
            }
            entry.clone()
        };
        let entry = erased.downcast::<ContextEntry<T>>()
            .unwrap_or_else(|_| panic!("Context entry type invariant violated"));
        Some(ContextHandle { entry })
    }

    /// 无普通句柄（context handle）尚存时移出实际T；空槽（slot）返回Ok(None)。
    ///
    /// 类型（type）不符优先报错；有普通句柄（context handle）沿原共享错误文字拒绝；
    /// 数据许可（access permit）占用也拒绝，失败不改原对象。外层容器（container）的
    /// Rc数量不参与检查。成功返回owned T并使槽（slot）可复用，不执行T的析构（Drop）。
    /// O(1)管理/额外空间，T移动成本另计；同线程（thread）、同步（synchronous）、
    /// 无等待/重试/用户代码。重复空移出为Ok(None)，非法私有槽重入（slot reentrancy）
    /// 或内部不变量（invariant）损坏为panic。关联测试（test）：CTX-U01/U06～U08/U12/U13/U15/U17。
    pub fn remove<T: 'static>(&self) -> Result<Option<T>, &str> {
        let mut slot = self.slot.try_borrow_mut().expect("Context slot borrow conflict");
        let Some(current) = slot.as_ref() else {
            return Ok(None);
        };
        let Some(entry) = current.downcast_ref::<ContextEntry<T>>() else {
            return Err("Remove context failed, reason: type mismatch");
        };
        // 不先克隆Rc；计数（reference count）只包含槽及真正的普通句柄（context handle）。
        if Rc::strong_count(current) > 1 {
            return Err("Remove context failed, reason: context shared exist");
        }
        let mut data = entry.value.try_borrow_mut()
            .map_err(|_| "Remove context failed, reason: data access is busy")?;
        let value = data.take().expect("Context entry value invariant violated");
        drop(data);
        let retired = slot.take();
        drop(slot);
        drop(retired);
        Ok(Some(value))
    }

    /// 不要求普通句柄（context handle）归还，立即尝试移出实际T。
    ///
    /// 空槽（slot）Ok(None)；先核类型（type），不符TypeMismatch，任意数据许可
    /// （access permit）尚存则Busy，失败均不改变原对象。成功返回owned T、清空槽
    /// （slot），旧Entry变为Closed；新set总是新Entry，旧句柄（context handle）不能
    /// 指向它。不能撤销仍有效的引用（reference），不等待、不自动重试或关闭网络。
    /// 同线程（thread）同步（synchronous），O(1)管理/额外空间，值移动成本另计；
    /// 无用户代码，非法私有槽重入（slot reentrancy）或内部不变量（invariant）损坏为panic。
    /// 关联测试（test）：CTX-U01/U08～U10/U12/U13/U15/U17。
    pub fn try_remove<T: 'static>(&self) -> Result<Option<T>, ContextRemoveError> {
        let mut slot = self.slot.try_borrow_mut().expect("Context slot borrow conflict");
        let Some(current) = slot.as_ref() else {
            return Ok(None);
        };
        let Some(entry) = current.downcast_ref::<ContextEntry<T>>() else {
            return Err(ContextRemoveError::TypeMismatch);
        };
        let mut data = entry.value.try_borrow_mut().map_err(|_| ContextRemoveError::Busy)?;
        let value = data.take().expect("Context entry value invariant violated");
        drop(data);
        let retired = slot.take();
        drop(slot);
        drop(retired);
        Ok(Some(value))
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

/// 连接的一次异步休眠，共享句柄的克隆指向同一次休眠状态，不是独立的广播订阅。
///
/// 由连接所属运行时轮询，使用连接句柄的 `wakeup` 发布成功或错误结果。Pending
/// 不表示线程停止；每次轮询仍有短同步锁和原事件重新登记开销，锁竞争可能等待。
/// 不应在完成后再次轮询，也不要持有该调用链需要的锁等待休眠完成。
/// 关联 `hibernate_regression` 的 TCP/TLS 真实专项和局部分支单元，及外部唤醒基准。
// TCP-HIB-REPAIR-001/D-HIB-07仅修订锁序/通知，不扩展原SocketHandle的整体引用边界。
pub struct Hibernate<S: Socket>(Arc<InnerHibernate<S>>);

impl<S: Socket> Clone for Hibernate<S> {
    fn clone(&self) -> Self {
        Hibernate(self.0.clone())
    }
}

impl<S: Socket> Future for Hibernate<S> {
    type Output = IOResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // clone/drop可调用唤醒器实现，不能放在W或H的临界区。待销毁值的声明
        // 在W守卫之前，即使登记出错或展开，也先释放守卫再销毁这些值。
        // 提前准备避免为clone拆分两次W临界区；Ready分支也有一次clone/drop，
        // 其实现可能分配或panic，相关额外成本由前后基准观察，不作零成本承诺。
        let mut incoming = Some(cx.waker().clone());
        let mut retired_waker = None;
        let mut stored_result;
        let (polled, run_tasks) = {
            let mut locked = self.0.waker.lock();

            // 仅测试的阶段探针；正常库和基准完全不编译此调用。
            #[cfg(all(test, target_os = "linux"))]
            hibernate_regression::after_poll_waker_lock(&*self);

            stored_result = self.0.result.0.borrow_mut().take();
            // 保持原先先取结果、再重新登记事件的顺序，不持锁await或执行通知。
            if let Err(e) = self.0.handle.reregister_interest(self.0.ready.clone()) {
                (Poll::Ready(Err(e)), false)
            } else if let Some(result) = stored_result.take() {
                (Poll::Ready(result), true)
            } else {
                // set_hibernate只短持H；获取为W→H，离开该调用先释放H。
                if self.0.handle.set_hibernate(self.clone()) {
                    retired_waker = locked.replace(incoming.take().unwrap());
                } else {
                    self.0.handle.set_hibernate_wakers(incoming.take().unwrap());
                    self.0.waker_status.store(1, Ordering::Relaxed);
                }
                (Poll::Pending, false)
            }
        };
        if run_tasks {
            // 原积压任务仍只在结果已取出且事件登记成功后派发；派发不占用W。
            self.0.handle.run_hibernated_tasks();
        }
        drop(retired_waker);
        polled
    }
}

impl<S: Socket> Hibernate<S> {
    /// 为 `handle` 指向的连接构建一次休眠，恢复轮询时按 `ready` 重新登记关注事件。
    ///
    /// handle移入并保活连接，ready为原就绪选项，构建时不会安装连接休眠槽；首次
    /// poll才尝试安装。返回对象和Clone共享一次结果，不是独立可重复使用的订阅。
    /// 不做I/O或等待，不幂等地创建新对象，O(1)时间/空间并分配固定大小共享状态。
    /// 应由所属运行时轮询；丢弃调用侧Future不等于自动唤醒或关闭连接。
    /// 构建本身没有同步锁/异步锁等待，分配失败遵循全局分配器策略。
    /// 关联hibernate_regression真实装配和unit身份测试。
    ///
    /// 下例仅构造，返回后应交给该连接所属运行时poll/await；构造不是已休眠确认，
    /// 外部唤醒仍须遵循句柄wakeup的原调度/重试约定。示例只编译，不启动网络。
    /// ```no_run
    /// fn prepare<S: pi_tcp::Socket>(handle: pi_tcp::SocketHandle<S>) -> pi_tcp::utils::Hibernate<S> {
    ///     pi_tcp::utils::Hibernate::new(handle, pi_tcp::utils::Ready::ReadWrite)
    /// }
    /// ```
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

    // 本连接槽候选的身份比较，O(1)时间/空间，无锁、无分配、无副作用。
    // 不比较UID；候选自身的Arc保活使并发槽换代不会复用候选地址。
    pub(crate) fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    // TCP-HIB-REPAIR-001/D-HIB-07：仅供TCP/TLS两个内部调用点协调一次唤醒。
    // result由调用者移入；detach只在W内调用一次，必须同步短持本连接H，按
    // 真实候选身份摘除并释放H。不得保存闭包、await、执行用户代码或反向取得W。
    // 条件失败不动waker/result；成功则保留原status和bool含义，不替调用者重试。
    // 状态工作量/额外空间O(1)，不含同步锁等待及锁外通知耗时；不新增分配。
    // 可能同步等待锁，回调panic继续传播；所有通知/废弃结果析构在释放W后。
    // 关联hibernate_regression真实专项和unit的条件失效/错误/通知/析构测试。
    pub(crate) fn wakeup<F>(&self, result: IOResult<()>, detach: F) -> bool
        where F: FnOnce(&Self) -> bool {
        // 仅测试的第二阶段；不读写生产锁状态，不出现在正常库或基准产物。
        #[cfg(all(test, target_os = "linux"))]
        hibernate_regression::before_wakeup_waker_lock(self);

        let mut retired_result = None;
        let (completed, waker) = {
            let mut locked = self.0.waker.lock();
            // 此操作内H的守卫先结束，再继续W内的结果发布；失效候选不碰新槽。
            if !detach(self) {
                return false;
            }
            if let Some(waker) = locked.take() {
                if self.0.waker_status.load(Ordering::Relaxed) > 0 {
                    self.0.waker_status.store(0, Ordering::Relaxed);
                    (false, Some(waker))
                } else {
                    retired_result = self.0.result.0.borrow_mut().replace(result);
                    (true, Some(waker))
                }
            } else {
                // RefCell守卫必须在W内随此语句结束，不能作为块末临时值跨出锁域。
                let completed = self.0.result.0.borrow().is_some();
                (completed, None)
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        drop(retired_result);
        completed
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


