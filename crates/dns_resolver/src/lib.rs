use std::fmt::Formatter;
use std::sync::Arc;
use std::str::FromStr;
use std::io::{Error, Result, ErrorKind};
use std::net::{SocketAddr, ToSocketAddrs};
use hickory_proto::{rr::RecordType,
                    op::Query};
use hickory_resolver::{Name, Hosts, Resolver,
                       config::{ResolverConfig, ResolverOpts},
                       lookup::Lookup};

pub type DNSResolverConfig = ResolverConfig;
pub type DNSResolverOpts = ResolverOpts;
pub type DNSResolverRecordType = RecordType;

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