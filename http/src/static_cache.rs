use std::sync::Arc;
use std::collections::BTreeMap;
use std::thread::{Builder, sleep};
use std::time::{Duration, SystemTime};
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use mime::Mime;
use https::header::{IF_UNMODIFIED_SINCE, IF_MODIFIED_SINCE, LAST_MODIFIED, IF_NONE_MATCH, IF_MATCH, ETAG, CACHE_CONTROL};
use httpdate::{parse_http_date, fmt_http_date};
use parking_lot::RwLock;
use crossbeam_channel::{Sender, Receiver, unbounded};
use log::warn;

use pi_atom::Atom;
use tcp::driver::{Socket, AsyncIOWait};
use pi_hash::XHashMap;
use pi_adler32::adler32 as encode_adler32;

use crate::{request::HttpRequest, response::HttpResponse};
use crate::static_cache::CacheRes::Cache;

/*
* 支持的Http请求缓存指令
*/
const REQUEST_NO_STORE_CONTROL_CMD: &str = "no-store";  //不缓存资源
const REQUEST_MAX_AGE_CONTROL_CMD: &str = "max-age";    //获取过期时间不大于指定秒数的资源

/*
* 根据Http请求头的验证缓存是否未修改
*/
pub fn is_unmodified<S: Socket, W: AsyncIOWait>(cache: &StaticCache,
                                                req: &HttpRequest<S, W>,
                                                owner: Option<Atom>,
                                                key: Atom) -> Result<Option<bool>> {
    let mut reply = Ok(None);
    if let Some(value) = req.headers().get(IF_UNMODIFIED_SINCE.as_str()) {
        match value.to_str() {
            Err(e) => {
                return Err(Error::new(ErrorKind::Other, format!("valid http cache unmodified failed, reason: {:?}", e)));
            },
            Ok(date) => {
                match parse_http_date(date) {
                    Err(e) => {
                        return Err(Error::new(ErrorKind::Other, format!("valid http cache unmodified failed, reason: {:?}", e)));
                    },
                    Ok(client_last_modified) => {
                        reply = Ok(Some(cache.is_unmodified(owner.clone(), key.clone(), &client_last_modified)));
                    },
                }
            },
        }
    }

    if let Some(value) = req.headers().get(IF_MATCH.as_str()) {
        match value.to_str() {
            Err(e) => {
                return Err(Error::new(ErrorKind::Other, format!("valid http cache match failed, reason: {:?}", e)));
            },
            Ok(client_sign_str) => {
                if let CacheRes::Cache((_, _, sign, _)) = cache.get(owner, key) {
                    match client_sign_str.parse::<usize>() {
                        Ok(client_sign) if client_sign == sign => {
                            //指定缓存的资源未修改
                            reply = Ok(Some(true));
                        },
                        _ => {
                            //指定缓存的资源已修改
                            reply = Ok(Some(false));
                        },
                    }
                }
            },
        }
    }

    reply
}

/*
* 根据Http请求头的验证缓存是否已修改
*/
pub fn is_modified<S: Socket, W: AsyncIOWait>(cache: &StaticCache,
                                              req: &HttpRequest<S, W>,
                                              owner: Option<Atom>,
                                              key: Atom) -> Result<bool> {
    let mut reply = Ok(true);
    if let Some(value) = req.headers().get(IF_MODIFIED_SINCE.as_str()) {
        match value.to_str() {
            Err(e) => {
                return Err(Error::new(ErrorKind::Other, format!("valid http cache modified failed, reason: {:?}", e)));
            },
            Ok(date) => {
                match parse_http_date(date) {
                    Err(e) => {
                        return Err(Error::new(ErrorKind::Other, format!("valid http cache modified failed, reason: {:?}", e)));
                    },
                    Ok(client_last_modified) => {
                        reply = Ok(!cache.is_unmodified(owner.clone(), key.clone(), &client_last_modified));
                    },
                }
            },
        }
    }

    if let Some(value) = req.headers().get(IF_NONE_MATCH.as_str()) {
        match value.to_str() {
            Err(e) => {
                return Err(Error::new(ErrorKind::Other, format!("valid http cache match failed, reason: {:?}", e)));
            },
            Ok(client_sign_str) => {
                if let CacheRes::Cache((_, _, sign, _)) = cache.get(owner, key) {
                    match client_sign_str.parse::<usize>() {
                        Ok(client_sign) if client_sign == sign => {
                            //指定缓存的资源未修改
                            reply = Ok(false);
                        },
                        _ => {
                            //指定缓存的资源已修改
                            reply = Ok(true);
                        },
                    }
                }
            },
        }
    }

    reply
}

