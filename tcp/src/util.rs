use std::mem;
use std::ptr;
use std::thread;
use std::fs::File;
use std::sync::Arc;
use std::path::Path;
use std::time::Duration;
use std::io::{Result as IOResult, Read, BufReader};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::slice::{from_raw_parts, from_raw_parts_mut};

use iovec::MAX_LENGTH;
use mio::{Token, Ready};
use rustls::{ALL_CIPHERSUITES, ProtocolVersion, Session,
             RootCertStore, NoClientAuth, AllowAnyAuthenticatedClient, AllowAnyAnonymousOrAuthenticatedClient,
             ClientConfig, ClientSession, ServerConfig, ServerSession, ServerSessionMemoryCache};
use rustls::internal::pemfile;
use crossbeam_channel::Sender;
use parking_lot::RwLock;

use pi_hash::XHashMap;

/*
* Tcp连接池发送器表
*/
lazy_static! {
    pub static ref TCP_SOCKET_POOL_SENDER_TAB: Arc<RwLock<XHashMap<u8, Sender<(Token, IOResult<()>)>>>> = Arc::new(RwLock::new(XHashMap::default()));
}

/*
* 线程安全的注册Tcp连接池的关闭事件发送器
*/
pub fn register_close_sender(uid: u8, sender: Sender<(Token, IOResult<()>)>) {
    TCP_SOCKET_POOL_SENDER_TAB.write().insert(uid, sender);
}

/*
* 线程安全的关闭指定唯一id的Tcp连接
*/
pub fn close_socket(uid: usize, reason: IOResult<()>) -> bool {
    let pool_uid = (uid >> 24 & 0xff) as u8;
    let token = Token::from(uid & 0xffffff);
    if let Some(sender) = TCP_SOCKET_POOL_SENDER_TAB.read().get(&pool_uid) {
        sender.send((token, reason));
        return true;
    }

    false
}

#[cfg(all(feature="unstable", any(target_arch = "x86", target_arch = "x86_64")))]
#[inline(always)]
pub fn pause() {
    unsafe { asm!("PAUSE") };
}

#[cfg(all(not(feature="unstable"), any(target_arch = "x86", target_arch = "x86_64")))]
#[inline(always)]
pub fn pause() {
    thread::sleep(Duration::from_millis(1));
}

#[cfg(all(not(target_arch = "x86"), not(target_arch = "x86_64")))]
#[inline(always)]
pub fn pause() {
    thread::sleep(Duration::from_millis(1));
}

/*
* Tcp连接就绪状态
*/
#[derive(Clone)]
pub struct SocketReady(Arc<AtomicU8>);

impl SocketReady {
    //构建一个空的Tcp连接就绪状态
    pub fn empty() -> Self {
        SocketReady(Arc::new(AtomicU8::new(0)))
    }

    //获取当前就绪状态
    pub fn get(&self) -> Ready {
        match self.0.load(Ordering::SeqCst) {
            1 => {
                //可读
                Ready::readable()
            },
            2 => {
                //可写
                Ready::writable()
            },
            3 => {
                //可读写
                Ready::readable() | Ready::writable()
            },
            _ => {
                //空
                Ready::empty()
            },
        }
    }

    //插入当前就绪状态
    pub fn insert(&self, ready: Ready) {
        if ready.is_readable() && ready.is_writable() {
            self.0.fetch_or(3, Ordering::SeqCst);
        } else if ready.is_readable() {
            self.0.fetch_or(1, Ordering::SeqCst);
        } else if ready.is_writable() {
            self.0.fetch_or(2, Ordering::SeqCst);
        }
    }

    //移除当前就绪状态
    pub fn remove(&self, ready: Ready) {
        if ready.is_readable() && ready.is_writable() {
            self.0.store(0, Ordering::SeqCst);
        } else if ready.is_readable() {
            self.0.fetch_update(Ordering::SeqCst,
                                Ordering::SeqCst,
                                |v| {
                                    Some(v & !1)
            });
        } else if ready.is_writable() {
            self.0.fetch_update(Ordering::SeqCst,
                                Ordering::SeqCst,
                                |v| {
                                    Some(v & !2)
            });
        }
    }
}

/*
* 上下文句柄
*/
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
    //获取上下文只读引用
    pub fn as_ref(&self) -> &T {
        self.0.as_ref().unwrap().as_ref()
    }

    //获取上下文可写引用
    pub fn as_mut(&mut self) -> Option<&mut T> {
        if let Some(shared) = self.0.as_mut() {
            return Arc::get_mut(shared);
        }

        None
    }
}

