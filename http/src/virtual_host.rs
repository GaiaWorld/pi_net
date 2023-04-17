use std::sync::Arc;
use std::io::{Error, Result, ErrorKind};

use pi_hash::XHashMap;
use pi_atom::Atom;
use pi_handler::{Args, Handler, SGenType};

use tcp::Socket;

use crate::{service::{HttpService, ServiceFactory},
            gateway::{GatewayContext, HttpGateway},
            route::{RouterTab, HttpRoute},
            middleware::Middleware};

///
/// 虚拟主机池
///
pub trait VirtualHostPool<S: Socket>: Clone + Send + Sync + 'static {
    type Host: ServiceFactory<S>;

    //获取虚拟主机数量
    fn size(&self) -> usize;

    //获取指定主机名的虚拟主机
    fn get(&self, name: &str) -> Option<&Self::Host>;

    //增加指定主机名的虚拟主机
    fn add(&mut self, name: &str, host: Self::Host) -> Result<()>;

    //增加默认的虚拟主机
    fn add_default(&mut self, host: Self::Host) -> Result<()>;
}

///
/// 虚拟主机表
///
pub struct VirtualHostTab<S: Socket, H: Middleware<S, GatewayContext>>(Arc<XHashMap<Atom, VirtualHost<S, H>>>);

unsafe impl<S: Socket, H: Middleware<S, GatewayContext>> Send for VirtualHostTab<S, H> {}
unsafe impl<S: Socket, H: Middleware<S, GatewayContext>> Sync for VirtualHostTab<S, H> {}

impl<S: Socket, H: Middleware<S, GatewayContext>> Clone for VirtualHostTab<S, H> {
    fn clone(&self) -> Self {
        VirtualHostTab(self.0.clone())
    }
}

impl<S: Socket, H: Middleware<S, GatewayContext>> VirtualHostPool<S> for VirtualHostTab<S, H> {
    type Host = VirtualHost<S, H>;

    fn size(&self) -> usize {
        self.0.len()
    }

    fn get(&self, name: &str) -> Option<&Self::Host> {
        let host: Vec<&str> = name.split(":").collect();
        if let Some(host) = self.0.get(&Atom::from(host[0])) {
            Some(host)
        } else {
            self.0.get(&Atom::from(""))
        }
    }

    fn add(&mut self, name: &str, host: Self::Host) -> Result<()> {
        if let Some(tab) = Arc::get_mut(&mut self.0) {
            tab.insert(Atom::from(name), host);
            return Ok(());
        }

        Err(Error::new(ErrorKind::Other,
                       format!("Add virtual host error, host: {:?}, reason: not writable",
                               name)))
    }

    fn add_default(&mut self, host: Self::Host) -> Result<()> {
        self.add("", host)
    }
}

impl<S: Socket, H: Middleware<S, GatewayContext>> VirtualHostTab<S, H> {
    //构建虚拟主机表
    pub fn new() -> Self {
        VirtualHostTab(Arc::new(XHashMap::default()))
    }
}

///
/// 虚拟主机，即Http网关工厂
///
pub struct VirtualHost<S: Socket, H: Middleware<S, GatewayContext>> {
    router_tab: RouterTab<S, GatewayContext, H>, //路由器表
}

unsafe impl<S: Socket, H: Middleware<S, GatewayContext>> Send for VirtualHost<S, H> {}
unsafe impl<S: Socket, H: Middleware<S, GatewayContext>> Sync for VirtualHost<S, H> {}

impl<S: Socket, H: Middleware<S, GatewayContext>> Clone for VirtualHost<S, H> {
    fn clone(&self) -> Self {
        VirtualHost {
            router_tab: self.router_tab.clone(),
        }
    }
}

impl<S: Socket, H: Middleware<S, GatewayContext>> ServiceFactory<S> for VirtualHost<S, H> {
    type Service = HttpGateway<S, H>;

    fn new_service(&self) -> Self::Service {
        HttpGateway::with(self.router_tab.clone())
    }
}

impl<S: Socket, H: Middleware<S, GatewayContext>> VirtualHost<S, H> {
    /// 构建指定Http路由配置和Http请求处理器的Http网关工厂
    pub fn with(route: HttpRoute<S, GatewayContext, H>) -> Self {
        VirtualHost {
            router_tab: route.into(),
        }
    }
}