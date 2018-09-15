use std::fmt;
use std::sync::Arc;

use http::StatusCode;
use hyper::body::Body;
use npnc::ConsumeError;

use pi_lib::atom::Atom;
use pi_base::task::TaskType;

use request::Request;
use response::Response;

use {Error, HttpResponse};

/*
* http服务器结果
*/
pub type HttpsResult<T> = Result<T, HttpsError>;

/*
* http服务器错误
*/
#[derive(Debug)]
pub struct HttpsError {
    pub error: Box<Error + Send>
}

impl fmt::Display for HttpsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        fmt::Display::fmt(&*self.error, f)
    }
}

impl Error for HttpsError {
    fn description(&self) -> &str {
        self.error.description()
    }

    fn cause(&self) -> Option<&Error> {
        self.error.cause()
    }
}

impl HttpsError {
    //构建一个http服务器错误
    pub fn new<E: 'static + Error + Send>(e: E) -> HttpsError {
        HttpsError {
            error: Box::new(e)
        }
    }
}

/*
* http处理器
*/
pub trait Handler: Send + Sync + 'static {
    //处理方法，返回None表示异步处理，不会继续后处理，如果为Some则表示同步处理，并继续后处理
    fn handle(&self, req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)>;
}

impl<F> Handler for F
where
    F: Send + Sync + 'static + Fn(Request, Response) -> Option<(Request, Response, HttpsResult<()>)>,
{
    fn handle(&self, req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        (*self)(req, res)
    }
}

impl Handler for Box<Handler> {
    fn handle(&self, req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        (**self).handle(req, res)
    }
}

/*
* http前置中间件
*/
pub trait BeforeMiddleware: Send + Sync + 'static {
    //对成功请求进行同步预处理，可以返回Err，以表示在这个中间件内出现错误，让后续前置中间件或处理器处理
    fn before(&self, req: Request) -> (Request, HttpsResult<()>) {
        (req, Ok(()))
    }

    //对错误请求进行同步预处理，可以返回Ok，以表示在这个中间件内处理了错误，以保证后续前置中间件或处理器可以正常处理
    fn catch(&self, req: Request, err: HttpsError) -> (Request, HttpsResult<()>) {
        (req, Err(err))
    }
}

impl<F> BeforeMiddleware for F
where
    F: Send + Sync + 'static + Fn(Request) -> (Request, HttpsResult<()>),
{
    fn before(&self, req: Request) -> (Request, HttpsResult<()>) {
        (*self)(req)
    }
}

impl BeforeMiddleware for Box<BeforeMiddleware> {
    fn before(&self, req: Request) -> (Request, HttpsResult<()>) {
        (**self).before(req)
    }

    fn catch(&self, req: Request, err: HttpsError) -> (Request, HttpsResult<()>) {
        (**self).catch(req, err)
    }
}

impl<T> BeforeMiddleware for Arc<T>
where
    T: BeforeMiddleware,
{
    fn before(&self, req: Request) -> (Request, HttpsResult<()>) {
        (**self).before(req)
    }

    fn catch(&self, req: Request, err: HttpsError) -> (Request, HttpsResult<()>) {
        (**self).catch(req, err)
    }
}

/*
* http后置中间件
*/
pub trait AfterMiddleware: Send + Sync + 'static {
    //对成功的响应进行同步后处理，可以返回Err，以表示在这个中间件内出现错误，让后续后置中间件处理
    fn after(&self, req: Request, res: Response) -> (Request, Response, HttpsResult<()>) {
        (req, res, Ok(()))
    }

    //对错误的响应进行同步后处理，可以返回Ok，以表示在这个中间件内处理了错误，以保证后续后置中间件可以正常处理
    fn catch(&self, req: Request, res: Response, err: HttpsError) -> (Request, Response, HttpsResult<()>) {
        (req, res, Err(err))
    }
}

impl<F> AfterMiddleware for F
where
    F: Send + Sync + 'static + Fn(Request, Response) -> (Request, Response, HttpsResult<()>),
{
    fn after(&self, req: Request, res: Response) -> (Request, Response, HttpsResult<()>) {
        (*self)(req, res)
    }
}

impl AfterMiddleware for Box<AfterMiddleware> {
    fn after(&self, req: Request, res: Response) -> (Request, Response, HttpsResult<()>) {
        (**self).after(req, res)
    }

    fn catch(&self, req: Request, res: Response, err: HttpsError) -> (Request, Response, HttpsResult<()>) {
        (**self).catch(req, res, err)
    }
}