/*
* 根据Http请求头确定是否获取指定用户和名称的缓存，返回是否缓存资源、缓存的过期时长和缓存
*/
pub fn request_get_cache<S: Socket, W: AsyncIOWait>(cache: &StaticCache,
                                                    req: &HttpRequest<S, W>,
                                                    owner: Option<Atom>,
                                                    key: Atom) -> Result<(bool, u64, CacheRes)> {
    let mut is_store = true; //默认要缓存资源
    let mut max_age: u64 = 0; //默认缓存立即过期，即每次Http请求必须进行缓存是否未修改的验证
    if let Some(value) = req.headers().get(CACHE_CONTROL.as_str()) {
        //当前请求有缓存指令
        for val in String::from_utf8_lossy(value.as_bytes()).split(',').collect::<Vec<&str>>() {
            match val.trim() {
                REQUEST_NO_STORE_CONTROL_CMD => {
                    //客户端不缓存获取的资源，且要求服务器端也不缓存获取的资源，则设置移除标记
                    is_store = false;
                },
                any => {
                    if let Some(_) = any.find(REQUEST_MAX_AGE_CONTROL_CMD) {
                        //有max-age
                        match any.split('=').collect::<Vec<&str>>()[1].parse::<u64>() {
                            Err(e) => {
                                return Err(Error::new(ErrorKind::Other, format!("http request get cache failed, reason: {:?}", e)));
                            },
                            Ok(age) => {
                                max_age = age;
                            },
                        }
                    }
                },
            }
        }

        return Ok((is_store, max_age, cache.get(owner, key)));
    }

    Ok((is_store, max_age, CacheRes::Empty))
}

/*
* 设置缓存的响应头
*/
pub fn set_cache_resp_headers<S: Socket, W: AsyncIOWait>(resp: &mut HttpResponse<S, W>,
                                                         is_private: bool,
                                                         is_cache: bool,
                                                         is_store: bool,
                                                         is_transform: bool,
                                                         is_only_if_cached: bool,
                                                         max_age: u64,
                                                         last_modified: Option<SystemTime>,
                                                         etag: usize) {
    let mut cache_control_value = "".to_string();

    //设置是否是私有资源
    if is_private {
        cache_control_value = cache_control_value + "private";
    } else {
        cache_control_value = cache_control_value + "public";
    }

    //设置是否要求客户端每次请求强制验证资源是否过期
    if !is_cache {
        cache_control_value = cache_control_value + ",no-cache";
    }

    //设置是否缓存资源
    if !is_store {
        cache_control_value = cache_control_value + ",no-store";
    }

    //设置是否允许客户端更改前端资源内容
    if !is_transform {
        cache_control_value = cache_control_value + ",no-transform";
    }

    //设置是否要求代理有缓存，则只由代理向客户端提供资源
    if is_only_if_cached {
        cache_control_value = cache_control_value + ",only_if_cached";
    }

    //设置指定缓存资源的过期时长
    cache_control_value  = cache_control_value + ",max-age=" + &max_age.to_string();

    //设置Cache-Control头，用于控制客户端或代理的缓存策略
    resp.header(CACHE_CONTROL.as_str(), cache_control_value.as_str());

    //设置Last-Modified头，用于客户端或代理向服务器进行缓存验证
    if let Some(last_modified) = last_modified {
        resp.header(LAST_MODIFIED.as_str(), fmt_http_date(last_modified).as_str());
    }

    //设置Etag头，用于客户端或代理向服务器进行缓存验证
    resp.header(ETAG.as_str(), etag.to_string().as_str());
}

/*
* Http缓存资源主键
*/
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum CacheKey {
    Private((Atom, Atom)),  //私有缓存主键
    Public(Atom),           //公共缓存主键
}

impl CacheKey {
    //判断是否是私有缓存主键
    pub fn is_private(&self) -> bool {
        match self {
            CacheKey::Private(_) =>  true,
            _ => false,
        }
    }
}

