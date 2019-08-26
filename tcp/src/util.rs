use std::mem;
use std::thread;
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
* IO数组
*/
pub struct IoArr(bool, usize, *mut u8);

unsafe impl Send for IoArr {}

impl Drop for IoArr {
    fn drop(&mut self) {
        if self.0 {
            //已释放，则忽略
            return;
        }
        self.0 = true;

        unsafe { Vec::from_raw_parts(self.2, self.1, self.1); }
    }
}

impl<const N: usize> From<&'static [u8; N]> for IoArr {
    fn from(slice: &'static [u8; N]) -> Self {
        let len = slice.len();
        if len > MAX_LENGTH {
            panic!("slice to io array failed, invalid slice length");
        }

        let ptr = slice.as_ptr() as *mut u8;
        mem::forget(slice);

        IoArr(false, len, ptr)
    }
}

impl From<Vec<u8>> for IoArr {
    fn from(mut vec: Vec<u8>) -> Self {
        let len = vec.len();
        if len > MAX_LENGTH {
            panic!("vectory to io array failed, invalid vectory length");
        }

        let ptr = vec.as_mut_ptr();
        mem::forget(vec);

        IoArr(false, len, ptr)
    }
}

impl From<IoArr> for Vec<u8> {
    fn from(mut arr: IoArr) -> Self {
        arr.0 = true; //声明已释放
        unsafe { Vec::from_raw_parts(arr.2, arr.1, arr.1) }
    }
}

impl IoArr {
    //构建一个指定容量的IO数组
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity > MAX_LENGTH {
            panic!("new io array failed, invalid array length");
        }

        let mut vec = Vec::with_capacity(capacity);
        vec.resize(capacity, 0);
        let ptr = vec.as_mut_ptr();
        mem::forget(vec);

        IoArr(false, capacity, ptr)
    }

    //获取IO数组长度
    pub fn len(&self) -> usize {
        self.1
    }

    //获取只读引用
    pub fn as_ref(&self) -> &[u8] {
        unsafe { from_raw_parts(self.2 as *const u8, self.1) }
    }

    //获取可写引用
    pub fn as_mut(&mut self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.2, self.1) }
    }
}

/*
* IO列表
*/
pub struct IoList(usize, VecDeque<IoArr>);

unsafe impl Send for IoList {}

impl From<Vec<IoArr>> for IoList {
    fn from(vec: Vec<IoArr>) -> Self {
        let mut len = 0;
        for arr in &vec {
            len += arr.len();
        }
        let queue: VecDeque<IoArr> = vec.into();

        IoList(len, queue)
    }
}

impl From<IoList> for Vec<IoArr> {
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

    //在列表前部增加IO数组
    pub fn push_front(&mut self, arr: IoArr) {
        let len = arr.len();
        if len == 0 {
            return;
        }

        self.1.push_front(arr);
        self.0 += len;
    }

    //在列表后部增加IO数组
    pub fn push_back(&mut self, arr: IoArr) {
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
