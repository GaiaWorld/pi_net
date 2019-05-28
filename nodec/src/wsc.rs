use std::boxed::FnBox;
use std::time::{SystemTime, Duration};
use std::sync::{Arc, Mutex};
use std::io::{Error, Result, ErrorKind};
use std::sync::atomic::{AtomicUsize, Ordering, AtomicBool};

use wsc_lite::{Opcode, Message, NetworkStream, ClientBuilder, Client};

use worker::task::TaskType;
use worker::impls::cast_net_task;
use apm::counter::{GLOBAL_PREF_COLLECT, PrefCounter, PrefTimer};
use atom::Atom;
use timer::{FuncRuner, TIMER};

/*
* Mqtt异步访问任务类型
*/
const ASYNC_WSC_TASK_TYPE: TaskType = TaskType::Async(false);

/*
* Mqtt异步访问任务优先级
*/
const ASYNC_WSC_PRIORITY: usize = 100;

/*
* drop导致的websocket连接关闭
*/
const WSC_CLOSED_BY_DROP_CODE: u16 = 1001;

/*
* drop导致的websocket连接关闭原因
*/
const WSC_CLOSED_BY_DROP_REASON: &'static str = "wsc client droped";

/*
* 共享websocket客户端
*/
#[derive(Clone)]
pub struct SharedWSClient(Arc<Mutex<WSClient>>);

unsafe impl Send for SharedWSClient {}
unsafe impl Sync for SharedWSClient {}

impl SharedWSClient {
    //构建websocket客户端
    pub fn create(url: &str) -> Result<Self> {
        match ClientBuilder::new(url) {
            Err(e) => Err(Error::new(ErrorKind::AddrNotAvailable, e)),
            Ok(builder) => {
                Ok(SharedWSClient(Arc::new(Mutex::new(WSClient {
                    status: WSClientStatus::NotConnected,
                    url: url.to_string(),
                    builder: Some(builder),
                    client: None,
                }))))
            },
        }
    }

    //检查当前客户端状态
    pub fn status(&self) -> isize {
        self.0.lock().unwrap().status.to_status()
    }

    //获取当前客户端的对端url
    pub fn get_url(&self) -> String {
        self.0.lock().unwrap().url.clone()
    }

    //建立websocket连接
    pub fn connect(&self) -> Result<()> {
        let mut wsc = self.0.lock().unwrap();
        if let WSClientStatus::Connected(_) = wsc.status {
            return Err(Error::new(ErrorKind::AlreadyExists, "wsc connect already exists"));
        }

        if let Some(builder) = wsc.builder.take() {
            match builder.connect() {
                Err(e) => Err(Error::new(ErrorKind::ConnectionRefused, e.to_string())),
                Ok(client) => {
                    wsc.status = WSClientStatus::Connected(now_second());
                    wsc.client = Some(client);
                    Ok(())
                },
            }
        } else {
            Err(Error::new(ErrorKind::ConnectionAborted, "invalid wsc builder"))
        }
    }

    //发送websocket文本消息
    pub fn send_text(&self, text: &str) -> Result<()> {
        let mut wsc = self.0.lock().unwrap();

        if let WSClientStatus::Connected(_) = wsc.status {
            match Message::new(Opcode::Text, text) {
                Err(e) => Err(Error::new(ErrorKind::InvalidData, e)),
                Ok(msg) => {
                    if let Err(e) = wsc.client.as_mut().unwrap().send(msg) {
                        Err(Error::new(ErrorKind::InvalidData, e.to_string()))
                    } else {
                        Ok(())
                    }
                },
            }
        } else {
            Err(Error::new(ErrorKind::NotConnected, "wsc connect closed"))
        }
    }

    //发送websocket二进制消息
    pub fn send_bin(&self, bin: Vec<u8>) -> Result<()> {
        let mut wsc = self.0.lock().unwrap();

        if let WSClientStatus::Connected(_) = wsc.status {
            match Message::new(Opcode::Binary, bin) {
                Err(e) => Err(Error::new(ErrorKind::InvalidData, e)),
                Ok(msg) => {
                    if let Err(e) = wsc.client.as_mut().unwrap().send(msg) {
                        Err(Error::new(ErrorKind::InvalidData, e.to_string()))
                    } else {
                        Ok(())
                    }
                },
            }
        } else {
            Err(Error::new(ErrorKind::NotConnected, "wsc connect closed"))
        }
    }

