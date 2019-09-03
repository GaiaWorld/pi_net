use std::mem;
use std::ptr;
use std::thread;
use std::sync::Arc;
use std::time::Duration;
use std::collections::VecDeque;
use std::slice::{from_raw_parts, from_raw_parts_mut};

use iovec::MAX_LENGTH;

#[cfg(all(feature="unstable", any(target_arch = "x86", target_arch = "x86_64")))]
#[inline(always)]
pub fn pause() {
    unsafe { asm!("PAUSE") };
}

#[cfg(all(not(feature="unstable"), any(target_arch = "x86", target_arch = "x86_64")))]
#[inline(always)]
pub fn pause() {
    thread::sleep(Duration::from_millis(1));
}

#[cfg(all(not(target_arch = "x86"), not(target_arch = "x86_64")))]
#[inline(always)]
pub fn pause() {
    thread::sleep(Duration::from_millis(1));
}

/*
* 通用上下文
* 注意，设置上下文后，需要移除当前上下文，上下文才会自动释放
*/
pub struct SocketContext {
    inner: *const (), //内部上下文
}

impl SocketContext {
    //创建空的上下文
    pub fn empty() -> Self {
        SocketContext {
            inner: ptr::null(),
        }
    }

    //判断上下文是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.is_null()
    }

    //获取上下文的句柄
    pub fn get<T: 'static>(&self) -> Option<Arc<T>> {
        if self.is_empty() {
            return None;
        }

        Some(unsafe { Arc::from_raw(self.inner as *const T) })
    }

    //设置上下文，如果当前上下文不为空，则设置失败
    pub fn set<T: 'static>(&mut self, context: T) -> bool {
        if !self.is_empty() {
            return false;
        }

        self.inner = Arc::into_raw(Arc::new(context)) as *const T as *const ();
        true
    }

    //移除上下文，如果当前还有未释放的上下文句柄，则返回移除错误，如果当前有上下文，则返回被移除的上下文，否则返回空
    pub fn remove<T: 'static>(&mut self) -> Result<Option<T>, &str> {
        if self.is_empty() {
            return Ok(None);
        }

        let inner = unsafe { Arc::from_raw(self.inner as *const T) };
        if Arc::strong_count(&inner) > 0 {
            Arc::into_raw(inner); //释放临时共享指针
            Err("remove context failed, reason: context shared exist")
        } else {
            match Arc::try_unwrap(inner) {
                Err(inner) => {
                    Arc::into_raw(inner); //释放临时共享指针
                    Err("remove context failed, reason: invalid shared")
                },
                Ok(context) => {
                    //将当前内部上下文设置为空，并返回上下文
                    self.inner = ptr::null();
                    Ok(Some(context))
                },
            }
        }
    }
}

/*
* IO数据源
*/
enum IoBytesOrigin {
    Slice(*mut u8),
    Vec(*mut u8),
}

/*
* IO数据
*/
pub struct IoBytes(bool, usize, IoBytesOrigin);

unsafe impl Send for IoBytes {}

