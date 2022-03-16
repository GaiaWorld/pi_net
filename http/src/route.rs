use std::sync::Arc;
use std::marker::PhantomData;
use std::collections::hash_map::Entry;
use std::io::{Error, Result, ErrorKind};

use https::Method;
use regex::{RegexSetBuilder, RegexSet, RegexBuilder, Regex};

use pi_hash::XHashMap;
use pi_atom::Atom;
use pi_gray::GrayVersion;
use pi_handler::{Args, Handler, SGenType};
use tcp::driver::{Socket, AsyncIOWait};

use crate::{service::HttpService,
            middleware::Middleware,
            request::HttpRequest,
            response::HttpResponse};

/*
* 需要被替换的字符
*/
const DOT_CHAR: &str = r".";
const SINGLE_STAR_CHAR: &str = r"*";
const DOUBLE_STAR_CHAR: &str = r"**";

/*
* 替换的字符
*/
const REPLACED_DOT: &str = r"\.";
const REPLACED_SINGLE_STAR: &str = r"([\w \.-])+";
const REPLACED_DOUBLE_STAR: &str = r"/?([\w \.-]/?)+";

/*
* 通配符路由表
*/
struct WildcardRouter<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> {
    matchor:    Option<RegexSet>,               //匹配器
    route:      Arc<Vec<Atom>>,                 //路由表
    handlers:   Arc<Vec<Arc<Handler>>>,         //处理器列表
    marker:     PhantomData<(S, W, Context)>,
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> Clone for WildcardRouter<S, W, Context, Handler> {
    fn clone(&self) -> Self {
        WildcardRouter {
            matchor: self.matchor.clone(), //实际复制了一个正则引擎的共享指针，并构建了一个匹配缓存
            route: self.route.clone(),
            handlers: self.handlers.clone(),
            marker: PhantomData,
        }
    }
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> WildcardRouter<S, W, Context, Handler> {
    //构建通配符路由表
    pub fn new() -> Self {
        WildcardRouter {
            matchor: None,
            route: Arc::new(Vec::new()),
            handlers: Arc::new(Vec::new()),
            marker: PhantomData,
        }
    }

    //构建指定初始容量的通配符路由表
    pub fn with_capacity(capacity: usize) -> Self {
        WildcardRouter {
            matchor: None,
            route: Arc::new(Vec::with_capacity(capacity)),
            handlers: Arc::new(Vec::with_capacity(capacity)),
            marker: PhantomData,
        }
    }

    //获取路由表长度
    pub fn len(&self) -> usize {
        self.route.len()
    }

    //增加路由条目
    pub fn add(&mut self, route: Atom, handler: Handler) {
        if let Some(vec) = Arc::get_mut(&mut self.route) {
            vec.push(route);
        } else {
            panic!("add wildcard route error, get mut ref failed");
        }

        if let Some(vec) = Arc::get_mut(&mut self.handlers) {
            vec.push(Arc::new(handler));
        } else {
            panic!("add wildcard route handler error, get mut ref failed");
        }
    }

    //完成路由表，完成以后才允许复制路由表
    pub fn finish(&mut self) -> Result<()> {
        let route: Vec<&str> = self.route.iter().map(|r| {
            r.as_ref()
        }).collect();

        match RegexSetBuilder::new(route.as_slice()).build() {
            Err(e) => {
                Err(Error::new(ErrorKind::Other, format!("finish wildcard router failed, reason: {:?}", e)))
            },
            Ok(set) => {
                //构建指定路由的匹配器成功
                self.matchor = Some(set);
                Ok(())
            }
        }
    }

    //判断是否匹配
    pub fn is_match(&self, path: &str) -> bool {
        if let Some(matchor) = &self.matchor {
            return matchor.is_match(path);
        }

        false
    }

    //匹配路由表，匹配成功返回处理器
    pub fn match_route(&mut self, path: &str) -> Option<Arc<Handler>> {
        if let Some(matchor) = &mut self.matchor {
            let indexes: Vec<usize> = matchor.matches(path).into_iter().collect();
            let len = indexes.len();
            if len > 0 {
                //已匹配，则获取所有匹配项中的最后增加的项
                return Some(self.handlers[indexes[len - 1]].clone());
            }
        }

        None
    }
}

/*
* Http路由器
*/
struct Router<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> {
    fixed:              Arc<XHashMap<Atom, Arc<Handler>>>,          //确定路由表
    single_wildcard:    WildcardRouter<S, W, Context, Handler>,     //单级通配符路由表
    mutil_wildcard:     WildcardRouter<S, W, Context, Handler>,     //多级通配符路由表
    filter:             Arc<Regex>,                                 //路由过滤器
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> Clone for Router<S, W, Context, Handler> {
    fn clone(&self) -> Self {
        Router {
            fixed: self.fixed.clone(),
            single_wildcard: self.single_wildcard.clone(),
            mutil_wildcard: self.mutil_wildcard.clone(),
            filter: self.filter.clone(),
        }
    }
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> Router<S, W, Context, Handler> {
    //构建Http路由器
    pub fn new() -> Self {
        Router {
            fixed: Arc::new(XHashMap::default()),
            single_wildcard: WildcardRouter::new(),
            mutil_wildcard: WildcardRouter::new(),
            filter: Arc::new(RegexBuilder::new(r"^([^\*])+[\.\*]$").build().ok().unwrap()),
        }
    }

    //构建指定路由的Http路由器
    pub fn with(route: String, handler: Handler) -> Self {
        let mut router = Self::new();
        router.add(route, handler);
        router
    }

    //获取路由表长度
    pub fn len(&self) -> usize {
        self.fixed.len() + self.single_wildcard.len() + self.mutil_wildcard.len()
    }

    //增加路由条目
    pub fn add(&mut self, mut route: String, handler: Handler) {
        if self.filter.is_match(&route) {
            //优化形如/.../xxx.*的路由
            route = route.replace(".*", "");
        }

        if route.contains(DOUBLE_STAR_CHAR) {
            //路由中包含**，则加入多级通配符路由表
            let atom = Atom::from(route.replace(DOT_CHAR, REPLACED_DOT)
                .replace(DOUBLE_STAR_CHAR, REPLACED_DOUBLE_STAR)
                .replace(SINGLE_STAR_CHAR, REPLACED_SINGLE_STAR));
            self.mutil_wildcard.add(atom, handler);
        } else if  route.contains(SINGLE_STAR_CHAR) {
            //路由中只包含*，则加入单级通配符路由表
            let atom = Atom::from(route.replace(DOT_CHAR, REPLACED_DOT)
                .replace(SINGLE_STAR_CHAR, REPLACED_SINGLE_STAR));
            self.single_wildcard.add(atom, handler);
        } else {
            //加入确定路由表
            if let Some(map) = Arc::get_mut(&mut self.fixed) {
                let atom = Atom::from(route);
                map.insert(atom.clone(), Arc::new(handler));
            } else {
                panic!("add route error, get mut ref failed");
            }
        }

    }

    //完成路由表，完成以后才允许复制路由表
    pub fn finish(&mut self) -> Result<()> {
        if let Err(e) = self.single_wildcard.finish() {
            return Err(e);
        }

        if let Err(e) = self.mutil_wildcard.finish() {
            return Err(e);
        }

        Ok(())
    }

    //判断是否匹配，优先判断确定路由表，再判断单级通配符路由表，最后判断多级通配符路由表
    pub fn is_match(&self, path: &str) -> bool {
        if !self.fixed.contains_key(&Atom::from(path)) {
            if !self.single_wildcard.is_match(path) {
                return self.mutil_wildcard.is_match(path);
            }
        }

        true
    }

    //匹配路由表，优先判断确定路由表，再判断单级通配符路由表，最后判断多级通配符路由表，匹配成功返回处理器
    pub fn match_route(&mut self, path: &str) -> Option<Arc<Handler>> {
        if let Some(handler) = self.fixed.get(&Atom::from(path)) {
            return Some(handler.clone());
        }

        if let Some(handler) = self.single_wildcard.match_route(path) {
            return Some(handler);
        }

        if let Some(handler) = self.mutil_wildcard.match_route(path) {
            return Some(handler);
        }

        None
    }
}

/*
* Http路由器表
*/
pub struct RouterTab<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> {
    map:    XHashMap<Method, Router<S, W, Context, Handler>>,  //路由器方法表
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> Clone for RouterTab<S, W, Context, Handler> {
    fn clone(&self) -> Self {
        RouterTab {
            map: self.map.clone(),
        }
    }
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> RouterTab<S, W, Context, Handler> {
    //构建Http路由器表
    pub fn new() -> Self {
        RouterTab {
            map: XHashMap::default(),
        }
    }

    //获取所有方法的路由表长度
    pub fn len(&self) -> usize {
        self.map.iter().map(|(_, router)| {
            router.len()
        }).collect::<Vec<usize>>().iter().sum()
    }

    //增加指定方法的路由
    pub fn add(&mut self, route: String, method: Method, handler: Handler) {
        match self.map.entry(method) {
            Entry::Vacant(v) => {
                //增加指定方法的路由器，并增加新的路由
                v.insert(Router::with(route, handler));
            },
            Entry::Occupied(mut o) => {
                //在指定方法的路由器中增加新的路由
                o.get_mut().add(route, handler);
            },
        }
    }

    //完成路由器表，完成以后才允许复制路由表
    pub fn finish(&mut self) -> Result<()> {
        for router in self.map.values_mut() {
            if let Err(e) = router.finish() {
                return Err(e);
            }
        }

        Ok(())
    }

    //判断是否匹配
    pub fn is_match(&self, method: &Method, path: &str) -> bool {
        if let Some(router) = self.map.get(method) {
            return router.is_match(path);
        }

        false
    }

    //匹配路由表
    pub fn match_route(&mut self, method: &Method, path: &str) -> Option<Arc<Handler>> {
        if let Some(router) = self.map.get_mut(method) {
            return router.match_route(path);
        }

        None
    }
}

/*
* Http路由配置
*/
pub struct HttpRoute<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> {
    tab:    RouterTab<S, W, Context, Handler>,  //路由器
    path:   String,                             //路径
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> From<HttpRoute<S, W, Context, Handler>> for RouterTab<S, W, Context, Handler> {
    fn from(route: HttpRoute<S, W, Context, Handler>) -> Self {
        let mut tab = route.tab;
        tab.finish();
        tab
    }
}

impl<S: Socket, W: AsyncIOWait, Context: Send + Sync + 'static, Handler: Middleware<S, W, Context>> HttpRoute<S, W, Context, Handler> {
    //构建Http路由配置
    pub fn new() -> Self {
        HttpRoute {
            tab: RouterTab::new(),
            path: String::default(),
        }
    }

    //设置指定的Http路由路径
    pub fn at(&mut self, path: &str) -> &mut Self {
        self.path = path.to_string();
        self
    }

    //为指定的路由设置指定方法的处理器
    pub fn method(&mut self, method: Method, handler: Handler) -> &mut Self {
        self.tab.add(self.path.clone(), method, handler);
        self
    }

    //为指定的路由设置options方法的处理器
    pub fn options(&mut self, handler: Handler) -> &mut Self {
        self.method(Method::OPTIONS, handler);
        self
    }

    //为指定的路由设置head方法的处理器
    pub fn head(&mut self, handler: Handler) -> &mut Self {
        self.method(Method::HEAD, handler);
        self
    }

    //为指定的路由设置get方法的处理器
    pub fn get(&mut self, handler: Handler) -> &mut Self {
        self.method(Method::GET, handler);
        self
    }

    //为指定的路由设置post方法的处理器
    pub fn post(&mut self, handler: Handler) -> &mut Self {
        self.method(Method::POST, handler);
        self
    }
}