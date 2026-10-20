//! DNS解析服务；运行时只在join_dns_resolver的原构建后/全局发布前冷登记。
//! 默认runtime_metrics功能不安装采集器/时钟，不保存生产ID，不处理关闭或替换。
//! 登记会由上游强保活，原close不会注销；旧核及在途采样限制由使用者理解，
//! 不因登记成功宣称线程就绪、采样完成或释放。本次边界为NET-MET-BATCH-DESIGN-001/B-DNS。
//! 登记参数/成功验证：batch_runtime_metrics_tests::test_dns_registers_runtime_metrics。

use std::str::FromStr;
use std::fmt::Formatter;
use std::sync::{Arc, OnceLock};
use std::io::{Error, Result, ErrorKind};
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::Deref;
use pi_async_rt::rt::{serial::AsyncRuntimeBuilder,
                      serial_local_thread::LocalTaskRuntime};
use dashmap::DashMap;
use hickory_proto::{rr::RecordType,
                    op::Query};
use hickory_resolver::{Name, Hosts, Resolver,
                       config::{ResolverConfig, ResolverOpts},
                       lookup::Lookup};

// 仅测试构建观测实际调用的纯参数/返回值，不持核心、不中转生产行为。
#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
std::thread_local! {
    static DNS_METRICS_TEST_RESULT: std::cell::RefCell<Option<(usize, String,
        std::result::Result<pi_async_rt::runtime_metrics::RuntimeMetricId,
            pi_async_rt::runtime_metrics::RuntimeMetricRegistrationError>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, feature = "runtime_metrics", not(target_arch = "wasm32")))]
mod batch_runtime_metrics_tests {
    //! 仅验证公开join_dns_resolver在真实构建/发布之间的登记参数及原Ok。
    //! 局部单元只预备原全局空表，不替生产登记；使用真实系统解析器但不查询域名/网络。
    //! 不验证替换/采集/SDK，原关闭不改；清理自有核心前用有界通知确认原线程已启动。

    use super::*;

    /// 被测生产：join_dns_resolver，必须库内登记pi_dns_resolver-local-原get_id。
    /// 返回值来自真实调用，断言前注销自有ID、移除自有表项并沿原close清理；
    /// 通知仅为夹具退出准备，不证明采样收尾；O(L)命名核对，最长等待2秒，无指标采集。
    #[test]
    fn test_dns_registers_runtime_metrics() {
        DNS_METRICS_TEST_RESULT.with(|value| { value.borrow_mut().take(); });
        let map = GLOBAL_DNS_RESOLVE_SERVICES.get_or_init(DashMap::new);
        join_dns_resolver("runtime-metrics-unit", DNSResolver::with_system().expect("真实系统解析器构造"));
        let observed = DNS_METRICS_TEST_RESULT.with(|value| value.borrow_mut().take());
        if let Some((_, _, Ok(id))) = &observed {
            let _ = pi_async_rt::runtime_metrics::unregister_runtime_metrics(*id);
        }
        let (owned, _) = map.remove("runtime-metrics-unit").expect("原join必须发布解析器").1;
        let expected_id = owned.get_id();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        owned.send(async move { let _ = ready_tx.send(()); });
        let ready = ready_rx.recv_timeout(std::time::Duration::from_secs(2));
        owned.close();
        ready.expect("原DNS所属线程必须启动才能完成夹具清理");
        let (actual_id, actual_name, registration) = observed.expect("原join必须实际登记");
        assert_eq!(actual_id, expected_id);
        assert_eq!(actual_name, format!("pi_dns_resolver-local-{}", expected_id));
        assert!(registration.is_ok(), "真实登记必须成功: {:?}", registration);
    }
}

// DNS解析器运行时
struct DNSResolverRuntime(LocalTaskRuntime<()>);

unsafe impl Sync for DNSResolverRuntime {}