impl<T> AfterMiddleware for Arc<T>
where
    T: AfterMiddleware,
{
    fn after(&self, req: Request, res: Response) -> (Request, Response, HttpsResult<()>) {
        (**self).after(req, res)
    }

    fn catch(&self, req: Request, res: Response, err: HttpsError) -> (Request, Response, HttpsResult<()>) {
        (**self).catch(req, res, err)
    }
}

/*
* http替换中间件
*/
pub trait AroundMiddleware {
    //替换指定的处理器，返回替换前的处理器
    fn around(self, handler: Box<Handler>) -> Box<Handler>;
}

impl<F> AroundMiddleware for F
where
    F: FnOnce(Box<Handler>) -> Box<Handler>,
{
    fn around(self, handler: Box<Handler>) -> Box<Handler> {
        self(handler)
    }
}

/*
* http处理链
*/
pub struct Chain {
    befores: Vec<Box<BeforeMiddleware>>,    //预处理链
    afters: Vec<Box<AfterMiddleware>>,      //后处理链
    handler: Option<Box<Handler>>,          //处理器
}

impl Chain {
    //构建一个指定处理器的处理链
    pub fn new<H: Handler>(handler: H) -> Chain {
        Chain {
            befores: vec![],
            afters: vec![],
            handler: Some(Box::new(handler) as Box<Handler>),
        }
    }

    //将处理链转化为共享处理链
    pub fn to_share(chain: Chain) -> SharedChain {
        Arc::new(chain)
    }

    //在当前处理链中增加前置和后置中间件
    pub fn link<B, A>(&mut self, link: (B, A)) -> &mut Chain
    where
        A: AfterMiddleware,
        B: BeforeMiddleware,
    {
        let (before, after) = link;
        self.befores.push(Box::new(before) as Box<BeforeMiddleware>);
        self.afters.push(Box::new(after) as Box<AfterMiddleware>);
        self
    }

    //增加一个新的前置中间件
    pub fn link_before<B>(&mut self, before: B) -> &mut Chain
    where
        B: BeforeMiddleware,
    {
        self.befores.push(Box::new(before) as Box<BeforeMiddleware>);
        self
    }

    //增加一个新的后置中间件
    pub fn link_after<A>(&mut self, after: A) -> &mut Chain
    where
        A: AfterMiddleware,
    {
        self.afters.push(Box::new(after) as Box<AfterMiddleware>);
        self
    }

    //在当前处理链中执行指定的替换中间件
    pub fn link_around<A>(&mut self, around: A) -> &mut Chain
    where
        A: AroundMiddleware,
    {
        let mut handler = self.handler.take().unwrap();
        handler = around.around(handler);
        self.handler = Some(handler);
        self
    }
}

/*
* 共享http处理链
*/
type SharedChain = Arc<Chain>;

impl Handler for SharedChain {
    fn handle(&self, req: Request, res: Response) -> Option<(Request, Response, HttpsResult<()>)> {
        //开始预处理
        self.continue_from_before(req, res, 0);
        None
    }
}

/*
* 链式处理过程
*/
pub trait ChainProcess {
    //从预处理开始继续处理错误
    fn fail_from_before(&self, req: Request, res: Response, index: usize, err: HttpsError);
    //从处理器开始继续处理错误
    fn fail_from_handler(&self, req: Request, res: Response, err: HttpsError);
    //从后处理开始继续处理错误
    fn fail_from_after(&self, req: Request, res: Response, index: usize, err: HttpsError);
    //从预处理开始继续处理
    fn continue_from_before(&self, req: Request, res: Response, index: usize);
    //从处理器开始继续处理
    fn continue_from_handler(&self, req: Request, res: Response, is_before: bool);
    //从后处理开始继续处理
    fn continue_from_after(&self, req: Request, res: Response, index: usize);
    //返回结果
    fn reply(&self, req: Request, res: Response);
}

impl ChainProcess for SharedChain {
    fn fail_from_before(&self, mut req: Request, res: Response, index: usize, mut err: HttpsError) {
        if index >= self.befores.len() {
            //预处理完成，开始执行处理器
            self.fail_from_handler(req, res, err);
            return;
        }

        for (i, before) in self.befores[index..].iter().enumerate() {
            err = match before.catch(req, err) {
                (r, Err(err)) => {
                    req = r;
                    err
                },
                (r, Ok(_)) => {
                    self.continue_from_before(r, res, index + i + 1); //继续下一个预处理
                    return;
                },
            };
        }

        self.fail_from_handler(req, res, err); //错误预处理完成，开始执行处理器
    }

