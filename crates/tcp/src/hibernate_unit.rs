//! Hibernate 私有协议的局部分支测试，不冒充真实运行时整合证明。
//!
//! 使用真实成对 TCP 和已注册 mio Poll 构造最小对象，不启动 owner 或伪造网络回包。
//! 分支状态只在单元内部布置；外部可达性由 hibernate_regression 的真实专项另证。
//! 每项在独立进程组运行，八秒截止并清理，避免错误实现使测试永久等锁。

use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::io::{Error, ErrorKind};
use std::net::{TcpListener, TcpStream};
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Weak, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::task::{Context, Wake, Waker};
use std::time::Duration;
use crossbeam_channel::bounded;
use crate::{SocketHandle, SocketImage, Stream};
use crate::connect::TcpSocket;
use super::super::{Hibernate, InnerHibernate, Ready, TlsConfig};

const CHILD_ENV: &str = "PI_TCP_HIB_UNIT_CHILD";

/// 单线程组件夹具；Poll 与接收端只在本线程使用，保留客户端直至断言完成。
/// 不给 Socket 设置自持有 handle，不添加运行时、事件补发或后台资源清理。
struct Fixture {
    handle: SocketHandle<TcpSocket>,
    _client: TcpStream,
}

impl Fixture {
    fn new(registered: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(2)).unwrap();
        let (server, remote) = listener.accept().unwrap();
        let local = server.local_addr().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut stream = mio::net::TcpStream::from_std(server);
        let poll = mio::Poll::new().unwrap();
        poll.registry().register(&mut stream, mio::Token(1), mio::Interest::READABLE).unwrap();
        if !registered { poll.registry().deregister(&mut stream).unwrap(); }
        let mut socket = TcpSocket::new(&local, &remote, Some(mio::Token(1)), stream,
            16, 4096, 4096, TlsConfig::Empty);
        socket.set_poll(Rc::new(UnsafeCell::new(poll)));
        let context = socket.get_context_binding();
        let shared = Arc::new(UnsafeCell::new(socket));
        let (close, _) = bounded(4);
        let (timer, _) = bounded(4);
        let image = SocketImage::new(&shared, local, remote, mio::Token(1), 1, false,
            Arc::new(AtomicBool::new(false)), close, timer, context);
        Self { handle: SocketHandle::new(image), _client: client }
    }

    fn sleep(&self) -> Hibernate<TcpSocket> { Hibernate::new(self.handle.clone(), Ready::ReadWrite) }
}

