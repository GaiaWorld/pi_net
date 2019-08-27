use std::mem;
use std::sync::{Weak, Arc};
use std::marker::PhantomData;
use std::io::{ErrorKind, Result, Error};
use std::sync::atomic::{AtomicUsize, Ordering};

use iovec::IoVec;
use crossbeam_channel::{Sender, Receiver, bounded};

use crate::util::{pause, IoBytes, IoList};

/*
* Tcp连接写缓冲的只读视图
*/
pub struct ReadableView {
    recover: Sender<IoList>,            //写缓冲回收器
    inner:  Option<Arc<Vec<IoBytes>>>,    //只读共享指针
}

unsafe impl Send for ReadableView {}
unsafe impl Sync for ReadableView {}

impl Drop for ReadableView {
    fn drop(&mut self) {
        if self.inner.is_none() {
            //没有读共享指针，则忽略
            return;
        }

        //有读共享指针，则将IO数据向量转换为IO列表，清空IO列表并回收
        let shared = self.inner.take().unwrap();
        if let Ok(vec) = Arc::try_unwrap(shared) {
            let mut list = IoList::from(vec);
            list.clear();
            if let Err(e) = self.recover.send(list) {
                println!("!!!> Drop Read View Error, reason: {:?}", e);
            }
        }
    }
}

impl From<WriteBuffer> for ReadableView {
    //一个写缓冲，如果需要读，必须转换为只读视图
    fn from(mut buf: WriteBuffer) -> Self {
        let shared = buf.inner.take().unwrap();
        let iolist = Arc::try_unwrap(shared).ok().unwrap();

        ReadableView {
            recover: buf.recover.clone(),
            inner: Some(Arc::new(Vec::from(iolist))),
        }
    }
}

impl ReadableView {
    //获取iovec
    pub fn get_iovec<'a>(&'a self) -> Vec<&'a IoVec> {
        self.inner.as_ref().unwrap().iter().map(|arr| {
            arr.as_ref().into()
        }).collect::<Vec<&IoVec>>()
    }
}

/*
* Tcp连接写缓冲句柄
*/
#[derive(Clone)]
pub struct WriteBufferHandle(Weak<ReadableView>);

unsafe impl Send for WriteBufferHandle {}
unsafe impl Sync for WriteBufferHandle {}

impl WriteBufferHandle {
    //构建Tcp连接写缓冲句柄
    fn new(weak: Weak<ReadableView>) -> Self {
        WriteBufferHandle(weak)
    }

    //获取iovec
    pub fn get_shared(&self) -> Option<Arc<ReadableView>> {
        match self.0.upgrade() {
            None =>  None,
            Some(shared) => {
                Some(shared)
            },
        }
    }
}

/*
* Tcp连接写缓冲
*/
pub struct WriteBuffer {
    recover:    Sender<IoList>,             //写缓冲回收器
    sender:     Sender<Arc<ReadableView>>,  //写就绪发送器
    inner:      Option<Arc<IoList>>,        //可写共享指针
}

unsafe impl Send for WriteBuffer {}
unsafe impl Sync for WriteBuffer {}

impl Drop for WriteBuffer {
    fn drop(&mut self) {
        if self.inner.is_none() {
            //没有写共享指针，则忽略
            return;
        }

        //有写共享指针，则清空IO列表并回收
        let shared = self.inner.take().unwrap();
        if let Ok(mut list) = Arc::try_unwrap(shared) {
            list.clear();
            if let Err(e) = self.recover.send(list) {
                println!("!!!> Drop Writable Buffer Error, reason: {:?}", e);
            }
        }
    }
}

impl WriteBuffer {
    //构建指定回收器和IO列表的Tcp连接写缓冲
    fn with_iolist(recover: Sender<IoList>, sender: Sender<Arc<ReadableView>>, iolist: IoList) -> Self {
        WriteBuffer {
            recover,
            sender,
            inner: Some(Arc::new(iolist)),
        }
    }

    //完成写缓冲
    pub fn finish(self) -> Option<WriteBufferHandle> {
        if self.inner.as_ref().unwrap().byte_len() < 0 {
            //没有写入任何数据，则忽略
            return None;
        }

        //将写缓冲转换为只读视图的共享指针，并发送到写缓冲池的写就绪队列
        let sender = self.sender.clone();
        let shared = Arc::new(ReadableView::from(self));
        let weak = Arc::downgrade(&shared);
        if let Err(e) = sender.send(shared) {
            //发送失败，则忽略
            println!("!!!> Finish Writable Buffer Error, reason: {:?}", e);
            return None;
        }

        Some(WriteBufferHandle::new(weak))
    }