    fn fail_from_handler(&self, req: Request, res: Response, err: HttpsError) {
        //错误不会进入处理器，直接开始错误的异步后处理
        let executor = req.executor;
        let chain = self.clone();
        let func = Box::new(move || {
            chain.fail_from_after(req, res, 0, err);
        });
        executor(TaskType::Sync, 1000000, func, Atom::from("https service after task"));
    }

    fn fail_from_after(&self, mut req: Request, mut res: Response, index: usize, mut err: HttpsError) {
        if index == self.afters.len() {
            //后处理完成，返回错误
            self.reply(req, res);
            return;
        }

        for (i, after) in self.afters[index..].iter().enumerate() {
            err = match after.catch(req, res, err) {
                (r, q, Err(err)) => {
                    req = r;
                    res = q;
                    err
                },
                (r, q, Ok(_)) => {
                    self.continue_from_after(r, q, index + i + 1); //继续下一个后处理
                    return;
                },
            }
        }

        //错误后处理完成，返回错误
        self.reply(req, res);
    }

    fn continue_from_before(&self, mut req: Request, res: Response, index: usize) {
        if index >= self.befores.len() {
            //预处理完成，开始执行处理器
            self.continue_from_handler(req, res, false);
            return;
        }

        //按顺序执行预处理
        for (i, before) in self.befores[index..].iter().enumerate() {
            match before.before(req) {
                (r, Ok(_)) => req = r,   //更新请求对象
                (r, Err(err)) => {
                    self.fail_from_before(r, res, index + i + 1, err); //继续下一个错误预处理
                    return;
                },
            }
        }

        self.continue_from_handler(req, res, true); //预处理完成，开始执行处理器
    }

    fn continue_from_handler(&self, req: Request, res: Response, is_before: bool) {
        if is_before {
            //有预处理，则异步执行handler
            let executor = req.executor;
            let chain = self.clone();
            let func = Box::new(move || {
                match chain.handler.as_ref().unwrap().handle(req, res) {
                    None => (), //异步处理，并中断后处理
                    Some((r, q, reply)) => {
                        match reply {
                            Err(e) => chain.fail_from_handler(r, q, e), //同步处理错误，并开始错误处理
                            Ok(_) => chain.continue_from_after(r, q, 0), //同步处理成功，并开始后处理
                        }
                    },
                }
            });
            executor(TaskType::Sync, 1000000, func, Atom::from("https service handler task"));
        } else {
            //无预处理，则同步执行handler
            match self.handler.as_ref().unwrap().handle(req, res) {
                None => (), //异步处理，并中断后处理
                Some((r, q, reply)) => {
                    match reply {
                        Err(e) => self.fail_from_handler(r, q, e), //同步处理错误，并开始错误处理
                        Ok(_) => self.continue_from_after(r, q, 0), //同步处理成功，并开始后处理
                    }
                },
            }
        }
    }

    fn continue_from_after(&self, mut req: Request, mut res: Response, index: usize) {
        if index >= self.afters.len() {
            //无后处理，则立即返回结果
            self.reply(req, res);
        } else {
            //有后处理，则开始异步后处理
            let executor = req.executor;
            let chain = self.clone();
            let func = Box::new(move || {
                //按顺序执行后处理
                for (i, after) in chain.afters[index..].iter().enumerate() {
                    res = match after.after(req, res) {
                        (r, q, Ok(_)) => {
                            req = r;
                            q
                        },
                        (r, q, Err(err)) => {
                            chain.fail_from_after(r, q, index + i + 1, err); //继续下一个错误后处理
                            return;
                        },
                    }
                }

                //后处理完成，返回结果
                chain.reply(req, res);
            });
            executor(TaskType::Sync, 1000000, func, Atom::from("https service after task"));
        }
    }

    fn reply(&self, req: Request, res: Response) {
        let executor = req.executor;
        let chain = self.clone();
        let func = Box::new(move || {
            match res.receiver.as_ref().unwrap().consume() {
                Err(ConsumeError::Disconnected) => println!("https service reply task wakeup failed, task id: {}", req.uid),
                Err(ConsumeError::Empty) => {
                    //外部执行器未准备好，则继续等待
                    chain.reply(req, res);
                }
                Ok(waker) => {
                    //外部执行器已准备好
                    let mut http_res = HttpResponse::<Body>::new(Body::empty());
                    *http_res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    let sender = res.sender.clone();
                    res.write_back(&mut http_res);
                    sender.unwrap().produce(Ok(http_res)).is_ok();
                    waker.notify();
                }
            }
        });
        executor(TaskType::Sync, 1000000, func, Atom::from("https service reply task"));
    }
}