    //发送websocket ping消息
    pub fn ping(&self, bin: Vec<u8>) -> Result<()> {
        let mut wsc = self.0.lock().unwrap();

        if let WSClientStatus::Connected(_) = wsc.status {
            if let Err(e) = wsc.client.as_mut().unwrap().send(Message::ping(bin)) {
                Err(Error::new(ErrorKind::InvalidData, e.to_string()))
            } else {
                Ok(())
            }
        } else {
            Err(Error::new(ErrorKind::NotConnected, "wsc connect closed"))
        }
    }

    //发送websocket close消息
    pub fn close(&self, code: u16, reason: &str) -> Result<()> {
        let mut wsc = self.0.lock().unwrap();

        if let WSClientStatus::Connected(_) = wsc.status {
            if let Err(e) = wsc.client.as_mut().unwrap().send(Message::close(Some((code, reason)))) {
                Err(Error::new(ErrorKind::InvalidData, e.to_string()))
            } else {
                wsc.status = WSClientStatus::Closed(now_second());
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    //异步接收websocket消息，通过抢占式控制，可以在超时后退出阻塞的接收线程
    pub fn receive(&self, timeout: Option<u32>, callback: Box<FnBox(Self, Result<Option<Vec<u8>>>)>) {
        let client = self.clone();
        let handle = client.0.lock().unwrap().client.as_ref().unwrap().is_running.clone();
        let handle_copy = handle.clone();
        let timer: Option<usize> = match timeout {
            None => None, //未设置超时时长，则忽略
            Some(time) => {
                //已设置超时时长
                Some(TIMER.set_timeout(FuncRuner::new(Box::new(move || {
                    //超时则抢占式退出当前接收
                    handle_copy.swap(false, Ordering::SeqCst);
                })), time))
            },
        };

        let func = Box::new(move |_lock| {
            let mut receive_result = Ok(None);
            match client.0.lock().unwrap().client.as_mut().unwrap().preemptive_receive() {
                Err(e) => {
                    receive_result = Err(Error::new(ErrorKind::TimedOut, e.to_string()));
                },
                Ok(reply) => {
                    if let Some(msg) = reply {
                        receive_result = Ok(Some(msg.data()[..].to_vec()));
                    } else {
                        receive_result = Ok(None);
                    }
                },
            }

            if let Some(t) = timer {
                //已设置接收超时定时器
                if receive_result.is_ok() {
                    //未超时，则取消接收超时定时器
                    TIMER.cancel(t);
                    handle.swap(true, Ordering::SeqCst); //取消定时器后，保证可以继续接收
                }
            };

            callback(client.clone(), receive_result);
        });
        cast_net_task(ASYNC_WSC_TASK_TYPE, ASYNC_WSC_PRIORITY, None, func, Atom::from("wsc receive task"));
    }
}

/*
* websocket客户端状态
*/
#[derive(Debug, Clone)]
enum WSClientStatus {
    NotConnected,
    Connected(isize),
    Closed(isize),
}

impl WSClientStatus {
    fn to_status(&self) -> isize {
        match self {
            WSClientStatus::Closed(_) => -1,
            WSClientStatus::NotConnected => 0,
            WSClientStatus::Connected(_) => 1,
        }
    }
}

/*
* websocket客户端
*/
struct WSClient {
    status:     WSClientStatus,                                                 //客户端状态
    url:        String,                                                         //对端url
    builder:    Option<ClientBuilder>,                                          //连接构造器
    client:     Option<Client<Box<dyn NetworkStream + Sync + Send + 'static>>>, //内部客户端
}

impl Drop for WSClient {
    fn drop(&mut self) {
        if let WSClientStatus::Connected(_) = &self.status {
            if let Some(client) = &mut self.client {
                self.status = WSClientStatus::Closed(now_second());
                client.send(Message::close(Some((WSC_CLOSED_BY_DROP_CODE, WSC_CLOSED_BY_DROP_REASON))));
            }
        }
    }
}

//获取当前时间的秒数
fn now_second() -> isize {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Err(e) => -1,
        Ok(n) => n.as_secs() as isize,
    }
}