impl Drop for IoBytes {
    fn drop(&mut self) {
        if self.0 {
            //已释放，则忽略
            return;
        }
        self.0 = true;

        match self.2 {
            IoBytesOrigin::Slice(raw) => {
                unsafe { from_raw_parts(raw as *const u8, self.1); }
            },
            IoBytesOrigin::Vec(raw) => {
                unsafe { Vec::from_raw_parts(raw, self.1, self.1); }
            },
        }
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for IoBytes {
    fn from(slice: &'a [u8; N]) -> Self {
        let len = slice.len();
        if len > MAX_LENGTH {
            panic!("slice to io array failed, invalid slice length");
        }

        let raw = slice.as_ptr() as *mut u8;
        mem::forget(slice);

        IoBytes(false, len, IoBytesOrigin::Slice(raw))
    }
}

impl<'a> From<IoBytes> for &'a [u8] {
    fn from(mut arr: IoBytes) -> Self {
        arr.0 = true; //声明已释放

        if let IoBytesOrigin::Slice(raw) = arr.2 {
            unsafe { from_raw_parts_mut(raw, arr.1) }
        } else {
            panic!("from IoArr to Slice failed, invalid IoArr");
        }
    }
}

impl From<Vec<u8>> for IoBytes {
    fn from(mut vec: Vec<u8>) -> Self {
        let len = vec.len();
        if len > MAX_LENGTH {
            panic!("vectory to io array failed, invalid vectory length");
        }

        let raw = vec.as_mut_ptr();
        mem::forget(vec);

        IoBytes(false, len, IoBytesOrigin::Vec(raw))
    }
}

impl From<IoBytes> for Vec<u8> {
    fn from(mut arr: IoBytes) -> Self {
        arr.0 = true; //声明已释放

        if let IoBytesOrigin::Vec(raw) = arr.2 {
            unsafe { Vec::from_raw_parts(raw, arr.1, arr.1) }
        } else {
            panic!("from IoArr to Vec<u8> failed, invalid IoArr");
        }
    }
}

impl IoBytes {
    //构建一个指定容量的IO数据
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity > MAX_LENGTH {
            panic!("new io array failed, invalid array length");
        }

        let mut vec = Vec::with_capacity(capacity);
        vec.resize(capacity, 0);
        let raw = vec.as_mut_ptr();
        mem::forget(vec);

        IoBytes(false, capacity, IoBytesOrigin::Vec(raw))
    }

    //获取IO数据长度
    pub fn len(&self) -> usize {
        self.1
    }

    //获取只读引用
    pub fn as_ref(&self) -> &[u8] {
        match self.2 {
            IoBytesOrigin::Slice(raw) => {
                unsafe { from_raw_parts(raw as *const u8, self.1) }
            },
            IoBytesOrigin::Vec(raw) => {
                unsafe { from_raw_parts(raw as *const u8, self.1) }
            },
        }
    }

    //获取可写引用
    pub fn as_mut(&mut self) -> &mut [u8] {
        match self.2 {
            IoBytesOrigin::Slice(raw) => {
                unsafe { from_raw_parts_mut(raw, self.1) }
            },
            IoBytesOrigin::Vec(raw) => {
                unsafe { from_raw_parts_mut(raw, self.1) }
            },
        }
    }
}

/*
* IO列表
*/
pub struct IoList(usize, VecDeque<IoBytes>);

unsafe impl Send for IoList {}

impl From<Vec<IoBytes>> for IoList {
    fn from(vec: Vec<IoBytes>) -> Self {
        let mut len = 0;
        for arr in &vec {
            len += arr.len();
        }
        let queue: VecDeque<IoBytes> = vec.into();

        IoList(len, queue)
    }
}

impl From<IoList> for Vec<IoBytes> {
    fn from(list: IoList) -> Self {
        list.1.into()
    }
}

impl IoList {
    //构建一个指定初始容量的IO列表
    pub fn with_capacity(capacity: usize) -> Self {
        IoList(0, VecDeque::with_capacity(capacity))
    }

    //获取当前IO列表字节长度
    pub fn byte_len(&self) -> usize {
        self.0
    }

    //获取当前IO列表长度
    pub fn len(&self) -> usize {
        self.1.len()
    }

    //在列表前部增加IO数据
    pub fn push_front(&mut self, arr: IoBytes) {
        let len = arr.len();
        if len == 0 {
            return;
        }

        self.1.push_front(arr);
        self.0 += len;
    }

    //在列表后部增加IO数据
    pub fn push_back(&mut self, arr: IoBytes) {
        let len = arr.len();
        if len == 0 {
            return;
        }

        self.1.push_back(arr);
        self.0 += len;
    }

    //清空IO列表
    pub fn clear(&mut self) {
        self.1.clear();
        self.0 = 0;
    }
}
