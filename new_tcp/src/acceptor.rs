use std::thread;
use std::str::FromStr;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::io::{ErrorKind, Result, Error};
use std::net::{SocketAddr, IpAddr, Ipv6Addr};

use mio::{
    Events, Poll, Token, Interest,
    net::TcpListener
};
use crossbeam_channel::{Sender, Receiver, unbounded};
use log::{info, warn};

use pi_async::rt::{serial::{AsyncRuntime, AsyncRuntimeBuilder},
                   serial_worker_thread::WorkerRuntime};
use pi_hash::XHashMap;
use pi_slotmap::{Key, DefaultKey, KeyData, SlotMap};

use crate::{DEFAULT_TCP_IP_V6, SocketAdapter, Socket, Stream, AcceptorCmd, SocketDriver};
use crate::utils::TlsConfig;

/*
* Tcp连接接受器上下文
*/
struct AcceptorContext<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    listener:   TcpListener,        //Tcp连接监听器
    driver:     SocketDriver<S, A>, //Tcp连接驱动
    tls_cfg:    TlsConfig,          //传输层安全协议配置
}

/*
* Tcp连接接受器
*/
pub struct Acceptor<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    name:       String,                                             //Tcp连接接受器名称
    poll:       Poll,                                               //TCP连接事件轮询器
    contexts:   SlotMap<DefaultKey, Option<AcceptorContext<S, A>>>, //Tcp连接上下文表
    listeners:  XHashMap<SocketAddr, Token>,                        //Tcp连接监听器表
    sender:     Sender<Box<dyn FnOnce() -> AcceptorCmd + Send>>,    //Tcp连接接受器的控制发送器
    receiver:   Receiver<Box<dyn FnOnce() -> AcceptorCmd + Send>>,  //Tcp连接接受器的控制接收器
}

unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Send for Acceptor<S, A> {}
unsafe impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Sync for Acceptor<S, A> {}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> Acceptor<S, A> {
    //构建指定地址列表的Tcp连接接受器，并绑定地址列表
    pub fn bind(addrs: &[(SocketAddr, TlsConfig)],
                driver: &SocketDriver<S, A>) -> Result<Self> {
        let mut len = addrs.len();
        let mut contexts = SlotMap::with_capacity(len);
        let mut listeners = XHashMap::default();

        let poll = Poll::new()?;
        let (sender, receiver) = unbounded();

        //绑定所有地址
        for (addr, tls_cfg) in addrs {
            match TcpListener::bind(addr.clone()) {
                Err(e) => {
                    //绑定指定地址失败，则立即抛出异常
                    panic!("Tcp acceptor bind address failed, addr: {:?}, reason: {:?}",
                           addr,
                           e);
                },
                Ok(mut listener) => {
                    //绑定指定地址成功，则为地址绑定连接上下文
                    let id = contexts.insert(None);
                    let token = Token(id.data().as_ffi() as usize);
                    listeners.insert(addr.clone(), token);

                    //注册绑定地址的监听器
                    if let Err(e) = poll
                        .registry()
                        .register(&mut listener, token, Interest::READABLE) {
                        warn!("Tcp acceptor poll register failed, addr: {:?}, reason: {:?}",
                            addr,
                            e);
                        len -= 1;
                        continue;
                    }

                    let mut copy = driver.clone();
                    copy.set_controller(sender.clone());
                    contexts[id] = Some(AcceptorContext {
                        listener,
                        driver: copy,
                        tls_cfg: tls_cfg.clone(),
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
                  recv_frame_buf_size: usize,
                  readed_read_size_limit: usize,
                  readed_write_size_limit: usize,
                  timeout: Option<usize>) -> Result<()> {
        let worker_name = "Tcp Acceptor ".to_string() + &self.name;
        let rt = AsyncRuntimeBuilder::default_worker_thread(Some(worker_name.as_str()),
                                                            Some(stack_size),
                                                            Some(1),
                                                            Some(Some(1)));

        let acceptor = self;
        let rt_copy = rt.clone();
        rt.spawn(rt.alloc(), async move {
            listen_loop(rt_copy,
                        acceptor,
                        event_size,
                        recv_frame_buf_size,
                        readed_read_size_limit,
                        readed_write_size_limit,
                        timeout).await;
        });

        Ok(())
    }
}

//接受器监听连接事件循环
#[inline]
async fn listen_loop<S: Socket + Stream, A: SocketAdapter<Connect = S>>(rt: WorkerRuntime<()>,
                                                                        mut acceptor: Acceptor<S, A>,
                                                                        event_size: usize,
                                                                        recv_frame_buf_size: usize,
                                                                        readed_read_size_limit: usize,
                                                                        readed_write_size_limit: usize,
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
            warn!("Tcp acceptor poll failed, timeout: {:?}, ports: {:?}, reason: {:?}",
                poll_timeout,
                &acceptor_name,
                e);
            break;
        }

        match receiver.try_recv() {
            Err(_) => (), //忽略接收错误
            Ok(cmd) => {
                match cmd() {
                    AcceptorCmd::Continue => (), //继续监听连接事件
                    AcceptorCmd::Pause(time) => {
                        let now = Instant::now();
                        info!("Pause tcp acceptor start, ports: {:?}", &acceptor_name);
                        thread::sleep(Duration::from_millis(time as u64));
                        info!("Pause tcp acceptor finish, time: {:?}, ports: {:?}",
                            Instant::now() - now,
                            &acceptor_name);
                    },
                    AcceptorCmd::Close(reason) => {
                        info!("Close tcp acceptor ok, ports: {:?}, reason: {:?}",
                            &acceptor_name, reason);
                        break;
                    }
                }
            },
        }

        for event in &events {
            let token = event.token();

            let mut is_error = false;
            if let Some(Some(context)) = (&mut acceptor.contexts)
                .get_mut(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                if event.is_readable() {
                    match accept::<S>(&rt,
                                 &context.listener,
                                 token.0,
                                 recv_frame_buf_size,
                                 readed_read_size_limit,
                                 readed_write_size_limit,
                                 context.tls_cfg.clone()).await {
                        Ok(socket) => {
                            //连接成功，则路由连接到连接池
                            if let Err(e) = context.driver.route(socket) {
                                //严重错误，则输出日志后，立即退出进程
                                warn!("Tcp acceptor listen failed, port: {:?}, token: {:?}, reason: {:?}",
                                    context.listener.local_addr(),
                                    token,
                                    e);
                                std::process::exit(1);
                            }

                            //连接成功后，重置当前端口的监听器感兴趣的事件
                            acceptor
                                .poll
                                .registry()
                                .reregister(&mut context.listener,
                                            token.clone(),
                                            Interest::READABLE);
                        },
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            //连接将阻塞，则等待下次事件轮询时，完成连接，并继续处理下一个连接事件
                            warn!("Tcp acceptor listen would block, port: {:?}, token: {:?}",
                                context.listener.local_addr(),
                                token);
                        },
                        Err(e) => {
                            //连接失败
                            is_error = true; //标记当前Tcp连接监听器出错
                            warn!("Tcp acceptor listen failed, port: {:?}, token: {:?}, reason: {:?}",
                                context.listener.local_addr(),
                                token,
                                e);
                        },
                    }
                } else {
                    //无效的事件准备状态，则继续处理下一个连接事件
                    warn!("Tcp acceptor listen failed, port: {:?}, token: {:?}, reason: invalid ready",
                        context.listener.local_addr(),
                        token);
                }
            }

            //从Tcp连接接受器的上下文中，移除出错的Tcp连接监听器，并从轮询器中移除出错的Tcp连接监听器
            if is_error {
                is_error = false;
                if let Some(Some(mut error_context)) = (&mut acceptor.contexts)
                    .remove(DefaultKey::from(KeyData::from_ffi(token.0 as u64))) {
                    if let Ok(addr) = &error_context.listener.local_addr() {
                        if let Err(e) = acceptor
                            .poll
                            .registry()
                            .deregister(&mut error_context.listener) {
                            //从轮询器中注销出错的Tcp连接监听器失败
                            warn!("Tcp acceptor unregister listen failed, port: {:?}, token: {:?}, reason: {:?}",
                                addr,
                                token,
                                e);
                        }

                        (&mut acceptor.listeners).remove(addr);
                    }
                }
            }
        }
    }
}

//接受连接请求
async fn accept<S: Socket + Stream>(rt: &WorkerRuntime<()>,
                                    listener: &TcpListener,
                                    id: usize,
                                    recv_frame_buf_size: usize,
                                    readed_read_size_limit: usize,
                                    readed_write_size_limit: usize,
                                    tls_cfg: TlsConfig) -> Result<S> {
    loop {
        match listener.accept() {
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                //连接接受中断，则休眠后继续尝试
                rt.timeout(1).await;
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
                        return Ok(S::new(&local,
                                         &remote,
                                         Some(Token(id)),
                                         stream,
                                         recv_frame_buf_size,
                                         readed_read_size_limit,
                                         readed_write_size_limit,
                                         tls_cfg));
                    },
                }
            },
        }
    }
}