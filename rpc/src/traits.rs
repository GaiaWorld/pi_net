
pub trait RPCClient {
    // 最终变为：$r，payload: params
    // fn request(func_name: Atom, params: &[u8], resp: Fn(&[u8]));
}

pub trait RPCServer {
    // $q 请求
    // $r 回应
    // 最终变为：$q，payload; 返回值, $r/$id
    // 
    // fn register_sync(func_name: Atom, fn: Box<Fn(&[u8]-> Vec<u8>>));
    
    // fn register_async(func_name: Atom, fn: Box<Fn(&[u8], Arc<Fn(&[u8])>>));
    
    // fn unregister(func_name: Atom);
}


// sub($q/$id, function () {
//     var func ();
//     pub();
// })

// sub($q/$id, function () {
//     bf=Arc
//     var func (, bf);

// })
