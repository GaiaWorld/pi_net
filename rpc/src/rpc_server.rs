use std::sync::{Arc, RwLock};
use std::io::{Error, Result};

use string_cache::DefaultAtom as Atom;
use fnv::FnvHashMap;

use mqtt::{ServerNode as MQTT, ClientStub};

use handler::TopicHandler;

use traits::RPCServer;

pub struct Server(MQTT);

pub struct Session(Arc<ClientStub>);

impl Server {
    pub fn new(mqtt: MQTT) -> Self {
        Server(mqtt)
    }
}

//会话
impl Session {
    pub fn new(client: ClientStub) -> Self {
        Session(Arc::new(client))
    }

    //发布消息
    pub fn send(topic: Atom, msg: Vec<u8>) -> Result<()> {
        Ok(())
    }
    //获取会话表数据
    pub fn get_attr(key: Atom) -> Result<Arc<Vec<u8>>> {
        Ok(Arc::new(vec![]))
    }
    //设置会话表
    pub fn set_attr(key: Atom, Value: Option<Arc<[u8]>>) -> Result<()> {
        Ok(())
    }
}

impl RPCServer for Server {
    fn register_sync(topic: Atom, func: TopicHandler) -> Result<()> {
        let handle = move | arg: (ClientStub, Result<[u8]>)| {
            let mut session = Session::new(arg.0);
            //设置客户端对应的topic
            //自动为客户端订阅topic
            //分析数据(解压、uid)
            // func.handle(topic, session, r);
        };

        Ok(())
    }

    fn unregister(topic: Atom) -> Result<()> {
        Ok(())
    }
}