    //获取可写的IO列表
    pub fn get_iolist_mut(&mut self) -> &mut IoList {
        Arc::get_mut(self.inner.as_mut().unwrap()).unwrap()
    }
}

/*
* Tcp连接写缓冲池
*/
#[derive(Clone)]
pub struct WriteBufferPool {
    frees:      Receiver<IoList>,               //空闲写缓冲队列
    recover:    Sender<IoList>,                 //写缓冲回收器
    ready:      Receiver<Arc<ReadableView>>,    //写准备就绪队列
    sender:     Sender<Arc<ReadableView>>,      //就绪发送器
    size:       Arc<AtomicUsize>,               //写缓冲数量
    iolist_cap: usize,                          //IO列表的默认容量
}

unsafe impl Send for WriteBufferPool {}
unsafe impl Sync for WriteBufferPool {}

impl WriteBufferPool {
    //构建指定容量、初始数量和IO列表的默认容量的Tcp连接写缓冲池
    pub fn new(pool_capacity: usize, init_count: usize, iolist_cap: usize) -> Result<Self> {
        if pool_capacity == 0 {
            return Err(Error::new(ErrorKind::Other, "invalid init pool capacity"));
        }

        if init_count > pool_capacity {
            return Err(Error::new(ErrorKind::Other, "invalid init count"));
        }

        //初始化缓冲池
        let (recover, frees) = bounded(pool_capacity);
        for _ in 0..init_count {
            recover.send(IoList::with_capacity(iolist_cap));
        }
        let size = Arc::new(AtomicUsize::new(init_count));
        let (sender, ready) = bounded(pool_capacity);

        Ok(WriteBufferPool {
            frees,
            recover,
            ready,
            sender,
            size,
            iolist_cap,
        })
    }

    //获取写缓冲池的容量
    pub fn capacity(&self) -> usize {
        self.frees.capacity().unwrap()
    }

    //获取写缓冲池的写缓冲数量
    pub fn size(&self) -> usize {
        self.size.load(Ordering::SeqCst)
    }

    //获取写缓冲池中空闲写缓冲数量
    pub fn free_size(&self) -> usize {
        self.frees.len()
    }

    //获取写就绪的数量
    pub fn ready_size(&self) -> usize {
        self.ready.len()
    }

    //分配一个空闲的写缓冲
    pub fn alloc(&self) -> Result<Option<WriteBuffer>> {
        match self.frees.try_recv() {
            Ok(free) => {
                //有空闲IO列表，则返回写缓冲
                Ok(Some(WriteBuffer::with_iolist(self.recover.clone(), self.sender.clone(), free)))
            },
            Err(ref e) if e.is_empty() => {
                //当前没有空闲IO列表
                Ok(self.new_iolist())
            }
            Err(e) => {
                Err(Error::new(ErrorKind::BrokenPipe, e))
            },
        }
    }

    //线程安全的创建新的写缓冲
    fn new_iolist(&self) -> Option<WriteBuffer> {
        let capacity = self.capacity();
        let mut cur_size = self.size();
        if cur_size >= capacity {
            //当前缓冲池容量不足，则返回空
            None
        } else {
            //当前缓冲池容量足够
            loop {
                match self.size.compare_and_swap(cur_size, cur_size + 1, Ordering::SeqCst) {
                    new_cur_size if new_cur_size == cur_size => {
                        //增加写缓冲数量成功，则创建新的IO列表，并返回写缓冲
                        return Some(WriteBuffer::with_iolist(self.recover.clone(), self.sender.clone(), IoList::with_capacity(self.iolist_cap)));
                    },
                    new_cur_size if new_cur_size >= capacity => {
                        //增加写缓冲数据失败，且当前缓冲池容量不足，则返回空
                        return None;
                    },
                    new_cur_size => {
                        //增加写缓冲数据失败，但当前缓冲池容量足够，则重试
                        cur_size = new_cur_size;
                        pause();
                    },
                }
            }
        }
    }

    //事理写缓冲池，释放已完成发送的写缓冲
    pub fn collect(&self) {
        if self.ready.len() < self.free_size() {
            //如果写就绪队列长度为空，或小于写缓冲池空闲队列长度，则忽略
            return;
        }

        let views = self.ready.try_iter().collect::<Vec<Arc<ReadableView>>>();
        for view in views {
            if Arc::weak_count(&view) == 0 {
                //外部没有握住当前写缓冲的只读视图，则立即释放当前写缓冲的只读视图，释放后将会回收对应的Io列表
                mem::drop(view);
            } else {
                //外部握住了当前写缓冲的只读视图，则继续等待下次整理时回收当前写缓冲的只读视图
                self.sender.send(view);
            }
        }
    }
}