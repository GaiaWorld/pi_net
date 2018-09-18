use mio_extras::timer::{Timeout, Timer};
use pi_lib::atom::Atom;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::boxed::FnBox;

use std::time::Duration;

pub struct NetTimer<T> {
    timer: Arc<RefCell<Timer<T>>>,
}

pub type NetTimeout = Timeout;

//定时器管理
pub struct NetTimers<T> {
    timers: HashMap<Atom, (Timer<T>, NetTimeout)>,
}

// unsafe impl Sync for NetTimers<TimerCallback> {}
// unsafe impl Send for NetTimers<TimerCallback> {}

//回调函数
pub type TimerCallback = Box<FnBox(Atom) + Send>;

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
            timers: HashMap::new(),
        }
    }
    //设置定时器，并设置回调,返回定时器ID
    pub fn set_timeout(&mut self, src: Atom, delay_from_now: Duration, state: TimerCallback) {
        let mut timer = Timer::default();
        let timeout = timer.set_timeout(delay_from_now, state);
        self.timers.insert(src, (timer, timeout));
    }
    //取消定时器，传入设置定时器返回的ID
    pub fn cancel_timeout(&mut self, src: Atom) -> Option<TimerCallback> {
        let v = match self.timers.get_mut(&src) {
            Some((timer, timeout)) => timer.cancel_timeout(timeout),
            None => None,
        };
        self.timers.remove(&src);
        v
    }
    //mio中轮训，到期开新进程触发回调
    pub fn poll(&mut self) {
        let mut vec = Vec::new();
        for (src, (timer, _timeout)) in self.timers.iter_mut() {
            match timer.poll() {
                Some(cb) => {
                    let src1 = src.clone();
                    thread::spawn(move || {
                        cb.call_box((src1,))
                    });
                    let src2 = src.clone();
                    vec.push(src2)
                }
                None => (),
            }
        }
        for src in vec {
            self.timers.remove(&src);
        }
    }
}

#[cfg(test)]
mod test {
    use pi_lib::atom::Atom;
    //use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use timer::NetTimers;

    #[test]
    pub fn timer_test() {
        let mut timers = NetTimers::new();
        timers.set_timeout(
            Atom::from("test"),
            Duration::from_millis(100),
            Box::new(|src: Atom| println!("timeout src: {}", *src)),
        );
        //下面由mio线程轮询
        {
            thread::sleep(Duration::from_millis(500));
            timers.poll();
        }
    }

    #[test]
    pub fn cancel_timeout_test() {
        let mut timers = NetTimers::new();
        let src = Atom::from("test2");
        timers.set_timeout(
            src.clone(),
            Duration::from_millis(200),
            Box::new(|_src: Atom| {
                assert!(false);
            }),
        );

        //取消定时器
        timers.cancel_timeout(src);
        //下面由mio线程轮询
        {
            thread::sleep(Duration::from_millis(500));
            timers.poll();
        }
    }
}