/// 只检查所属 Hibernate 的 W 能重新取得，不访问 Socket 或反向调用业务层。
/// Weak 避免测试唤醒器与 Hibernate 构成引用环；计数只用于断言真实调用发生。
struct Observe {
    target: Weak<InnerHibernate<TcpSocket>>,
    wakes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Observe {
    fn waker(sleep: &Hibernate<TcpSocket>, wakes: &Arc<AtomicUsize>, drops: &Arc<AtomicUsize>) -> Waker {
        Waker::from(Arc::new(Self { target: Arc::downgrade(&sleep.0),
            wakes: wakes.clone(), drops: drops.clone() }))
    }

    fn check_unlocked(&self) {
        if let Some(target) = self.target.upgrade() { let _guard = target.waker.lock(); }
    }
}

impl Wake for Observe {
    fn wake(self: Arc<Self>) {
        self.check_unlocked();
        self.wakes.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for Observe {
    fn drop(&mut self) {
        self.check_unlocked();
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// 错误载荷析构也可执行代码；验证早退不会持 W 销毁未采用的错误值。
struct ObserveError(Observe);

impl fmt::Debug for ObserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("单元错误载荷").field(&self.0.target.strong_count()).finish()
    }
}

impl fmt::Display for ObserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("单元错误载荷") }
}

impl std::error::Error for ObserveError {}

/// 分别触达身份比较、条件失败、原状态分支、通知及析构边界，均为局部单元。
fn component_case(case: &str) {
    let fixture = Fixture::new(case != "reregister_error");
    let mut sleep = fixture.sleep();
    let wakes = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    match case {
        "identity" => {
            assert!(sleep.is_same(&sleep.clone()));
            assert!(!sleep.is_same(&fixture.sleep()), "同连接及UID不等于同休眠对象");
        },
        "detach" => {
            *sleep.0.waker.lock() = Some(Observe::waker(&sleep, &wakes, &drops));
            assert!(!sleep.wakeup(Ok(()), |_| false));
            assert!(sleep.0.waker.lock().is_some());
            assert!(sleep.0.result.0.borrow().is_none());
            assert_eq!(wakes.load(Ordering::Relaxed), 0);
            assert!(sleep.wakeup(Err(Error::from(ErrorKind::PermissionDenied)), |_| true));
            assert_eq!(sleep.0.result.0.borrow_mut().take().unwrap().unwrap_err().kind(), ErrorKind::PermissionDenied);
            assert_eq!(wakes.load(Ordering::Relaxed), 1);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        },
        "status" => {
            *sleep.0.waker.lock() = Some(Observe::waker(&sleep, &wakes, &drops));
            sleep.0.waker_status.store(1, Ordering::Relaxed);
            assert!(!sleep.wakeup(Ok(()), |_| true));
            assert_eq!(sleep.0.waker_status.load(Ordering::Relaxed), 0);
            assert!(sleep.0.result.0.borrow().is_none());
            assert_eq!(wakes.load(Ordering::Relaxed), 1);
            assert!(!sleep.wakeup(Ok(()), |_| true), "没有waker或结果时不能报完成");
            *sleep.0.result.0.borrow_mut() = Some(Ok(()));
            assert!(sleep.wakeup(Ok(()), |_| true), "保留已有结果分支");
        },
        "retire" => {
            // 刻意布置替换分支，只证明旧waker析构锁外，不声称该状态来自网络流程。
            *sleep.0.waker.lock() = Some(Observe::waker(&sleep, &wakes, &drops));
            let incoming = Observe::waker(&sleep, &wakes, &Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&incoming);
            assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert!(fixture.handle.wakeup(Ok(())));
            assert!(Pin::new(&mut sleep).poll(&mut cx).is_ready());
        },
        "error_drop" => {
            let error = Error::new(ErrorKind::Other, ObserveError(Observe {
                target: Arc::downgrade(&sleep.0), wakes: wakes.clone(), drops: drops.clone(),
            }));
            assert!(!sleep.wakeup(Err(error), |_| false));
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        },
        "reregister_error" => {
            // 真实epoll中先注销，再通过原poll触发MOD失败，不用伪造错误返回。
            *sleep.0.result.0.borrow_mut() = Some(Err(Error::new(ErrorKind::Other, ObserveError(Observe {
                target: Arc::downgrade(&sleep.0), wakes: wakes.clone(), drops: drops.clone(),
            }))));
            let incoming = Observe::waker(&sleep, &wakes, &Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&incoming);
            let result = Pin::new(&mut sleep).poll(&mut cx);
            assert!(matches!(result, std::task::Poll::Ready(Err(ref error)) if error.kind() == ErrorKind::NotFound));
            assert_eq!(drops.load(Ordering::Relaxed), 1, "未采用的原结果须在释放W后析构");
        },
        _ => panic!("未知单元场景"),
    }
}

/// 同一二进制的严格有界单元执行；组件等待锁超过截止就是失败，不补成功状态。
fn run_case(case: &str, test: &str) {
    if std::env::var(CHILD_ENV).ok().as_deref() == Some(case) {
        component_case(case);
        return;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([test, "--exact", "--nocapture", "--test-threads=1"]).env(CHILD_ENV, case);
    let result = super::process::run_child(&mut command, Duration::from_secs(8), &format!("unit-{case}"));
    assert!(!result.timed_out && result.status.success(), "局部边界未完成，所属组已清理\n{}\n{}",
        result.stdout, result.stderr);
}

/// 身份必须来自Arc核心而不是连接UID；保护Hibernate::is_same。
#[test]
fn test_hibernate_identity() {
    run_case("identity", "utils::hibernate_regression::unit::test_hibernate_identity");
}

/// 失败条件不消耗waker或结果，成功结果保留错误种类；通知及析构可再次取得W。
#[test]
fn test_hibernate_conditional_detach() {
    run_case("detach", "utils::hibernate_regression::unit::test_hibernate_conditional_detach");
}

/// 保留阻挡状态清除/返回false、无waker无结果返回false及已有结果返回true分支。
#[test]
fn test_hibernate_wakeup_status() {
    run_case("status", "utils::hibernate_regression::unit::test_hibernate_wakeup_status");
}

/// poll替换旧waker时必须锁外析构；随后真实mio登记对象仍可完成一次唤醒。
#[test]
fn test_hibernate_poll_retired_waker() {
    run_case("retire", "utils::hibernate_regression::unit::test_hibernate_poll_retired_waker");
}

/// 条件失败时错误载荷析构不持W，防止只搬显式wake而遗漏参数销毁。
#[test]
fn test_hibernate_unused_error_drop() {
    run_case("error_drop", "utils::hibernate_regression::unit::test_hibernate_unused_error_drop");
}

/// poll事件重新登记真实失败时返回该I/O错误，原存储错误载荷析构不占用W。
#[test]
fn test_hibernate_reregister_error_drop() {
    run_case("reregister_error", "utils::hibernate_regression::unit::test_hibernate_reregister_error_drop");
}