impl Deref for DNSResolverRuntime {
    type Target = LocalTaskRuntime<()>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DNSResolverRuntime {
    /// 关闭DNS解析器运行时
    pub fn close(self) {
        self.0.close();
    }
}

///
/// 默认的DNS解析器名
///
pub const DEFAULT_DNS_RESOLVER_NAME: String = String::new();

// 全局域名解析服务列表
static GLOBAL_DNS_RESOLVE_SERVICES: OnceLock<DashMap<String, (DNSResolverRuntime, DNSResolver)>> = OnceLock::new();

///
/// 初始化全局DNS域名解析服务，并提供一个默认的全局DNS解析器
///
pub fn init_global_dns_resolve_service() {
    let _ = GLOBAL_DNS_RESOLVE_SERVICES.get_or_init(|| {
        DashMap::new()
    });

    join_dns_resolver(DEFAULT_DNS_RESOLVER_NAME,
                      DNSResolver::with_system_conf().unwrap());
}

///
/// 加入一个全局DNS解析器，每个解析器由一个异步运行时推动
///
/// name是保存的业务键，允许空键/重复键；resolver按原逻辑移动保存。
/// 未初始化全局服务时保持原无操作；重复键仍创建新核并立即close旧核，返回()不表示
/// 线程就绪或指标成功。默认在构建器返回后、insert前登记pi_dns_resolver-local-原编号，
/// 失败只告警继续，不新增ID字段/注销/等待/Drop或解析任务逻辑。
/// 名称处理O(L)时间/空间，另有上游登记短同步锁、分配及宿主logger成本，可能等待；
/// 无await/异步锁，不应在解析热路径反复调用。非纯、重复调用非幂等；线程与原表合同不变。
/// 成功登记强保活，即使原close也不会自动撤销，反复替换可能保留旧核并消耗历史预算。
/// 示例：全局服务初始化后join_dns_resolver("system", DNSResolver::with_system()?);
/// 登记专项：batch_runtime_metrics_tests::test_dns_registers_runtime_metrics，仅参数/Ok。
pub fn join_dns_resolver<N: AsRef<str>>(name: N,
                                        resolver: DNSResolver) {
    if let Some(map) = GLOBAL_DNS_RESOLVE_SERVICES.get() {
        let rt = AsyncRuntimeBuilder
        ::default_local_thread(Some(("DNS-Resolver-".to_string() + name.as_ref()).as_str()),
                               None);

        #[cfg(all(feature = "runtime_metrics", not(target_arch = "wasm32")))]
        {
            // 只登记原核心，不改变原启动、发布、同名替换或关闭时序。
            let metric_name = format!("pi_dns_resolver-local-{}", rt.get_id());
            let registration = pi_async_rt::runtime_metrics::register_runtime_metrics(&rt, &metric_name);
            #[cfg(test)]
            DNS_METRICS_TEST_RESULT.with(|value| {
                *value.borrow_mut() = Some((rt.get_id(), metric_name.clone(), registration));
            });
            if let Err(error) = registration {
                log::warn!("DNS 运行时指标注册失败, 名称: {}, 原因: {}", metric_name, error);
            }
        }

        if let Some((last_rt, _last_resolver)) = map
            .insert(name.as_ref().to_string(),
                    (DNSResolverRuntime(rt), resolver))
        {
            last_rt.close();
        }
    }
}

///
/// 异步用指定DNS解析器解析指定的域名的ip，不会阻塞当前线程
///
pub fn async_resolve_ip<N, F>(name: Option<N>,
                              host_name: N,
                              callback: F) -> Result<()>
where N: AsRef<str> + 'static,
      F: FnOnce(Result<Vec<SocketAddr>>) + 'static
{
    let resolver_name = if let Some(name) = name {
        //指定了DNS解析器
        name.as_ref().to_string()
    } else {
        //未指定DNS解析器
        DEFAULT_DNS_RESOLVER_NAME
    };

    if let Some(map) = GLOBAL_DNS_RESOLVE_SERVICES.get() {
        if let Some(item) = map.get(&resolver_name) {
            let (rt, resolver) = item.value();
            let resolver_copy = resolver.clone();
            rt.send(async move {
                let result = resolver_copy.lookup_ip(host_name);
                callback(result);
            });
        }

        Ok(())
    } else {
        Err(Error::new(ErrorKind::BrokenPipe,
                       format!("Async resolve ip failed, name: {:?}, reason: resolver not exist",
                               resolver_name)))
    }
}

pub type DNSResolverConfig = ResolverConfig;
pub type DNSResolverOpts = ResolverOpts;
pub type DNSResolverRecordType = RecordType;

///
/// 获取本地域名
///
pub fn local_domain() -> Option<Name> {
    if let Some(domain) = ResolverConfig::default().domain() {
        Some(domain.clone())
    } else {
        None
    }
}

///
/// DNS解析器
///
#[derive(Debug)]
pub struct DNSResolver(Arc<InnerDNSResolver>);

impl Clone for DNSResolver {
    fn clone(&self) -> Self {
        DNSResolver(self.0.clone())
    }
}

/*
* DNS解析器同步方法
*/
impl DNSResolver {
    /// 使用指定配置构建一个DNS解析器
    pub fn new(config: DNSResolverConfig,
               opts: DNSResolverOpts) -> Result<Self> {
        let resolver = Resolver::new(config, opts)?;
        let inner = InnerDNSResolver::Custom(resolver);

        Ok(DNSResolver(Arc::new(inner)))
    }

    /// 只使用本地主机配置的域名解析
    pub fn with_local_hosts_conf() -> Result<Self> {
        let inner = InnerDNSResolver::Local(Hosts::new());
        Ok(DNSResolver(Arc::new(inner)))
    }

    /// 使用系统域名解析
    /// 此解析会直接调用本地系统的域名解析器
    pub fn with_system() -> Result<Self> {
        let inner = InnerDNSResolver::System;
        Ok(DNSResolver(Arc::new(inner)))
    }

