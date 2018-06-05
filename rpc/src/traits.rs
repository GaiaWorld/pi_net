use mqtt::{ServerNode};
use string_cache::DefaultAtom as Atom;
use pi_vm::pi_vm_impl::TopicHandler;

use std::io::{Result};

pub trait RPCClient {
    // 最终变为：$r，payload: params
    // fn request(func_name: Atom, params: &[u8], resp: Fn(&[u8]));
    
    //订阅$r/#
}

pub trait RPCServer {
    // $q 请求
    // $r 回应
    // 最终变为：$q，payload; 返回值, $r/$id
    // 
    fn register_sync(topic: Atom, func: TopicHandler) -> Result<()>;
    
    // fn register_async(func_name: Atom, fn: Box<Fn(&[u8], Arc<Fn(&[u8])>>));
    
    fn unregister(topic: Atom) -> Result<()>;
}


// sub($q/$id, function () {
//     var func ();
//     pub();
// })

// sub($q/$id, function () {
//     bf=Arc
//     var func (, bf);

// })
