//! 上下文（Context）存储的定向契约测试（contract test），对应CTX-U01～U17。
//!
//! 仅驱动utils中的生产接口（production API）并观察值、错误及析构（Drop）；
//! 不启动网络（network）、运行时（runtime）或线程（thread），不补生产能力。
//! CTX-C01～C05的编译约束（compile-time constraint）由对应公开文档（rustdoc）示例验证。

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::{Rc, Weak};

use super::{ContextAccessError, ContextRemoveError, SocketContext};

/// 只记录所属线程（owner thread）的真实析构（Drop）次数，不依赖全局状态（global state）。
struct DropCount {
    drops: Rc<Cell<usize>>,
}

impl Drop for DropCount {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

/// CTX-U11/U12：唯一容器（container）持有结束时，业务值恰好析构（Drop）一次。
/// 红灯（red test）仅使用原有empty/set，不调用旧get/remove或解引用原始指针（raw pointer）。
#[test]
fn test_container_drop_releases_unshared_value() {
    let drops = Rc::new(Cell::new(0));
    {
        let mut context = SocketContext::empty();
        assert!(SocketContext::set(&mut context, DropCount { drops: drops.clone() }));
        assert_eq!(drops.get(), 0);
    }
    assert_eq!(drops.get(), 1, "the last owner must drop the installed value exactly once");
}

/// 核对真实展开（unwind）的诊断文字，避免用其它测试错误冒充目标分支。
fn assert_panic_message(expected: &str, action: impl FnOnce()) {
    let panic = catch_unwind(AssertUnwindSafe(action)).expect_err("the operation must panic");
    let message = panic.downcast_ref::<&str>().copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .expect("the panic must contain a string");
    assert!(message.contains(expected), "unexpected panic: {message}");
}

/// CTX-U01：空槽（slot）的观察及重复移出准确，不因泛型（generic type）不同而改变状态（state）。
#[test]
fn test_empty_and_repeated_empty_removal() {
    let context = SocketContext::empty();
    assert!(context.is_empty());
    assert!(context.get::<u8>().is_none());
    assert_eq!(context.remove::<u8>(), Ok(None));
    assert_eq!(context.remove::<String>(), Ok(None));
    assert_eq!(context.try_remove::<u8>(), Ok(None));
    assert_eq!(context.try_remove::<String>(), Ok(None));
    assert!(context.is_empty());
}

/// CTX-U02：失败输入恰好析构（Drop）一次，原值不覆盖且仍可经真实句柄（context handle）访问。
#[test]
fn test_set_rejects_without_overwrite() {
    let kept = Rc::new(Cell::new(0));
    let rejected = Rc::new(Cell::new(0));
    let context = SocketContext::empty();
    assert!(context.set(DropCount { drops: kept.clone() }));
    assert!(!context.set(DropCount { drops: rejected.clone() }));
    assert_eq!(rejected.get(), 1);
    assert_eq!(kept.get(), 0);
    let handle = context.get::<DropCount>().unwrap();
    {
        let read = handle.try_read().unwrap();
        assert!(Rc::ptr_eq(&read.as_ref().drops, &kept));
    }
    drop(handle);
    drop(context);
    assert_eq!(kept.get(), 1);
    assert_eq!(rejected.get(), 1);
}

/// CTX-U03：多个真实句柄（context handle）允许单份写许可（write permit），提前归还不析构（Drop）T。
#[test]
fn test_multiple_handles_allow_one_writer() {
    let drops = Rc::new(Cell::new(0));
    let context = SocketContext::empty();
    assert!(context.set((7_u32, DropCount { drops: drops.clone() })));
    let first = context.get::<(u32, DropCount)>().unwrap();
    let second = context.get::<(u32, DropCount)>().unwrap();
    {
        let mut write = first.try_write().unwrap();
        assert_eq!(write.as_ref().0, 7);
        write.as_mut().0 = 9;
    }
    drop(first);
    assert_eq!(second.try_read().unwrap().as_ref().0, 9);
    assert_eq!(drops.get(), 0);
    drop(second);
    assert_eq!(drops.get(), 0);
    drop(context);
    assert_eq!(drops.get(), 1);
}

/// CTX-U04：读读、读写、写读、写写均立即Busy；失败不会授予第二份资格或更改数据。
#[test]
fn test_all_same_entry_permit_conflicts() {
    let context = SocketContext::empty();
    assert!(context.set(5_u32));
    let first = context.get::<u32>().unwrap();
    let second = context.get::<u32>().unwrap();
    {
        let read = first.try_read().unwrap();
        assert!(matches!(first.try_read(), Err(ContextAccessError::Busy)));
        assert!(matches!(second.try_read(), Err(ContextAccessError::Busy)));
        assert!(matches!(first.try_write(), Err(ContextAccessError::Busy)));
        assert!(matches!(second.try_write(), Err(ContextAccessError::Busy)));
        assert_eq!(*read.as_ref(), 5);
    }
    {
        let write = second.try_write().unwrap();
        assert!(matches!(first.try_read(), Err(ContextAccessError::Busy)));
        assert!(matches!(second.try_read(), Err(ContextAccessError::Busy)));
        assert!(matches!(first.try_write(), Err(ContextAccessError::Busy)));
        assert!(matches!(second.try_write(), Err(ContextAccessError::Busy)));
        assert_eq!(*write.as_ref(), 5);
    }
    assert_eq!(*first.try_read().unwrap().as_ref(), 5);
}

/// CTX-U05：独立对象的资格不互斥；数据占用不阻止get取得原对象的真实持有（ownership）。
#[test]
fn test_independent_entries_and_get_during_access() {
    let left = SocketContext::empty();
    let right = SocketContext::empty();
    assert!(left.set(1_u32));
    assert!(right.set(2_u32));
    let first = left.get::<u32>().unwrap();
    let second = right.get::<u32>().unwrap();
    let mut left_write = first.try_write().unwrap();
    let mut right_write = second.try_write().unwrap();
    *left_write.as_mut() = 3;
    *right_write.as_mut() = 4;
    let cached = left.get::<u32>().expect("get must not acquire a data permit");
    assert!(!left.is_empty());
    assert!(left.get::<u64>().is_none());
    assert!(matches!(cached.try_read(), Err(ContextAccessError::Busy)));
    drop(left_write);
    assert_eq!(*cached.try_read().unwrap().as_ref(), 3);
    assert_eq!(*right_write.as_ref(), 4);
}

/// CTX-U06：任一普通句柄（context handle）尚存都拒绝普通移出，最后归还后返回原值。
#[test]
fn test_remove_refuses_each_live_handle() {
    let context = SocketContext::empty();
    assert!(context.set(11_u32));
    let first = context.get::<u32>().unwrap();
    let second = context.get::<u32>().unwrap();
    let shared = Err("Remove context failed, reason: context shared exist");
    assert_eq!(context.remove::<u32>(), shared);
    drop(first);
    assert_eq!(context.remove::<u32>(), shared);
    assert_eq!(*second.try_read().unwrap().as_ref(), 11);
    drop(second);
    assert_eq!(context.remove::<u32>(), Ok(Some(11)));
    assert!(context.is_empty());
}

/// CTX-U07：外层容器（container）的Rc克隆（clone）不等于业务句柄（context handle）。
#[test]
fn test_outer_container_clones_do_not_block_removal() {
    let context = Rc::new(SocketContext::empty());
    let other = context.clone();
    assert!(context.set(17_u32));
    assert_eq!(other.remove::<u32>(), Ok(Some(17)));
    assert!(context.is_empty());
    assert!(other.is_empty());
}

/// CTX-U08：实际类型（type）检查先于普通持有数及数据占用，所有失败保持原值。
#[test]
fn test_wrong_type_has_priority_and_preserves_value() {
    let context = SocketContext::empty();
    assert!(context.set(23_u32));
    let handle = context.get::<u32>().unwrap();
    let write = handle.try_write().unwrap();
    assert!(context.get::<u64>().is_none());
    assert_eq!(context.remove::<u64>(), Err("Remove context failed, reason: type mismatch"));
    assert_eq!(context.try_remove::<u64>(), Err(ContextRemoveError::TypeMismatch));
    assert!(!context.is_empty());
    assert_eq!(*write.as_ref(), 23);
    drop(write);
    assert_eq!(*handle.try_read().unwrap().as_ref(), 23);
}

/// CTX-U09：活跃读/写许可（access permit）都拒绝移出；归还后可移出且旧对象Closed。
#[test]
fn test_try_remove_busy_then_success() {
    let context = SocketContext::empty();
    assert!(context.set(29_u32));
    let handle = context.get::<u32>().unwrap();
    {
        let read = handle.try_read().unwrap();
        assert_eq!(context.try_remove::<u32>(), Err(ContextRemoveError::Busy));
        assert_eq!(*read.as_ref(), 29);
        assert!(!context.is_empty());
    }
    {
        let write = handle.try_write().unwrap();
        assert_eq!(context.try_remove::<u32>(), Err(ContextRemoveError::Busy));
        assert_eq!(*write.as_ref(), 29);
        assert!(!context.is_empty());
    }
    assert_eq!(context.try_remove::<u32>(), Ok(Some(29)));
    assert!(matches!(handle.try_read(), Err(ContextAccessError::Closed)));
    assert!(matches!(handle.try_write(), Err(ContextAccessError::Closed)));
    assert!(context.is_empty());
}

/// CTX-U10：同类型（type）及不同类型（type）复用均新建对象，所有旧句柄（context handle）保持Closed。
#[test]
fn test_old_handles_closed_after_slot_reuse() {
    let context = SocketContext::empty();
    assert!(context.set(31_u32));
    let old = context.get::<u32>().unwrap();
    assert_eq!(context.try_remove::<u32>(), Ok(Some(31)));
    assert!(context.set(37_u32));
    let newer = context.get::<u32>().unwrap();
    assert!(matches!(old.try_read(), Err(ContextAccessError::Closed)));
    assert!(matches!(old.try_write(), Err(ContextAccessError::Closed)));
    assert_eq!(*newer.try_read().unwrap().as_ref(), 37);
    assert_eq!(context.try_remove::<u32>(), Ok(Some(37)));
    assert!(context.set(String::from("new generation")));
    assert!(context.get::<u32>().is_none());
    assert!(matches!(old.try_read(), Err(ContextAccessError::Closed)));
    assert!(matches!(newer.try_write(), Err(ContextAccessError::Closed)));
    let current = context.get::<String>().unwrap();
    assert_eq!(current.try_read().unwrap().as_ref(), "new generation");
}

/// CTX-U11：容器（container）先归还，缓存句柄（context handle）仍访问原值，最后持有才析构（Drop）。
#[test]
fn test_container_drop_does_not_revoke_live_handle() {
    let drops = Rc::new(Cell::new(0));
    let context = SocketContext::empty();
    assert!(context.set(DropCount { drops: drops.clone() }));
    let handle = context.get::<DropCount>().unwrap();
    drop(context);
    assert_eq!(drops.get(), 0);
    {
        let read = handle.try_read().unwrap();
        assert!(Rc::ptr_eq(&read.as_ref().drops, &drops));
    }
    drop(handle);
    assert_eq!(drops.get(), 1);
}

/// CTX-U12：两种移出都返回未析构（Drop）的owned T，旧对象归还不重复析构（Drop）T。
#[test]
fn test_destruction_and_returned_value_are_separate() {
    let ordinary_drops = Rc::new(Cell::new(0));
    let ordinary = SocketContext::empty();
    assert!(ordinary.set(DropCount { drops: ordinary_drops.clone() }));
    let value = ordinary.remove::<DropCount>().unwrap().unwrap();
    assert_eq!(ordinary_drops.get(), 0);
    drop(ordinary);
    assert_eq!(ordinary_drops.get(), 0);
    drop(value);
    assert_eq!(ordinary_drops.get(), 1);

    let forced_drops = Rc::new(Cell::new(0));
    let forced = SocketContext::empty();
    assert!(forced.set(DropCount { drops: forced_drops.clone() }));
    let handle = forced.get::<DropCount>().unwrap();
    let value = forced.try_remove::<DropCount>().unwrap().unwrap();
    assert_eq!(forced_drops.get(), 0);
    assert!(matches!(handle.try_read(), Err(ContextAccessError::Closed)));
    drop(handle);
    drop(forced);
    assert_eq!(forced_drops.get(), 0);
    drop(value);
    assert_eq!(forced_drops.get(), 1);
}

/// CTX-U13：仅本测试（test）保留私有独占借用（exclusive borrow），逐项验证契约诊断；
/// 不增加生产后门（backdoor），不把非法内部重入（internal reentrancy）伪报成业务Busy/空值。
#[test]
fn test_slot_reentry_is_not_reported_as_business_failure() {
    let context = SocketContext::empty();
    let guard = context.slot.borrow_mut();
    assert_panic_message("Context slot borrow conflict", || { let _ = context.is_empty(); });
    assert_panic_message("Context slot borrow conflict", || { let _ = context.get::<u8>(); });
    assert_panic_message("Context slot borrow conflict", || { let _ = context.set(1_u8); });
    assert_panic_message("Context slot borrow conflict", || { let _ = context.remove::<u8>(); });
    assert_panic_message("Context slot borrow conflict", || { let _ = context.try_remove::<u8>(); });
    drop(guard);
    assert!(context.is_empty());
    assert!(context.set(1_u8));
    assert_eq!(context.remove::<u8>(), Ok(Some(1)));
}

/// CTX-U14：读/写许可（access permit）遇用户展开（unwind）仍归还资格，已完成修改不回滚。
#[test]
fn test_unwind_releases_permit_without_rollback() {
    let context = SocketContext::empty();
    assert!(context.set(41_u32));
    let handle = context.get::<u32>().unwrap();
    assert_panic_message("controlled write unwind", || {
        let mut write = handle.try_write().unwrap();
        *write.as_mut() = 43;
        panic!("controlled write unwind");
    });
    assert_eq!(*handle.try_read().unwrap().as_ref(), 43);
    assert_panic_message("controlled read unwind", || {
        let read = handle.try_read().unwrap();
        assert_eq!(*read.as_ref(), 43);
        panic!("controlled read unwind");
    });
    assert_eq!(*handle.try_write().unwrap().as_ref(), 43);
}

/// CTX-U15：显式遗忘读/写许可（access permit）后只验证防御性Busy，不承诺恢复遗忘资格；
/// 只用合法公开出口，不以私有赋值伪造业务可达性（reachability）。
#[test]
fn test_data_borrow_defense_preserves_live_slot() {
    for write in [false, true] {
        let context = SocketContext::empty();
        assert!(context.set(47_u32));
        let handle = context.get::<u32>().unwrap();
        if write {
            std::mem::forget(handle.try_write().unwrap());
        } else {
            std::mem::forget(handle.try_read().unwrap());
        }
        drop(handle);
        assert_eq!(context.remove::<u32>(), Err("Remove context failed, reason: data access is busy"));
        assert_eq!(context.try_remove::<u32>(), Err(ContextRemoveError::Busy));
        assert!(!context.is_empty());
        assert!(context.get::<u32>().is_some());
    }
}

/// 析构观察器（Drop observer）仅持弱引用（Weak），避免为观察引入持有环（reference cycle）。
/// 通过公开移出入口验证可重入（reentrant）写操作，不读取生产内部借用标志（borrow flag）。
struct ObserveDrop {
    context: Weak<SocketContext>,
    seen: Rc<RefCell<Vec<(bool, Option<u8>)>>>,
}

impl Drop for ObserveDrop {
    fn drop(&mut self) {
        let context = self.context.upgrade().expect("the observed container must still be alive");
        let empty = context.is_empty();
        let value = context.remove::<u8>().expect("user drop must run outside internal borrows");
        self.seen.borrow_mut().push((empty, value));
    }
}

/// CTX-U16：失败输入及两种移出值的用户析构（Drop）可在内部借用（borrow）外重入；
/// 失败输入主动移出原值是用户副作用（side effect），与本库是否覆盖原值分开核对。
#[test]
fn test_user_drop_runs_outside_internal_borrows() {
    let context = Rc::new(SocketContext::empty());
    let seen = Rc::new(RefCell::new(Vec::new()));
    assert!(context.set(53_u8));
    assert!(!context.set(ObserveDrop { context: Rc::downgrade(&context), seen: seen.clone() }));
    assert_eq!(&*seen.borrow(), &[(false, Some(53))]);
    assert!(context.is_empty());

    seen.borrow_mut().clear();
    assert!(context.set(ObserveDrop { context: Rc::downgrade(&context), seen: seen.clone() }));
    let value = context.remove::<ObserveDrop>().unwrap().unwrap();
    assert!(seen.borrow().is_empty());
    assert!(context.is_empty());
    drop(value);
    assert_eq!(&*seen.borrow(), &[(true, None)]);

    seen.borrow_mut().clear();
    assert!(context.set(ObserveDrop { context: Rc::downgrade(&context), seen: seen.clone() }));
    let handle = context.get::<ObserveDrop>().unwrap();
    let value = context.try_remove::<ObserveDrop>().unwrap().unwrap();
    assert!(seen.borrow().is_empty());
    assert!(matches!(handle.try_read(), Err(ContextAccessError::Closed)));
    drop(value);
    assert_eq!(&*seen.borrow(), &[(true, None)]);
    drop(handle);
    assert_eq!(&*seen.borrow(), &[(true, None)]);
}

/// CTX-U17：None业务值仍是已安装对象，普通/强制移出保留嵌套Option而不扁平化。
#[test]
fn test_nested_option_is_installed_value() {
    let context = SocketContext::empty();
    assert!(context.set(None::<u8>));
    assert!(!context.is_empty());
    let handle = context.get::<Option<u8>>().unwrap();
    assert_eq!(*handle.try_read().unwrap().as_ref(), None);
    assert!(context.get::<u8>().is_none());
    drop(handle);
    assert_eq!(context.remove::<Option<u8>>(), Ok(Some(None)));
    assert!(context.set(None::<u8>));
    let old = context.get::<Option<u8>>().unwrap();
    assert_eq!(context.try_remove::<Option<u8>>(), Ok(Some(None)));
    assert!(matches!(old.try_read(), Err(ContextAccessError::Closed)));
    assert!(context.set(Some(59_u8)));
    assert_eq!(context.try_remove::<Option<u8>>(), Ok(Some(Some(59))));
    assert!(context.is_empty());
}