    /// 使用系统配置的本地域名解析，只会使用本地系统的域名解析配置，不会调用本地系统的域名解析器
    pub fn with_system_conf() -> Result<Self> {
        let resolver = Resolver::from_system_conf()?;
        let inner = InnerDNSResolver::Custom(resolver);

        Ok(DNSResolver(Arc::new(inner)))
    }

    /// 使用Google的域名解析
    pub fn with_google() -> Result<Self> {
        let resolver = Resolver::new(ResolverConfig::google(), ResolverOpts::default())?;
        let inner = InnerDNSResolver::Custom(resolver);

        Ok(DNSResolver(Arc::new(inner)))
    }

    /// 使用Cloudflare的域名解析
    pub fn with_cloudflare() -> Result<Self> {
        let resolver = Resolver::new(ResolverConfig::cloudflare(), ResolverOpts::default())?;
        let inner = InnerDNSResolver::Custom(resolver);

        Ok(DNSResolver(Arc::new(inner)))
    }

    /// 使用Quad9的域名解析
    pub fn with_quad9() -> Result<Self> {
        let resolver = Resolver::new(ResolverConfig::quad9(), ResolverOpts::default())?;
        let inner = InnerDNSResolver::Custom(resolver);

        Ok(DNSResolver(Arc::new(inner)))
    }

    /// 是否是只使用本地主机配置的DNS解析器
    pub fn is_local_hosts(&self) -> bool {
        if let InnerDNSResolver::Local(_) = self.0.as_ref() {
            true
        } else {
            false
        }
    }

    /// 是否是使用系统域名解析的DNS解析器
    pub fn is_system(&self) -> bool {
        if let InnerDNSResolver::System = self.0.as_ref() {
            true
        } else {
            false
        }
    }

    /// 是否是使用自定义配置的DNS解析器
    pub fn is_custom(&self) -> bool {
        if let InnerDNSResolver::Custom(_) = self.0.as_ref() {
            true
        } else {
            false
        }
    }

    /// 同步查找指定名称的IP，可能会非常缓慢
    pub fn lookup_ip<N: AsRef<str>>(&self, name: N)
        -> Result<Vec<SocketAddr>>
    {
        match self.0.as_ref() {
            InnerDNSResolver::Local(hosts) => {
                match Name::from_str(name.as_ref()) {
                    Err(e) => {
                        Err(Error::new(ErrorKind::AddrNotAvailable,
                                       format!("{}", e)))
                    },
                    Ok(query_name) => {
                        if let Some(lookup) = hosts
                            .lookup_static_host(&Query::query(query_name,
                                                              RecordType::A.into())) {
                            Ok(lookup
                                .iter()
                                .filter_map(|data| {
                                    if let Some(ip) = data.ip_addr() {
                                        Some(SocketAddr::new(ip, 0))
                                    } else {
                                        None
                                    }
                                }).collect())
                        } else {
                            Ok(vec![])
                        }
                    },
                }
            },
            InnerDNSResolver::System => {
                let addrs = if name.as_ref().find(":").is_some() {
                    name.as_ref().to_socket_addrs()?
                } else {
                    (name.as_ref().to_string() + ":0").as_str().to_socket_addrs()?
                };
                Ok(addrs.map(|addr| {
                    addr
                }).collect())
            },
            InnerDNSResolver::Custom(resolver) => {
                match resolver.lookup_ip(name.as_ref()) {
                    Err(e) => {
                        Err(Error::new(ErrorKind::AddrNotAvailable,
                                       format!("{}", e)))
                    },
                    Ok(lookup) => {
                        Ok(lookup.iter().map(|ip| {
                            SocketAddr::new(ip, 0)
                        }).collect())
                    },
                }
            },
        }
    }

    /// 同步查找指定名称的信息，可能会非常缓慢
    pub fn lookup<N: AsRef<str>>(&self,
                                 name: N,
                                 record_type: DNSResolverRecordType)
        -> Result<Lookup>
    {
        match self.0.as_ref() {
            InnerDNSResolver::Custom(resolver) => {
                match resolver.lookup(name.as_ref(), record_type) {
                    Err(e) => {
                        Err(Error::new(ErrorKind::AddrNotAvailable,
                                       format!("{}", e)))
                    },
                    Ok(lookup) => {
                        Ok(lookup)
                    },
                }
            },
            _ => {
                Err(Error::new(ErrorKind::AddrNotAvailable,
                               format!("Lookup name failed, name: {:?}, reason: disable lookup with hosts config or system resolver",
                                       name.as_ref())))
            }
        }
    }
}

// 内部DNS解析器
enum InnerDNSResolver {
    Local(Hosts),       //本地主机文件域名解析
    System,             //系统域名解析
    Custom(Resolver),   //自定义域名解析
}

impl std::fmt::Debug for InnerDNSResolver {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Self::Custom(_) = self {
            write!(f, "Custom")
        } else {
            write!(f, "{:?}", self)
        }
    }
}