/*
* Http缓存整理控制指令
*/
#[derive(Debug, Clone)]
enum CollectCmd {
    Stop(String),                   //关闭整理
    Pause(String),                  //暂停整理
    Continue(String),               //继续整理
    Index((Duration, CacheKey)),    //更新缓存超时索引
    Clear(usize),                   //清理不小于指定大小的缓存，如果为0表示清理缓存最久的资源
}

/*
* Http缓存资源
*/
#[derive(Debug, Clone)]
pub enum CacheRes {
    Empty,                                          //缓存不存在
    Expired,                                        //缓存过期
    Cache((SystemTime, Mime, usize, Arc<Vec<u8>>)), //有效缓存
}

/*
* Http静态资源缓存
*/
pub struct StaticCache {
    max_size:       usize,                                                                          //缓存资源最大大小，单位字节
    max_len:        usize,                                                                          //缓存资源最大数量
    cache:          RwLock<XHashMap<CacheKey, (Duration, SystemTime, Mime, usize, Arc<Vec<u8>>)>>,  //公共缓存，值包括超时时间、最近修改时间、资源类型、签名和资源数据
    timeout_index:  RwLock<BTreeMap<Duration, CacheKey>>,                                           //缓存超时索引
    size:           AtomicUsize,                                                                    //缓存资源当前大小，单位字节
    len:            AtomicUsize,                                                                    //缓存资源当前数量
    is_running:     AtomicBool,                                                                     //缓存整理是否运行中
    collect_sent:   Sender<CollectCmd>,                                                             //缓存整理控制指令发送者
    collect_recv:   Receiver<CollectCmd>,                                                           //缓存整理控制指令接收者
}

unsafe impl Send for StaticCache {}
unsafe impl Sync for StaticCache {}

impl StaticCache {
    //构建指定缓存资源最大大小和最大数量的Http静态资源缓存，
    pub fn new(max_size: usize, max_len: usize) -> Self {
        let (collect_sent, collect_recv) = unbounded();

        StaticCache {
            max_size,
            max_len,
            cache: RwLock::new(XHashMap::default()),
            timeout_index: RwLock::new(BTreeMap::new()),
            size: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
            is_running: AtomicBool::new(false),
            collect_sent,
            collect_recv,
        }
    }

