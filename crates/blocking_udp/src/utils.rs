use std::ptr;
use std::net::SocketAddr;
use std::io::Result as IOResult;
use std::ops::{Deref, DerefMut};
use std::cell::{Ref, RefMut, RefCell};
use std::sync::{Arc, atomic::{AtomicPtr, Ordering}};

use futures::future::{FutureExt, LocalBoxFuture};
use dashmap::DashMap;
use parking_lot::RwLock;
use crossbeam_channel::Sender;
use async_channel::Sender as AsyncSender;

use pi_hash::XHashMap;

use crate::{Socket, SocketHandle};

///
/// Udp连接多播接口
///
pub enum UdpMultiInterface {
    V4(SocketAddr),
    V6(u32),
}

///
/// Udp连接状态
///
#[derive(Debug)]
pub enum UdpSocketStatus {
    SingleCast(Option<SocketAddr>),             //单播
    MultiCast(Arc<DashMap<SocketAddr, ()>>),    //多播
    BroadCast,                                  //广播
}

impl Clone for UdpSocketStatus {
    fn clone(&self) -> Self {
        match self {
            UdpSocketStatus::SingleCast(address) => UdpSocketStatus::SingleCast(address.clone()),
            UdpSocketStatus::MultiCast(map) => UdpSocketStatus::MultiCast(map.clone()),
            UdpSocketStatus::BroadCast => UdpSocketStatus::BroadCast,
        }
    }
}

impl UdpSocketStatus {
    /// 是否是单播
    pub fn is_singlecast(&self) -> bool {
        if let UdpSocketStatus::SingleCast(_) = self {
            true
        } else {
            false
        }
    }

    /// 是否是多播
    pub fn is_multicast(&self) -> bool {
        if let UdpSocketStatus::MultiCast(_) = self {
            true
        } else {
            false
        }
    }

    /// 是否广播
    pub fn is_broadcast(&self) -> bool {
        if let UdpSocketStatus::BroadCast = self {
            true
        } else {
            false
        }
    }

    /// 插入多播表
    pub fn insert_multicast(&self, address: SocketAddr) {
        if let UdpSocketStatus::MultiCast(map) = self {
            map.insert(address, ());
        }
    }

    /// 移除多播表，反回移除后多播表的大小
    pub fn remove_multicast(&self, address: &SocketAddr) -> usize {
        if let UdpSocketStatus::MultiCast(map) = self {
            let _ = map.remove(address);
            return map.len();
        }

        0
    }
}

///
/// 通用上下文
/// 注意，设置上下文后，需要移除当前上下文，上下文才会自动释放
///
pub struct SocketContext {
    inner:  AtomicPtr<()>,  //内部上下文
}

unsafe impl Send for SocketContext {}

impl SocketContext {
    /// 创建空的上下文
    pub fn empty() -> Self {
        SocketContext {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// 判断上下文是否为空
    pub fn is_empty(&self) -> bool {
        self
            .inner
            .load(Ordering::Acquire)
            .is_null()
    }

    /// 获取上下文的句柄
    pub fn get<T>(&self) -> Option<ContextHandle<T>> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { ContextHandle(Some(Arc::from_raw(self.inner.load(Ordering::Acquire) as *const T))) })
    }

    /// 设置上下文，如果当前上下文不为空，则设置失败
    pub fn set<T>(&self, context: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner.store(Arc::into_raw(Arc::new(context)) as *mut T as *mut (),
                         Ordering::Release);
        true
    }

    /// 移除上下文，如果当前还有未释放的上下文句柄，则返回移除错误，如果当前有上下文，则返回被移除的上下文，否则返回空
    pub fn remove<T>(&self) -> Result<Option<T>, &str> {
        if self.is_empty() {
            return Ok(None);
        }

        let inner = unsafe { Arc::from_raw(self.inner.load(Ordering::Acquire) as *const T) };
        if Arc::strong_count(&inner) > 1 {
            Arc::into_raw(inner); //释放临时共享指针
            Err("Remove context failed, reason: context shared exist")
        } else {
            match Arc::try_unwrap(inner) {
                Err(inner) => {
                    Arc::into_raw(inner); //释放临时共享指针
                    Err("Remove context failed, reason: invalid shared")
                },
                Ok(context) => {
                    //将当前内部上下文设置为空，并返回上下文
                    self.inner.store(ptr::null_mut(), Ordering::Release);
                    Ok(Some(context))
                },
            }
        }
    }
}

///
/// 上下文句柄
///
pub struct ContextHandle<T: 'static>(Option<Arc<T>>);

unsafe impl<T: 'static> Send for ContextHandle<T> {}

impl<T: 'static> Drop for ContextHandle<T> {
    fn drop(&mut self) {
        if let Some(shared) = self.0.take() {
            //当前上下文指针存在，则释放
            Arc::into_raw(shared);
        }
    }
}

impl<T: 'static> ContextHandle<T> {
    /// 获取上下文只读引用
    pub fn as_ref(&self) -> &T {
        self.0.as_ref().unwrap().as_ref()
    }

    /// 获取上下文可写引用
    pub fn as_mut(&mut self) -> Option<&mut T> {
        if let Some(shared) = self.0.as_mut() {
            return Arc::get_mut(shared);
        }

        None
    }
}

///
/// 共享连接
///
pub struct SharedSocket<S: Socket>(Arc<RefCell<S>>);

unsafe impl<S: Socket> Send for SharedSocket<S> {}

impl<S: Socket> Clone for SharedSocket<S> {
    fn clone(&self) -> Self {
        SharedSocket(self.0.clone())
    }
}

impl<S: Socket> SharedSocket<S> {
    /// 构建一个共享连接
    pub fn new(s: S) -> Self {
        SharedSocket(Arc::new(RefCell::new(s)))
    }

    /// 使用内部共享连接构建一个共享连接
    pub fn with_inner(inner: &Arc<RefCell<S>>) -> Self {
        SharedSocket(inner.clone())
    }

    /// 获取内部共享连接的只读引用
    pub fn inner_ref(&self) -> &Arc<RefCell<S>> {
        &self.0
    }

    /// 获取共享连接的只读引用
    #[inline]
    pub fn borrow<'a>(&'a self) -> SharedRef<'a, S> {
        SharedRef(self.0.borrow())
    }

    /// 获取共享连接的可写引用
    #[inline]
    pub fn borrow_mut<'a>(&'a self) -> SharedRefMut<'a, S> {
        SharedRefMut(self.0.borrow_mut())
    }

    /// 获取共享连接的指针
    #[inline]
    pub fn as_ptr(&self) -> *mut S {
        self.0.as_ptr()
    }
}

///
/// 共享连接的只读引用
///
pub struct SharedRef<'a, S: Socket>(Ref<'a, S>);

unsafe impl<'a, S: Socket> Send for SharedRef<'a, S> {}

impl<'a, S: Socket> Deref for SharedRef<'a, S> {
    type Target = S;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

///
/// 共享连接的可写引用
///
pub struct SharedRefMut<'a, S: Socket>(RefMut<'a, S>);

unsafe impl<'a, S: Socket> Send for SharedRefMut<'a, S> {}

impl<'a, S: Socket> Deref for SharedRefMut<'a, S> {
    type Target = S;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<'a, S: Socket> DerefMut for SharedRefMut<'a, S> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}