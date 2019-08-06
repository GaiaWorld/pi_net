use std::sync::Arc;
use std::net::SocketAddr;
use std::marker::PhantomData;
use std::collections::HashMap;
use std::result::Result as GenResult;
use std::io::{ErrorKind, Result, Error};

use rustls::RootCertStore;
use crossbeam_channel::{Sender, unbounded};
use fnv::FnvBuildHasher;

use apm::common::SysStat;

use driver::{Socket, Stream, SocketAdapter, AcceptorCmd, SocketOption, SocketConfig, SocketDriver};
use acceptor::Acceptor;
use connect_pool::TcpSocketPool;
use slab::VacantEntry;

/*
* Tcp端口适配器
*/
pub struct PortsAdapter<S: Socket> {
    ports:  HashMap<u16, Arc<SocketAdapter<Connect = S>>, FnvBuildHasher>,
}

impl<S: Socket> SocketAdapter for PortsAdapter<S> {
    type Connect = S;

    fn connected(&self, result: GenResult<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, e)) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.connected(Err((socket, e)));
                }
            },
            Ok(socket) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.connected(Ok(socket));
                }
            },
        }
    }

    fn readed(&self, result: GenResult<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, e)) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.readed(Err((socket, e)));
                }
            },
            Ok(socket) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.readed(Ok(socket));
                }
            },
        }
    }

    fn writed(&self, result: GenResult<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, e)) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.writed(Err((socket, e)));
                }
            },
            Ok(socket) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.writed(Ok(socket));
                }
            },
        }
    }

    fn closed(&self, result: GenResult<&mut Self::Connect, (&mut Self::Connect, Error)>) {
        match result {
            Err((socket, e)) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.closed(Err((socket, e)));
                }
            },
            Ok(socket) => {
                if let Some(adapter) = self.ports.get(&socket.get_local().port()) {
                    adapter.closed(Ok(socket));
                }
            },
        }
    }
}

impl<S: Socket> PortsAdapter<S> {
    //构建一个Tcp端口适配器
    pub fn new() -> Self {
        PortsAdapter {
            ports: HashMap::with_hasher(FnvBuildHasher::default()),
        }
    }

    //获取所有端口
    pub fn ports(&self) -> Vec<u16> {
        self.ports.keys().map(|port| {
            port.clone()
        }).collect::<Vec<u16>>()
    }

    //获取指定端口的事件适配器
    pub fn get_adapter(&self, port: &u16) -> &SocketAdapter<Connect = S> {
        self.ports.get(port).unwrap().as_ref()
    }

    //设置指定端口的事件适配器
    pub fn set_adapter(&mut self, port: u16, adapter: Arc<SocketAdapter<Connect = S>>) {
        self.ports.insert(port, adapter);
    }
}

/*
* Tcp连接监听器
*/
pub struct SocketListener<S: Socket + Stream, A: SocketAdapter<Connect = S>> {
    marker0: PhantomData<S>,
    marker1: PhantomData<A>,
}

impl<S: Socket + Stream, A: SocketAdapter<Connect = S>> SocketListener<S, A> {
    //绑定指定配置的Tcp连接监听器
    pub fn bind(adapter: A,             //连接事件适配置器
                config: SocketConfig,   //连接配置
                init_cap: usize,        //连接池初始容量
                stack_size: usize,      //线程堆栈大小
                event_size: usize,      //同时处理的事件数
                timeout: Option<usize>  //事件轮询超时时长
    ) -> Result<SocketDriver<S, A>> {
        let addrs = config.addrs();
        let (sender, receiver) = unbounded();
        let binds: Vec<(SocketAddr, Sender<S>)> = addrs.iter().map(|addr| {
            (addr.clone(), sender.clone())
        }).collect();

        let acceptor;
        let sys = SysStat::new();
        let processor = sys.processor_count();
        let mut pools = Vec::with_capacity(processor);
        let mut driver = SocketDriver::new(&binds[..], adapter);
        match Acceptor::bind(&addrs[..], &driver) {
            Err(e) => {
                return Err(e);
            },
            Ok(a) => {
                //创建当前系统cpu核心数的连接池
                acceptor = a;
                for _ in 0..processor {
                    match TcpSocketPool::with_capacity(acceptor.get_name(),
                                                       receiver.clone(),
                                                       config.clone(),
                                                       init_cap) {
                        Err(e) => {
                            return Err(e);
                        },
                        Ok(pool) => {
                            pools.push(pool);
                        },
                    }
                }
            },
        }

        //设置连接驱动的控制器，为所有连接池设置连接驱动，并启动所有连接池
        driver.set_controller(acceptor.get_controller());
        for pool in pools {
            if let Err(e) = pool.run(&driver, stack_size, event_size, timeout) {
                //启动连接池失败
                return Err(e);
            }
        }

        //启动接受器的监听
        if let Err(e) = acceptor.listen(stack_size, event_size, timeout) {
            //启动接受器失败
            return Err(e);
        }

        Ok(driver)
    }
}