    //运行指定整理时间间隔的缓存整理
    pub fn run_collect(cache: Arc<Self>, name: String, collect_time: u64) {
        if cache.is_running.load(Ordering::SeqCst) {
            //整理已运行，则忽略
            return;
        }

        let thread_name = name + "-" + "StaticCache";
        Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                //设置缓存的整理状态为已运行
                if cache.is_running.compare_and_swap(false, true, Ordering::SeqCst) {
                    //当前缓存的整理状态为已运行，则忽略
                    warn!("!!!> Http Static Cache Already Continue, name: {:?}, reason: compare swap failed", thread_name);
                    return;
                }

                let mut is_runing = true;
                let mut is_collect = true;
                let timeout = Duration::from_millis(collect_time);
                loop {
                    //处理整理控制指令
                    for cmd in cache.collect_recv.try_iter().collect::<Vec<CollectCmd>>() {
                        match cmd {
                            CollectCmd::Continue(reason) => {
                                if !is_collect {
                                    is_collect = true; //当前不整理，则设置为整理
                                    warn!("!!!> Http Static Cache Already Continue, name: {:?}, reason: {:?}", thread_name, reason);
                                }
                            },
                            CollectCmd::Pause(reason) => {
                                if is_collect {
                                    //当前整理，则设置为不整理
                                    is_collect = false;
                                    warn!("!!!> Http Static Cache Already Pause, name: {:?}, reason: {:?}", thread_name, reason);
                                }
                            },
                            CollectCmd::Stop(reason) => {
                                //停止缓存的整理
                                is_runing = false;
                                warn!("!!!> Http Static Cache Already Stop, name: {:?}, reason: {:?}", thread_name, reason);
                            },
                            CollectCmd::Index((timeout, key)) => {
                                //更新缓存的超时索引
                                cache.timeout_index.write().insert(timeout, key);
                            },
                            CollectCmd::Clear(clear_size) => {
                                //强制清理缓存
                                collect_clear(cache.as_ref(), clear_size);
                            },
                        }
                    }

                    if !is_runing {
                        //已停止运行，则立即退出循环
                        break;
                    }

                    //整理缓存资源
                    if is_collect {
                        sleep(timeout); //休眠指定的毫秒数
                        collect_expired(cache.as_ref());
                    }
                }

                cache.is_running.store(false, Ordering::SeqCst); //设置缓存的整理状态为已停止
            });
    }

    //获取当前缓存资源大小
    pub fn size(&self) -> usize {
        self.size.load(Ordering::SeqCst)
    }

    //获取当前缓存数量
    pub fn len(&self) -> usize {
        self.len.load(Ordering::SeqCst)
    }

    //检查指定用户和名称的缓存是否存在
    pub fn contains(&self, owner: Option<Atom>, key: Atom) -> bool {
        if let Some(owner) = owner {
            //私有缓存
            self.cache.read().contains_key(&CacheKey::Private((owner, key)))
        } else {
            //公共缓存
            self.cache.read().contains_key(&CacheKey::Public(key))
        }
    }

    //检查指定用户和名称的缓存是否过期
    pub fn is_expired(&self, owner: Option<Atom>, key: Atom) -> bool {
        if let Some(owner) = owner {
            //私有缓存
            if let Some((timeout, _, _, _, _)) = self.cache.read().get(&CacheKey::Private((owner, key))) {
                if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    return &now >= timeout;
                }
            }
        } else {
            //公共缓存
            if let Some((timeout, _, _, _, _)) = self.cache.read().get(&CacheKey::Public(key)) {
                if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    return &now >= timeout;
                }
            }
        }

        //不存在，则返回过期
        true
    }

    //检查指定用户、名称和上次修改时间的缓存是否未修改
    pub fn is_unmodified(&self, owner: Option<Atom>, key: Atom, last: &SystemTime) -> bool {
        if let Some(owner) = owner {
            //私有缓存
            if let Some((_, last_modified, _, _, _)) = self.cache.read().get(&CacheKey::Private((owner, key))) {
                if let Ok(last) = last.duration_since(SystemTime::UNIX_EPOCH) {
                    if let Ok(last_modified) = last_modified.duration_since(SystemTime::UNIX_EPOCH) {
                        return last.as_secs() == last_modified.as_secs();
                    }
                }

                false
            } else {
                //指定的私有缓存不存在，则返回已修改
                false
            }
        } else {
            //公共缓存
            if let Some((_, last_modified, _, _, _)) = self.cache.read().get(&CacheKey::Public(key)) {
                if let Ok(last) = last.duration_since(SystemTime::UNIX_EPOCH) {
                    if let Ok(last_modified) = last_modified.duration_since(SystemTime::UNIX_EPOCH) {
                        return last.as_secs() == last_modified.as_secs();
                    }
                }

                false
            } else {
                //指定的公共缓存不存在，则返回已修改
                false
            }
        }
    }

    //获取指定用户和名称的缓存，过期则返回空
    pub fn get(&self, owner: Option<Atom>, key: Atom) -> CacheRes {
        if let Some(owner) = owner {
            //私有缓存
            if let Some((timeout, last_modified, mime, sign, bin)) = self.cache.read().get(&CacheKey::Private((owner, key))) {
                if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    if &now >= timeout {
                        //指定的缓存已过期
                        return CacheRes::Expired;
                    }

                    return CacheRes::Cache((last_modified.clone(), mime.clone(), sign.clone(), bin.clone()))
                }
            }
        } else {
            //公共缓存
            if let Some((timeout, last_modified, mime, sign, bin)) = self.cache.read().get(&CacheKey::Public(key)) {
                if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    if &now >= timeout {
                        //指定的缓存已过期
                        return CacheRes::Expired;
                    }

                    return CacheRes::Cache((last_modified.clone(), mime.clone(), sign.clone(), bin.clone()))
                }
            }
        }

        CacheRes::Empty
    }

    //插入指定用户、名称、类型和有效时长的缓存，同名缓存则覆盖，并返回指定名称的上个缓存资源，有效时长单位为秒。同时更新缓存的超时索引
    pub fn insert(&self, owner: Option<Atom>, max_age: u64, last_modified: SystemTime, mime: Mime, full_sign: bool, key: Atom, value: Arc<Vec<u8>>) -> Result<(usize, CacheRes)> {
        let value_size = value.len();
        if self.size.load(Ordering::SeqCst) + value_size > self.max_size {
            //已超过指定的缓存大小限制，则强制缓存进行整理，并返回错误
            self.collect_sent.send(CollectCmd::Clear(value_size));
            return Err(Error::new(ErrorKind::Other, format!("insert http static cache failed, owner: {:?}, max_age: {:?}, mime: {:?}, key: {:?}, len: {:?}, reason: cache size full", owner, max_age, mime, key, value.len())));
        }

        if self.len.load(Ordering::SeqCst) + 1 > self.max_len {
            //已超过指定的缓存数量限制，则强制缓存进行整理，返回错误
            self.collect_sent.send(CollectCmd::Clear(0));
            return Err(Error::new(ErrorKind::Other, format!("insert http static cache failed, owner: {:?}, max_age: {:?}, mime: {:?}, key: {:?}, len: {:?}, reason: cache length full", owner, max_age, mime, key, value.len())));
        }

        match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("insert http static cache failed, owner: {:?}, max_age: {:?}, mime: {:?}, key: {:?}, len: {:?}, reason: {:?}", owner, max_age, mime, key, value.len(), e)))
            },
            Ok(now) => {
                if let Some(timeout) = now.clone().checked_add(Duration::from_secs(max_age)) {
                    let cache_key = if let Some(owner) = owner {
                        CacheKey::Private((owner, key))
                    } else {
                        CacheKey::Public(key)
                    };

                    //计算缓存资源的签名
                    let sign = if full_sign {
                        //完整签名
                        if let Ok(sign) = encode_adler32(value.as_slice()) {
                            //完整签名成功
                            sign as usize
                        } else {
                            //完整签名失败
                            0
                        }
                    } else {
                        //简单签名
                        if let Ok(sign) = encode_adler32(fmt_http_date(last_modified).as_bytes()) {
                            //简单签名成功
                            sign as usize
                        } else {
                            //简单签名失败
                            0
                        }
                    };

                    self.collect_sent.send(CollectCmd::Index((timeout, cache_key.clone()))); //更新缓存的超时索引
                    if cache_key.is_private() {
                        //私有缓存
                        if let Some((old_timeout, old_last_modifed, old_mime, old_sign, old_value)) = self.cache.write().insert(cache_key, (timeout, last_modified, mime, sign, value)) {
                            //更新私有缓存成功，则更新缓存大小
                            self.size.fetch_sub(old_value.len(), Ordering::SeqCst);
                            self.size.fetch_add(value_size, Ordering::SeqCst);

                            if now >= old_timeout {
                                //上个缓存已过期
                                return Ok((sign, CacheRes::Expired));
                            }

                            return Ok((sign, CacheRes::Cache((old_last_modifed, old_mime, old_sign, old_value))));
                        }
                    } else {
                        //公共缓存
                        if let Some((old_timeout, old_last_modifed, old_mime, old_sign, old_value)) = self.cache.write().insert(cache_key, (timeout, last_modified, mime, sign, value)) {
                            //更新公共缓存成功，则更新缓存大小
                            self.size.fetch_sub(old_value.len(), Ordering::SeqCst);
                            self.size.fetch_add(value_size, Ordering::SeqCst);

                            if now >= old_timeout {
                                //上个缓存已过期
                                return Ok((sign, CacheRes::Expired));
                            }

                            return Ok((sign, CacheRes::Cache((old_last_modifed, old_mime, old_sign, old_value))));
                        }
                    }

                    //插入缓存成功，则增加缓存大小和数量
                    self.size.fetch_add(value_size, Ordering::SeqCst);
                    self.len.fetch_add(1, Ordering::SeqCst);
                    return Ok((sign, CacheRes::Empty));
                }

                Err(Error::new(ErrorKind::Other, format!("insert http static cache failed, owner: {:?}, max_age: {:?}, mime: {:?}, key: {:?}, len: {:?}, reason: invalid max age", owner, max_age, mime, value.len(), key)))
            },
        }
    }

    //移除指定用户和名称的缓存，但不移除缓存超时索引
    pub fn remove(&self, owner: Option<Atom>, key: Atom) -> CacheRes {
        let cache_key = if let Some(owner) = owner {
            //私有缓存
            if let Some((timeout, last_modified, mime, sign, bin)) = self.cache.write().remove(&CacheKey::Private((owner, key))) {
                //移除私有缓存成功
                self.size.fetch_sub(bin.len(), Ordering::SeqCst);
                self.len.fetch_sub(1, Ordering::SeqCst);

                if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    if now >= timeout {
                        //指定的缓存已过期
                        return CacheRes::Expired;
                    }

                    return CacheRes::Cache((last_modified, mime, sign, bin));
                }
            }
        } else {
            //公共缓存
            if let Some((timeout, last_modified, mime, sign, bin)) = self.cache.write().remove(&CacheKey::Public(key)) {
                //移除公共缓存成功
                self.size.fetch_sub(bin.len(), Ordering::SeqCst);
                self.len.fetch_sub(1, Ordering::SeqCst);

                if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    if now >= timeout {
                        //指定的缓存已过期
                        return CacheRes::Expired;
                    }

                    return CacheRes::Cache((last_modified, mime, sign, bin));
                }
            }
        };

        CacheRes::Empty
    }

    /// 移除所有缓存
    pub fn remove_all_cache(&self) {
        self.cache.write().clear();
    }

    //暂停缓存的整理
    pub fn pause_collect(&self, reason: String) -> Result<()> {
        if !self.is_running.load(Ordering::SeqCst) {
            //整理已停止，则忽略
            return Ok(());
        }

        if let Err(e) = self.collect_sent.send(CollectCmd::Pause(reason)) {
            return Err(Error::new(ErrorKind::ConnectionAborted, format!("pause http static cache collect failed, reason: {:?}", e)));
        }

        Ok(())
    }

    //继续缓存的整理
    pub fn continue_collect(&self, reason: String) -> Result<()> {
        if !self.is_running.load(Ordering::SeqCst) {
            //整理已停止，则忽略
            return Ok(());
        }

        if let Err(e) = self.collect_sent.send(CollectCmd::Continue(reason)) {
            return Err(Error::new(ErrorKind::ConnectionAborted, format!("continue http static cache collect failed, reason: {:?}", e)));
        }

        Ok(())
    }
}