/*
* 通用上下文
* 注意，设置上下文后，需要移除当前上下文，上下文才会自动释放
*/
pub struct SocketContext {
    inner: *const (), //内部上下文
}

unsafe impl Send for SocketContext {}

impl SocketContext {
    //创建空的上下文
    pub fn empty() -> Self {
        SocketContext {
            inner: ptr::null(),
        }
    }

    //判断上下文是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.is_null()
    }

    //获取上下文的句柄
    pub fn get<T>(&self) -> Option<ContextHandle<T>> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { ContextHandle(Some(Arc::from_raw(self.inner as *const T))) })
    }

    //设置上下文，如果当前上下文不为空，则设置失败
    pub fn set<T>(&mut self, context: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner = Arc::into_raw(Arc::new(context)) as *const T as *const ();
        true
    }

    //移除上下文，如果当前还有未释放的上下文句柄，则返回移除错误，如果当前有上下文，则返回被移除的上下文，否则返回空
    pub fn remove<T>(&mut self) -> Result<Option<T>, &str> {
        if self.is_empty() {
            return Ok(None);
        }

        let inner = unsafe { Arc::from_raw(self.inner as *const T) };
        if Arc::strong_count(&inner) > 1 {
            Arc::into_raw(inner); //释放临时共享指针
            Err("remove context failed, reason: context shared exist")
        } else {
            match Arc::try_unwrap(inner) {
                Err(inner) => {
                    Arc::into_raw(inner); //释放临时共享指针
                    Err("remove context failed, reason: invalid shared")
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

/*
* IO数据源
*/
enum IoBytesOrigin {
    Slice(*mut u8),
    Vec(*mut u8),
}

/*
* IO数据
*/
pub struct IoBytes(bool, usize, IoBytesOrigin);

unsafe impl Send for IoBytes {}

impl Drop for IoBytes {
    fn drop(&mut self) {
        if self.0 {
            //已释放，则忽略
            return;
        }
        self.0 = true;

        match self.2 {
            IoBytesOrigin::Slice(raw) => {
                unsafe { from_raw_parts(raw as *const u8, self.1); }
            },
            IoBytesOrigin::Vec(raw) => {
                unsafe { Vec::from_raw_parts(raw, self.1, self.1); }
            },
        }
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for IoBytes {
    fn from(slice: &'a [u8; N]) -> Self {
        let len = slice.len();
        if len > MAX_LENGTH {
            panic!("slice to io array failed, invalid slice length");
        }

        let raw = slice.as_ptr() as *mut u8;
        mem::forget(slice);

        IoBytes(false, len, IoBytesOrigin::Slice(raw))
    }
}

impl<'a> From<IoBytes> for &'a [u8] {
    fn from(mut arr: IoBytes) -> Self {
        arr.0 = true; //声明已释放

        if let IoBytesOrigin::Slice(raw) = arr.2 {
            unsafe { from_raw_parts_mut(raw, arr.1) }
        } else {
            panic!("from IoArr to Slice failed, invalid IoArr");
        }
    }
}

impl From<Vec<u8>> for IoBytes {
    fn from(mut vec: Vec<u8>) -> Self {
        let len = vec.len();
        if len > MAX_LENGTH {
            panic!("vectory to io array failed, invalid vectory length");
        }

        let raw = vec.as_mut_ptr();
        mem::forget(vec);

        IoBytes(false, len, IoBytesOrigin::Vec(raw))
    }
}

impl From<IoBytes> for Vec<u8> {
    fn from(mut arr: IoBytes) -> Self {
        arr.0 = true; //声明已释放

        if let IoBytesOrigin::Vec(raw) = arr.2 {
            unsafe { Vec::from_raw_parts(raw, arr.1, arr.1) }
        } else {
            panic!("from IoArr to Vec<u8> failed, invalid IoArr");
        }
    }
}

impl IoBytes {
    //构建一个指定容量的IO数据
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity > MAX_LENGTH {
            panic!("new io array failed, invalid array length");
        }

        let mut vec = Vec::with_capacity(capacity);
        vec.resize(capacity, 0);
        let raw = vec.as_mut_ptr();
        mem::forget(vec);

        IoBytes(false, capacity, IoBytesOrigin::Vec(raw))
    }

    //获取IO数据长度
    pub fn len(&self) -> usize {
        self.1
    }

    //获取只读引用
    pub fn as_ref(&self) -> &[u8] {
        match self.2 {
            IoBytesOrigin::Slice(raw) => {
                unsafe { from_raw_parts(raw as *const u8, self.1) }
            },
            IoBytesOrigin::Vec(raw) => {
                unsafe { from_raw_parts(raw as *const u8, self.1) }
            },
        }
    }

    //获取可写引用
    pub fn as_mut(&mut self) -> &mut [u8] {
        match self.2 {
            IoBytesOrigin::Slice(raw) => {
                unsafe { from_raw_parts_mut(raw, self.1) }
            },
            IoBytesOrigin::Vec(raw) => {
                unsafe { from_raw_parts_mut(raw, self.1) }
            },
        }
    }
}

/*
* IO列表
*/
pub struct IoList(usize, VecDeque<IoBytes>);

unsafe impl Send for IoList {}

impl From<Vec<IoBytes>> for IoList {
    fn from(vec: Vec<IoBytes>) -> Self {
        let mut len = 0;
        for arr in &vec {
            len += arr.len();
        }
        let queue: VecDeque<IoBytes> = vec.into();

        IoList(len, queue)
    }
}

impl From<IoList> for Vec<IoBytes> {
    fn from(list: IoList) -> Self {
        list.1.into()
    }
}

impl IoList {
    //构建一个指定初始容量的IO列表
    pub fn with_capacity(capacity: usize) -> Self {
        IoList(0, VecDeque::with_capacity(capacity))
    }

    //连接IO列表中的所有IO数据
    pub fn concat(self) -> Vec<u8> {
        let mut vecs  = Vec::from(self);
        let vecs: Vec<Vec<u8>> = vecs.into_iter().map(|bytes| {
            bytes.into()
        }).collect();

        vecs.concat()
    }

    //获取当前IO列表字节长度
    pub fn byte_len(&self) -> usize {
        self.0
    }

    //获取当前IO列表长度
    pub fn len(&self) -> usize {
        self.1.len()
    }

    //在列表前部增加IO数据
    pub fn push_front(&mut self, arr: IoBytes) {
        let len = arr.len();
        if len == 0 {
            return;
        }

        self.1.push_front(arr);
        self.0 += len;
    }

    //在列表后部增加IO数据
    pub fn push_back(&mut self, arr: IoBytes) {
        let len = arr.len();
        if len == 0 {
            return;
        }

        self.1.push_back(arr);
        self.0 += len;
    }

    //清空IO列表
    pub fn clear(&mut self) {
        self.1.clear();
        self.0 = 0;
    }
}

/*
* Tcp连接的事件
*/
#[derive(Debug)]
pub struct SocketEvent {
    inner: *mut (), //内部事件
}

unsafe impl Send for SocketEvent {}

impl SocketEvent {
    //创建空的事件
    pub fn empty() -> Self {
        SocketEvent {
            inner: ptr::null_mut(),
        }
    }

    //判断事件是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.is_null()
    }

    //获取事件
    pub fn get<T: 'static>(&self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { *Box::from_raw(self.inner as *mut T) })
    }

    //设置事件，如果当前事件不为空，则设置失败
    pub fn set<T: 'static>(&mut self, event: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner = Box::into_raw(Box::new(event)) as *mut T as *mut ();
        true
    }

    //移除事件，如果当前有事件，则返回被移除的事件
    pub fn remove<T: 'static>(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        let result = self.get();
        self.inner = ptr::null_mut();
        result
    }
}

/*
* 传输层安全协议配置
*/
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
    ) -> Result<Self, String> {
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
        for suite in &ALL_CIPHERSUITES {
            vec.push(format!("{:?}", suite.suite).to_lowercase());
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
                      server_alpns: Vec<String>) -> Result<Arc<rustls::ServerConfig>, String> {
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
                        return Err(format!("save client auth root failed, reason: {:?}", e));
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

    let mut config = rustls::ServerConfig::new(client_auth);
    config.key_log = Arc::new(rustls::KeyLogFile::new());

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

    let ocsp = if let Some(path) = server_ocsp_path {
        //配置了在线证书状态响应文件
        match load_ocsp(path) {
            Err(e) => {
                //加载在线证书状态协议错误，则立即返回错误原因
                return Err(e);
            },
            Ok(ocsp) => {
                //加载在线证书状态协议成功，则继续
                ocsp
            },
        }
    } else {
        //未配置在线证书状态响应文件
        Vec::new()
    };

    //设置指定私钥和OCSP响应的单证书链
    if let Err(e) = config.set_single_cert_with_ocsp_and_sct(certs, pk, ocsp, vec![]) {
        //设置单证书链错误，则立即返回错误原因
        return Err(format!("bad certificates or private key, reason: {:?}", e));
    }

    if !server_suite.is_empty() {
        match select_suites(&server_suite) {
            Err(e) => {
                //选择指定名称的密码套件错误，则立即返回错误原因
                return Err(e);
            },
            Ok(suites) => {
                //选择指定名称的密码套件成功
                config.ciphersuites = suites;
            },
        }
    }

    if !server_versions.is_empty() {
        match select_versions(&server_versions) {
            Err(e) => {
                //选择指定名称的Tls版本错误，则立即返回错误原因
                return Err(e);
            },
            Ok(versions) => {
                //选择指定名称的Tls版本成功
                config.versions = versions;
            },
        }
    }

    if server_session_size != 0 {
        //设置指定最大容量的服务器Tls会话缓存
        config.set_persistence(ServerSessionMemoryCache::new(server_session_size));
    }

    if is_server_tickets {
        //设置服务器Tls会话的票证分配器，用于向客户端分配Tls会话票证，客户端用服务器分配的Tls会话票证，可以安全快速的握手
        config.ticketer = rustls::Ticketer::new();
    }

    //设置ALPN协议列表，根据列表顺序确定协议优先级
    config.set_protocols(&server_alpns
        .iter()
        .map(|protocol| protocol.as_bytes().to_vec())
        .collect::<Vec<_>>()[..]);

    Ok(Arc::new(config))
}

//加载证书
fn load_certs(path: &Path) -> Result<Vec<rustls::Certificate>, String> {
    match File::open(path) {
        Err(e) => {
            return Err(format!("cannot open certificate file, path: {:?}, reason: {:?}", path, e));
        },
        Ok(cert_file) => {
            let mut reader = BufReader::new(cert_file);
            if let Ok(result) = pemfile::certs(&mut reader) {
                return Ok(result);
            }

            Err(format!("cannot parse certificate file, path: {:?}", path))
        },
    }
}

//加载私钥
fn load_private_key(path: &Path) -> Result<rustls::PrivateKey, String> {
    let rsa_keys = {
        match File::open(path) {
            Err(e) => {
                return Err(format!("cannot open private key file, path: {:?}, reason: {:?}", path, e));
            },
            Ok(key_file) => {
                let mut reader = BufReader::new(key_file);
                if let Ok(result) = pemfile::rsa_private_keys(&mut reader) {
                    result
                } else {
                    return Err(format!("file contains invalid rsa private key, path: {:?}", path));
                }
            },
        }
    };

    let pkcs8_keys = {
        match File::open(path) {
            Err(e) => {
                return Err(format!("cannot open private key file, path: {:?}, reason: {:?}", path, e));
            },
            Ok(key_file) => {
                let mut reader = BufReader::new(key_file);
                if let Ok(result) = pemfile::pkcs8_private_keys(&mut reader) {
                    result
                } else {
                    return Err(format!("file contains invalid pkcs8 private key (encrypted keys not supported), path: {:?}", path));
                }
            },
        }
    };

    if !pkcs8_keys.is_empty() {
        Ok(pkcs8_keys[0].clone())
    } else {
        Ok(rsa_keys[0].clone())
    }
}

//加载在线证书状态协议的响应文件
fn load_ocsp(path: &Path) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    match File::open(path) {
        Err(e) => {
            return Err(format!("cannot open ocsp file, path: {:?}, reason: {:?}", path, e));
        },
        Ok(mut ocsp_file) => {
            if let Err(e) = ocsp_file.read_to_end(&mut result) {
                return Err(format!("cannot read ocsp file, path: {:?}, reason: {:?}", path, e));
            }

            Ok(result)
        },
    }
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
    for suite in &rustls::ALL_CIPHERSUITES {
        let sname = format!("{:?}", suite.suite).to_lowercase();

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

/*
* 传输层安全协议会话
*/
pub enum TlsSession {
    Client(ClientSession),  //客户端会话
    Server(ServerSession),  //服务器会话
}

impl TlsSession {
    //判断Tls会话是否读就绪
    pub fn wants_read(&self) -> bool {
        match self {
            TlsSession::Client(session) => session.wants_read(),
            TlsSession::Server(session) => session.wants_read(),
        }
    }

    //判断Tls会话是否写就绪
    pub fn wants_write(&self) -> bool {
        match self {
            TlsSession::Client(session) => session.wants_write(),
            TlsSession::Server(session) => session.wants_write(),
        }
    }

    //判断Tls会话是否正在握手
    pub fn is_handshaking(&self) -> bool {
        match self {
            TlsSession::Client(session) => session.is_handshaking(),
            TlsSession::Server(session) => session.is_handshaking(),
        }
    }
}

