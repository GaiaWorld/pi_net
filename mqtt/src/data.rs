use std::boxed::FnBox;
use std::io::Result;
use std::sync::{Arc, RwLock};

use mqtt3::{LastWill, QoS};

use net::{Socket, Stream};
use server::{ClientStub};

use string_cache::DefaultAtom as Atom;

pub type ClientCallback = Box<FnBox(Result<()>)>;

// mqtt客户端接口
pub trait Client {
    // 设置网络数据
    fn set_stream(&mut self, socket: Socket, stream: Arc<RwLock<Stream>>);

    // 创建mqtt连接
    fn connect(
        &mut self,
        keep_alive: u16,
        will: Option<LastWill>,
        close_func: Option<ClientCallback>,
        connect_func: Option<ClientCallback>,
    );

    // 订阅主题，同一个主题只能订阅一次
    fn subscribe(
        &mut self,
        topics: Vec<(String, QoS)>,
        resp_func: Option<ClientCallback>,
    ) -> Result<()>;

    // 取消订阅主题
    fn unsubscribe(&mut self, topics: Vec<String>, resp_func: Option<ClientCallback>)
        -> Result<()>;

    // 断开服务器链接
    fn disconnect(&mut self) -> Result<()>;

    // 给服务器发送数据
    fn publish(&mut self, retain: bool, qos: QoS, topic: Atom, payload: Vec<u8>) -> Result<()>;

    fn set_topic_handler(&mut self, name: Atom, handler: Box<Fn(Result<&[u8]>)>) -> Result<()>;

    fn remove_topic_handler(&mut self, name: Atom) -> Result<()>;

    fn add_attribute(&mut self, name: Atom, value: Vec<u8>);

    fn remove_attribute(&mut self, name: Atom);

    fn get_attribute(&mut self, name: Atom) -> Option<Arc<Vec<u8>>>;
}

// mqtt服务器接口
pub trait Server {
    // 设置网络数据
    fn add_stream(&mut self, socket: Socket, stream: Arc<RwLock<Stream>>);

    // 发布
    fn publish(&mut self, retain: bool, qos: QoS, topic: Atom, payload: Vec<u8>) -> Result<()>;

    // 关闭
    fn shutdown(&mut self) -> Result<()>;

    fn set_topic_meta(
        &mut self,
        name: Atom,
        can_publish: bool,
        can_subscribe: bool,
        only_one_key: Option<Atom>,
        handler: Box<Fn(ClientStub, Result<Arc<Vec<u8>>>)>,
    ) -> Result<()>;
    fn unset_topic_meta(
        &mut self,
        name: Atom,
    ) -> Result<()>;
}
