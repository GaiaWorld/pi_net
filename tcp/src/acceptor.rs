use std::thread;
use std::str::FromStr;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::io::{ErrorKind, Result, Error};
use std::net::{SocketAddr, IpAddr, Ipv6Addr};

use mio::{
    Events, Poll, PollOpt, Token, Ready,
    net::TcpListener
};
use slab::Slab;
use crossbeam_channel::{Sender, Receiver, unbounded};
use fnv::FnvBuildHasher;

use crate::driver::{DEFAULT_TCP_IP_V6, Socket, Stream, SocketAdapter, AcceptorCmd, SocketDriver};
use crate::util::pause;

/*
* Tcp连接接受器上下文
*/
struct AcceptorContext<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    listener:   TcpListener,        //Tcp连接监听器
    driver:     SocketDriver<S, A>, //Tcp连接驱动
}

/*
* Tcp连接接受器
*/
pub struct Acceptor<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    name:       String,                                             //Tcp连接接受器名称
    poll:       Poll,                                               //TCP连接事件轮询器
    contexts:   Slab<AcceptorContext<S, A>>,                        //Tcp连接上下文表
    listeners:  HashMap<SocketAddr, Token, FnvBuildHasher>,         //Tcp连接监听器表
    sender:     Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>,    //Tcp连接接受器的控制发送器
    receiver:   Receiver<Box<dyn FnOnce() -> AcceptorCmd + Send>>,  //Tcp连接接受器的控制接收器
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for Acceptor<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for Acceptor<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Acceptor<S, A> {
    //构建指定地址列表的Tcp连接接受器，并绑定地址列表
    pub fn bind(addrs: &[SocketAddr], driver: &SocketDriver<S, A>) -> Result<Self> {
        let mut len = addrs.len();
        let mut contexts = Slab::with_capacity(len);
        let mut listeners = HashMap::with_capacity_and_hasher(len, FnvBuildHasher::default());

        let poll = match Poll::new() {
            Err(e) => {
                return Err(e);
            },
            Ok(p) => {
                p
            }
        };

        let (sender, receiver) = unbounded();

        //绑定所有地址
        for addr in addrs {
            match TcpListener::bind(addr) {
                Err(e) => {
                    //绑定指定地址失败，则继续绑定其它地址
                    println!("!!!> Tcp Acceptor Bind Address Error, addr: {:?}, reason: {:?}", addr, e);
                    len -= 1;
                },
                Ok(listener) => {
                    if (addr.ip().ne(&IpAddr::V6(Ipv6Addr::from_str(DEFAULT_TCP_IP_V6).ok().unwrap()))) && addr.is_ipv6() {
                        //如果本地地址是ipv6，则设置当前监听器为ipv6独占
                        listener.set_only_v6(true);
                    }

                    //绑定指定地址成功，则为地址绑定连接上下文
                    let entry = contexts.vacant_entry();
                    let id = entry.key();
                    let token = Token(id);
                    listeners.insert(addr.clone(), token);

                    //注册绑定地址的监听器
                    if let Err(e) = poll.register(&listener, token, Ready::readable(), PollOpt::level()) {
                        println!("!!!> Tcp Acceptor Poll Register Error, addr: {:?}, reason: {:?}", addr, e);
                        len -= 1;
                        continue;
                    }

                    let mut copy = driver.clone();
                    copy.set_controller(sender.clone());
                    entry.insert(AcceptorContext {
                        listener,
                        driver: copy,
                    });
                },
            }
        }

        //获取接受器名称
        let name = listeners
            .keys()
            .map(|key| {
                key.port().to_string()
            })
            .collect::<Vec<String>>()
            .join(",");

        if len <= 0 {
            Err(Error::new(ErrorKind::AddrNotAvailable, "tcp bind address failed"))
        } else {
            Ok(Acceptor {
                name,
                poll,
                contexts,
                listeners,
                sender,
                receiver,
            })
        }
    }

    //获取接受器名称
    pub fn get_name(&self) -> String {
        self.name.clone()
    }

    //获取连接控制器
    pub fn get_controller(&self) -> Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>> {
        self.sender.clone()
    }

    //监听绑定的地址列表，返回监听控制器
    pub fn listen(self,
                  stack_size: usize,
                  event_size: usize,
                  timeout: Option<usize>) -> Result<()> {
        let acceptor = self;
        if let Err(e) = thread::Builder::new()
            .name("Tcp Acceptor ".to_string() + &acceptor.name)
            .stack_size(stack_size)
            .spawn(move || {
                listen_loop(acceptor, event_size, timeout);
            }) {
            return Err(e);
        }

        Ok(())
    }
}

