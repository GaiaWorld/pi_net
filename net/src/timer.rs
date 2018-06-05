use mio_extras::timer::{Timeout, Timer};

use std::cell::RefCell;
use std::sync::{Arc};
use std::thread;

use slab::Slab;
use std::time::Duration;

pub struct NetTimer<T> {
    timer: Arc<RefCell<Timer<T>>>,
}

pub type NetTimeout = Timeout;

//定时器管理
pub struct NetTimers<T> {
    timers: Slab<(Timer<T>, NetTimeout)>,
}

//回调函数
pub type TimerCallback = Arc<fn(usize)>;

impl<T> NetTimer<T> {
    pub fn new() -> NetTimer<T> {
        NetTimer {
            timer: Arc::new(RefCell::new(Timer::default())),
        }
    }

    pub fn set_timeout(&self, delay_from_now: Duration, state: T) -> NetTimeout {
        self.timer.borrow_mut().set_timeout(delay_from_now, state)
    }

    pub fn poll(&self) -> Option<T> {
        self.timer.borrow_mut().poll()
    }

    pub fn cancel_timeout(&self, timeout: &NetTimeout) -> Option<T> {
        self.timer.borrow_mut().cancel_timeout(timeout)
    }
}

//设置定时器到期自动触发回调
impl NetTimers<TimerCallback> {
    //在pi_net NetHandler 中初始化
    pub fn new() -> Self {
        Self {
            timers: Slab::new(),
        }
    }
    //设置定时器，并设置回调,返回定时器ID
    pub fn set_timeout(&mut self, delay_from_now: Duration, state: TimerCallback) -> usize {
        let mut timer = Timer::default();
        let timeout = timer.set_timeout(delay_from_now, state);
        self.timers.insert((timer, timeout))
    }
    //取消定时器，传入设置定时器返回的ID
    pub fn cancel_timeout(&mut self, id: usize) -> Option<TimerCallback> {
        let v = match self.timers.get_mut(id) {
            Some((timer, timeout)) => {
                timer.cancel_timeout(timeout)
            },
            None => None,
        };
        self.timers.remove(id);
        v
    }
    //mio中轮训，到期开新进程触发回调
    pub fn poll(&mut self) {
        let mut vec = Vec::new();
        for (id, (timer, _timeout)) in self.timers.iter_mut() {
            match timer.poll() {
                Some(cb) => {
                    thread::spawn(move || {
                        (*cb)(id)
                    });
                    vec.push(id)
                }
                None => (),
            }
        }
        for id in vec {
            self.timers.remove(id);
        }
    }
}

#[cfg(test)]
mod test {
    use timer::{NetTimers};
    use std::time::{Duration};
    use std::sync::{Arc};
    use std::thread;

    #[test]
    pub fn timer_test() {
        let mut timers = NetTimers::new();
        let id1 = timers.set_timeout(Duration::from_millis(100), 
        Arc::new(|id2: usize| {
            println!("timeout id2: {}", id2)
        }));
        
        println!("timeout id1: {}", id1);
        //下面由mio线程轮询
        {
            thread::sleep(Duration::from_millis(500));
            timers.poll();
        }
    }

    #[test]
    pub fn cancel_timeout_test() {
        let mut timers = NetTimers::new();
        let id = timers.set_timeout(Duration::from_millis(200), 
        Arc::new(|_id: usize| {
            assert!(false);
        }));
        
        println!("timeout cancel id: {}", id);
        //取消定时器
        timers.cancel_timeout(id);
        //下面由mio线程轮询
        {
            thread::sleep(Duration::from_millis(500));
            timers.poll();
        }
    }
}