//整理过期缓存资源
fn collect_expired(cache: &StaticCache) {
    let mut kvs = cache.timeout_index
        .read()
        .iter()
        .map(|(timeout, key)| {
            (timeout.clone(), key.clone())
        })
        .collect::<Vec<(Duration, CacheKey)>>();
    kvs.reverse(); //反转顺序

    if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        for (timeout, key) in kvs {
            if now >= timeout {
                //当前的缓存资源已过期，则从缓存和缓存超时索引中移除，并继续整理
                cache.timeout_index.write().remove(&timeout);
                match key {
                    CacheKey::Private((owner, key)) => {
                        cache.remove(Some(owner), key);
                    },
                    CacheKey::Public(key) => {
                        cache.remove(None, key);
                    },
                }
                continue;
            }

            //当前的缓存资源未过期，则后续的缓存资源肯定也未过期
            break;
        }
    }
}

//强制整理缓存
fn collect_clear(cache: &StaticCache, mut size: usize) {
    if size == 0 {
        //移除缓存最久的资源
        if let Some(e) = cache.timeout_index.write().last_entry() {
            match e.remove() {
                CacheKey::Private((owner, key)) => {
                    cache.remove(Some(owner), key);
                },
                CacheKey::Public(key) => {
                    cache.remove(None, key);
                },
            }
        }

        return;
    }

    //移除不小于指定大小的缓存，则以缓存时间从久到新的顺序，移除缓存的资源
    let mut kvs = cache.timeout_index
        .read()
        .iter()
        .map(|(timeout, key)| {
            (timeout.clone(), key.clone())
        })
        .collect::<Vec<(Duration, CacheKey)>>();
    kvs.reverse(); //反转顺序

    for (timeout, _) in kvs {
        if size == 0 {
            //已移除不小于指定大小的缓存，则立即退出强制整理
            return;
        }

        let res = if let Some(value) = cache.timeout_index.write().remove(&timeout) {
            match value {
                CacheKey::Private((owner, key)) => {
                    cache.remove(Some(owner), key)
                },
                CacheKey::Public(key) => {
                    cache.remove(None, key)
                },
            }
        } else {
            CacheRes::Empty
        };

        if let CacheRes::Cache((_, _, _, bin)) = res {
            if let Some(n) = size.checked_sub(bin.len()) {
                size = n;
            } else {
                size = 0;
            }
        }
    }
}