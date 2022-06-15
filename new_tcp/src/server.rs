use std::mem;
use std::task::Waker;
use std::cell::RefCell;
use std::rc::{Weak, Rc};
use std::future::Future;
use std::net::SocketAddr;
use std::any::{Any, TypeId};
use std::io::{Result, Error};
use std::marker::PhantomData;
use std::result::Result as GenResult;
use std::collections::{hash_map::Entry,  HashMap};

use mio::Token;
use fnv::FnvBuildHasher;
use crossbeam_channel::{Sender, unbounded};
use futures::future::BoxFuture;
use num_cpus;

use pi_async::rt::{AsyncRuntimeBuilder,
                   single_thread::SingleTaskRuntime};

/*
* Tcp连接监听器
*/
pub struct SocketListener<S: Socket + Stream, F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>> {
    marker: PhantomData<(S, F)>,
}

impl<S, F> SocketListener<S, F>
    where S: Socket + Stream,
          F: SocketAdapterFactory<Connect = S, Adapter = PortsAdapter<S>>, {
    //绑定指定配置的Tcp连接监听器
    pub fn bind(factory: F,                 //Tcp端口适配器工厂
                buffer: WriteBufferPool,    //写缓冲池
                config: SocketConfig,       //连接配置
                init_cap: usize,            //连接池初始容量
                stack_size: usize,          //线程堆栈大小
                event_size: usize,          //同时处理的事件数
                timeout: Option<usize>      //事件轮询超时时长
    ) -> Result<SocketDriver<S, PortsAdapter<S>>> {
        let addrs = config.addrs();
        let (sender, receiver) = unbounded();
        let binds: Vec<(SocketAddr, Sender<S>)> = addrs.iter().map(|(addr, _tls_cfg)| {
            (addr.clone(), sender.clone())
        }).collect();

        let acceptor;
        let processor = num_cpus::get();
        let mut pools = Vec::with_capacity(processor);
        let mut driver = SocketDriver::new(&binds[..]);
        match Acceptor::bind(&addrs[..], &driver) {
            Err(e) => {
                return Err(e);
            },
            Ok(a) => {
                //创建当前系统cpu核心数的连接池，共用一个写缓冲池
                acceptor = a;
                for index in 0..processor {
                    match TcpSocketPool::with_capacity(index as u8,
                                                       acceptor.get_name(),
                                                       receiver.clone(),
                                                       config.clone(),
                                                       buffer.clone(),
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

        driver.set_controller(acceptor.get_controller()); //设置连接驱动的控制器
        //为所有连接池，设置不同端口适配器的连接驱动，并启动所有连接池
        for pool in pools {
            let mut driver_clone = driver.clone();
            driver_clone.set_adapter(factory.get_instance()); //设置连接驱动的端口适配器
            if let Err(e) = pool.run(driver_clone, stack_size, event_size, timeout) {
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

    //绑定指定配置的Tcp连接监听器，需要指定处理器数量
    pub fn bind_with_processor(factory: F,                 //Tcp端口适配器工厂
                               buffer: WriteBufferPool,    //写缓冲池
                               config: SocketConfig,       //连接配置
                               mut processor: usize,       //处理器数量
                               init_cap: usize,            //连接池初始容量
                               stack_size: usize,          //线程堆栈大小
                               event_size: usize,          //同时处理的事件数
                               timeout: Option<usize>      //事件轮询超时时长
    ) -> Result<SocketDriver<S, PortsAdapter<S>>> {
        if processor == 0 {
            processor = 1; //默认的处理器数量
        }

        let addrs = config.addrs();
        let (sender, receiver) = unbounded();
        let binds: Vec<(SocketAddr, Sender<S>)> = addrs.iter().map(|(addr, _tls_cfg)| {
            (addr.clone(), sender.clone())
        }).collect();

        let acceptor;
        let mut pools = Vec::with_capacity(processor);
        let mut driver = SocketDriver::new(&binds[..]);
        match Acceptor::bind(&addrs[..], &driver) {
            Err(e) => {
                return Err(e);
            },
            Ok(a) => {
                //创建当前系统cpu核心数的连接池，共用一个写缓冲池
                acceptor = a;
                for index in 0..processor {
                    match TcpSocketPool::with_capacity(index as u8,
                                                       acceptor.get_name(),
                                                       receiver.clone(),
                                                       config.clone(),
                                                       buffer.clone(),
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

        driver.set_controller(acceptor.get_controller()); //设置连接驱动的控制器
        //为所有连接池，设置不同端口适配器的连接驱动，并启动所有连接池
        for pool in pools {
            let mut driver_clone = driver.clone();
            driver_clone.set_adapter(factory.get_instance()); //设置连接驱动的端口适配器
            if let Err(e) = pool.run(driver_clone, stack_size, event_size, timeout) {
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