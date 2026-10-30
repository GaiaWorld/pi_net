//! 绑定（binding）公开操作的状态（state）、线程（thread）和归还顺序验证。
//! 直接消费SocketContextBinding；仅保留分支使用有截止的子进程（subprocess），
//! 不创建网络（network）或运行时（runtime），不代表连接侧装配已经完成。

use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Barrier, atomic::{AtomicUsize, Ordering}};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use super::{SocketContextBinding, SocketContextGetError};

struct Counted {
    count: Arc<AtomicUsize>,
    owner: ThreadId,
}

impl Drop for Counted {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.owner);
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

/// BIND-U01：准备前取得/解除为空；首次准备后同线程（thread）可取得空容器（container）。
#[test]
fn test_unprepared_and_prepared() {
    let binding = SocketContextBinding::new();
    assert!(matches!(binding.get_context(), Err(SocketContextGetError::Unavailable)));
    assert!(matches!(binding.detach_context(), Err(SocketContextGetError::Unavailable)));
    binding.prepare_context().unwrap();
    assert!(binding.get_context().unwrap().is_empty());
    drop(binding.detach_context().unwrap().unwrap());
}

/// BIND-U02：克隆（clone）及重复准备共享容器（container）；业务许可（access permit）不影响取得。
#[test]
fn test_shared_preparation_and_busy_data() {
    let binding = SocketContextBinding::new();
    let other = binding.clone();
    binding.prepare_context().unwrap();
    let context = binding.get_context().unwrap();
    assert!(context.set(17_u32));
    let handle = context.get::<u32>().unwrap();
    let mut view = handle.try_write().unwrap();
    *view.as_mut() = 19;
    other.prepare_context().unwrap();
    let cached = other.get_context().unwrap();
    assert!(Rc::ptr_eq(&context, &cached));
    drop(view);
    assert_eq!(*handle.try_read().unwrap().as_ref(), 19);
    drop(other.detach_context().unwrap().unwrap());
    assert!(matches!(binding.get_context(), Err(SocketContextGetError::Unavailable)));
}

/// BIND-U03：解除为单向状态（state）变化，缓存（cache）及普通句柄（context handle）继续真实持有。
#[test]
fn test_detached_cache_lifetime() {
    let count = Arc::new(AtomicUsize::new(0));
    let binding = SocketContextBinding::new();
    binding.prepare_context().unwrap();
    let cached = binding.get_context().unwrap();
    assert!(cached.set(Counted { count: count.clone(), owner: thread::current().id() }));
    let handle = cached.get::<Counted>().unwrap();
    let detached = binding.detach_context().unwrap().unwrap();
    assert!(Rc::ptr_eq(&cached, &detached));
    assert!(binding.detach_context().unwrap().is_none());
    assert!(matches!(binding.prepare_context(), Err(SocketContextGetError::Unavailable)));
    assert!(matches!(binding.get_context(), Err(SocketContextGetError::Unavailable)));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(detached);
    assert!(handle.try_read().is_ok());
    drop(cached);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(handle.try_write().is_ok());
    drop(handle);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// BIND-U04：已发布身份（identity）后先核线程（thread），解除后仍保持同一结果。
#[test]
fn test_other_thread_before_and_after_detach() {
    let binding = SocketContextBinding::new();
    let unprepared = binding.clone();
    thread::spawn(move || {
        assert!(matches!(unprepared.get_context(), Err(SocketContextGetError::Unavailable)));
    }).join().unwrap();
    binding.prepare_context().unwrap();
    for detached in [false, true] {
        if detached {
            drop(binding.detach_context().unwrap().unwrap());
        }
        let other = binding.clone();
        thread::spawn(move || {
            assert_eq!(other.prepare_context(), Err(SocketContextGetError::WrongThread));
            assert!(matches!(other.get_context(), Err(SocketContextGetError::WrongThread)));
            assert!(matches!(other.detach_context(), Err(SocketContextGetError::WrongThread)));
        }).join().unwrap();
    }
}

/// BIND-U05：构造线程（thread）不等于所属线程（owner thread）；首次准备者决定身份（identity）。
#[test]
fn test_prepare_on_selected_thread() {
    let binding = SocketContextBinding::new();
    let selected = binding.clone();
    thread::spawn(move || {
        selected.prepare_context().unwrap();
        assert!(selected.get_context().unwrap().set(23_u32));
        let local = selected.detach_context().unwrap().unwrap();
        assert_eq!(local.remove::<u32>().unwrap(), Some(23));
        drop(local);
    }).join().unwrap();
    assert_eq!(binding.prepare_context(), Err(SocketContextGetError::WrongThread));
    assert!(matches!(binding.get_context(), Err(SocketContextGetError::WrongThread)));
}

/// BIND-U06：两个线程（thread）同时首次准备，只有一个成为所属线程（owner thread）。
#[test]
fn test_competing_preparation() {
    let binding = SocketContextBinding::new();
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2).map(|_| {
        let local_binding = binding.clone();
        let local_barrier = barrier.clone();
        thread::spawn(move || {
            local_barrier.wait();
            let result = local_binding.prepare_context();
            if result.is_ok() {
                assert!(local_binding.get_context().unwrap().is_empty());
                drop(local_binding.detach_context().unwrap().unwrap());
                assert!(local_binding.detach_context().unwrap().is_none());
            }
            result
        })
    }).collect();
    barrier.wait();
    let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| **result == Err(SocketContextGetError::WrongThread)).count(), 1);
}

struct Observed {
    binding: SocketContextBinding,
    count: Arc<AtomicUsize>,
}

impl Drop for Observed {
    fn drop(&mut self) {
        assert!(matches!(self.binding.get_context(), Err(SocketContextGetError::Unavailable)));
        assert!(self.binding.detach_context().unwrap().is_none());
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

/// BIND-U07：返回的持有在方法（method）返回后归还，业务析构（Drop）可再次观察空绑定（binding）。
#[test]
fn test_value_returned_before_user_drop() {
    let count = Arc::new(AtomicUsize::new(0));
    let binding = SocketContextBinding::new();
    binding.prepare_context().unwrap();
    assert!(binding.get_context().unwrap().set(Observed { binding: binding.clone(), count: count.clone() }));
    let detached = binding.detach_context().unwrap().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(detached);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// BIND-U08/U09子入口：最后控制持有归还后，未解除的业务值仍保留；数据不参与跨线程（thread）传递。
#[test]
#[ignore]
fn test_binding_retention_child() {
    let mode = std::env::var("PI_NET_BINDING_MODE").unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let binding = SocketContextBinding::new();
    binding.prepare_context().unwrap();
    assert!(binding.get_context().unwrap().set(Counted { count: count.clone(), owner: thread::current().id() }));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    match mode.as_str() {
        "owner" => drop(binding),
        "other" => thread::spawn(move || drop(binding)).join().unwrap(),
        _ => unreachable!(),
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

/// 子进程（subprocess）只运行一个明确入口，20秒内结束；到期结束并等待归还进程（process）资源。
fn run_retention_child(mode: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "context_binding_checks::test_binding_retention_child", "--ignored", "--nocapture"])
        .env("PI_NET_BINDING_MODE", mode)
        .spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "binding child mode={mode} status={status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("binding child exceeded deadline: {mode}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// BIND-U08：所属线程（owner thread）最后归还控制持有，沿用未解除时保留的约定。
#[test]
fn test_attached_last_release_on_owner() {
    run_retention_child("owner");
}

/// BIND-U09：其它线程（thread）最后归还控制持有，业务值不在该线程（thread）归还。
#[test]
fn test_attached_last_release_on_other() {
    run_retention_child("other");
}