//接受器监听连接事件循环
fn listen_loop<S: Socket + Stream, A: SocketAdapter<Connect = S>>(mut acceptor: Acceptor<S, A>,
                                                                  event_size: usize,
                                                                  timeout: Option<usize>) {
    let poll_timeout = if let Some(t) = timeout {
        Some(Duration::from_millis(t as u64))
    } else {
        None
    };

    let receiver = acceptor.receiver.clone();

    let acceptor_name = acceptor.name.clone();
    let mut events = Events::with_capacity(event_size);
    loop {
        if let Err(e) = acceptor.poll.poll(&mut events, poll_timeout) {
            println!("!!!> Tcp Acceptor Poll Failed, timeout: {:?}, ports: {:?}, reason: {:?}", poll_timeout, &acceptor_name, e);
            break;
        }

        match receiver.try_recv() {
            Err(_) => (), //忽略接收错误
            Ok(cmd) => {
                match cmd() {
                    AcceptorCmd::Continue => (), //继续监听连接事件
                    AcceptorCmd::Pause(time) => {
                        let now = Instant::now();
                        println!("===> Pause Tcp Acceptor Start, ports: {:?}", &acceptor_name);
                        thread::sleep(Duration::from_millis(time as u64));
                        println!("===> Pause Tcp Acceptor Finish, time: {:?}, ports: {:?}", Instant::now() - now, &acceptor_name);
                    },
                    AcceptorCmd::Close(reason) => {
                        println!("===> Close Tcp Acceptor Ok, ports: {:?}, reason: {:?}", &acceptor_name, reason);
                        break;
                    }
                }
            },
        }

        for event in &events {
            let token = event.token();

            let mut is_error = false;
            if let Some(context) = (&mut acceptor.contexts).get_mut(token.0) {
                if event.readiness().is_readable() {
                    match accept(&context.listener, token.0) {
                        Ok(socket) => {
                            //连接成功，则路由连接到连接池
                            if let Err(e) = context.driver.route(socket) {
                                println!("!!!> Tcp Acceptor Listen Failed, port: {:?}, token: {:?}, reason: {:?}", context.listener.local_addr(), token, e);
                            }
                        },
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            //连接将阻塞，则等待下次事件轮询时，完成连接，并继续处理下一个连接事件
                            println!("!!!> Tcp Acceptor Listen Would Block, port: {:?}, token: {:?}", context.listener.local_addr(), token);
                        },
                        Err(e) => {
                            //连接失败
                            is_error = true; //标记当前Tcp连接监听器出错
                            println!("!!!> Tcp Acceptor Listen Failed, port: {:?}, token: {:?}, reason: {:?}", context.listener.local_addr(), token, e);
                        },
                    }
                } else {
                    //无效的事件准备状态，则继续处理下一个连接事件
                    println!("!!!> Tcp Acceptor Listen Failed, port: {:?}, token: {:?}, reason: invalid ready", context.listener.local_addr(), token);
                }
            }

            //从Tcp连接接受器的上下文中，移除出错的Tcp连接监听器，并从轮询器中移除出错的Tcp连接监听器
            if is_error {
                is_error = false;
                let error_context = (&mut acceptor.contexts).remove(token.0);
                if let Ok(addr) = &error_context.listener.local_addr() {
                    if let Err(e) = acceptor.poll.deregister(&error_context.listener) {
                        //从轮询器中注销出错的Tcp连接监听器失败
                        println!("!!!> Tcp Acceptor Unregister Listen Failed, port: {:?}, token: {:?}, reason: {:?}", addr, token, e);
                    }

                    (&mut acceptor.listeners).remove(addr);
                }
            }
        }
    }
}

//接受连接请求
fn accept<S: Socket + Stream>(listener: &TcpListener, id: usize) -> Result<S> {
    loop {
        match listener.accept() {
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                //连接接受中断，则继续尝试接受连接
                pause();
                continue;
            },
            Err(e) => {
                //连接接受失败
                return Err(e);
            },
            Ok((stream, remote)) => {
                //连接接受成功，创建指定的Tcp Socket
                match listener.local_addr() {
                    Err(e) => {
                        return Err(e);
                    },
                    Ok(local) => {
                        return Ok(S::new(&local, &remote, Some(Token(id)), stream));
                    },
                }
            },
        }
    }
}