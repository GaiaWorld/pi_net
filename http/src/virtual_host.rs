use std::sync::Arc;
use std::io::{Error, Result, ErrorKind};

use hash::XHashMap;
use atom::Atom;
use handler::{Args, Handler, SGenType};
use tcp::driver::{Socket, AsyncIOWait};

use crate::{service::{HttpService, ServiceFactory},
            gateway::{GatewayContext, HttpGateway},
            route::{RouterTab, HttpRoute},
            middleware::Middleware};

/*
* 虚拟主机池
*/
pub trait VirtualHostPool<S: Socket, W: AsyncIOWait>: Clone + Send + Sync + 'static {
    type Host: ServiceFactory<S, W>;

    //获取虚拟主机数量
    fn size(&self) -> usize;

    //获取指定主机名的虚拟主机
    fn get(&self, name: &str) -> Option<&Self::Host>;

    //增加指定主机名的虚拟主机
    fn add(&mut self, name: &str, host: Self::Host) -> Result<()>;
}

/*
* 虚拟主机表
*/
pub struct VirtualHostTab<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>>(Arc<XHashMap<Atom, VirtualHost<S, W, H>>>);

unsafe impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> Send for VirtualHostTab<S, W, H> {}
unsafe impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> Sync for VirtualHostTab<S, W, H> {}

impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> Clone for VirtualHostTab<S, W, H> {
    fn clone(&self) -> Self {
        VirtualHostTab(self.0.clone())
    }
}

impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> VirtualHostPool<S, W> for VirtualHostTab<S, W, H> {
    type Host = VirtualHost<S, W, H>;

    fn size(&self) -> usize {
        self.0.len()
    }

    fn get(&self, name: &str) -> Option<&Self::Host> {
        self.0.get(&Atom::from(name))
    }

    fn add(&mut self, name: &str, host: Self::Host) -> Result<()> {
        if let Some(tab) = Arc::get_mut(&mut self.0) {
            tab.insert(Atom::from(name), host);
            return Ok(());
        }

        Err(Error::new(ErrorKind::Other, format!("add virtual host error, host: {:?}, reason: not writable", name)))
    }
}

impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> VirtualHostTab<S, W, H> {
    //构建虚拟主机表
    pub fn new() -> Self {
        VirtualHostTab(Arc::new(XHashMap::default()))
    }
}

/*
* 虚拟主机，即Http网关工厂
*/
pub struct VirtualHost<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> {
    router_tab: RouterTab<S, W, GatewayContext, H>, //路由器表
}

unsafe impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> Send for VirtualHost<S, W, H> {}
unsafe impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> Sync for VirtualHost<S, W, H> {}

impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> Clone for VirtualHost<S, W, H> {
    fn clone(&self) -> Self {
        VirtualHost {
            router_tab: self.router_tab.clone(),
        }
    }
}

impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> ServiceFactory<S, W> for VirtualHost<S, W, H> {
    type Service = HttpGateway<S, W, H>;

    fn new_service(&self) -> Self::Service {
        HttpGateway::with(self.router_tab.clone())
    }
}

impl<S: Socket, W: AsyncIOWait, H: Middleware<S, W, GatewayContext>> VirtualHost<S, W, H> {
    //构建指定Http路由配置和Http请求处理器的Http网关工厂
    pub fn with(route: HttpRoute<S, W, GatewayContext, H>) -> Self {
        VirtualHost {
            router_tab: route.into(),
        }
    }
}