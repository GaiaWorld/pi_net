use mio_extras::timer::{self, Timer, Timeout};

use std::sync::{Arc};
use std::cell::RefCell;

use std::time::{Duration};

pub struct NetTimer<T> {
    timer: Arc<RefCell<Timer<T>>>
}

pub type NetTimeout = Timeout;

impl <T> NetTimer<T> {
    pub fn new() -> NetTimer<T> {
        NetTimer {
            timer: Arc::new(RefCell::new(Timer::default()))
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