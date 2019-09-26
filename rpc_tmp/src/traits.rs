use handler::Handler;

use atom::Atom;

use std::io::Result;
use std::sync::Arc;
use std::net::SocketAddr;

pub trait RPCClientTraits {
    // 最终变为：$r，payload: params
    fn request(
        &self,
        topic: Atom,
        msg: Vec<u8>,
        resp: Box<Fn(Result<Arc<Vec<u8>>>)>,
        timeout: u8,
    );

    //订阅$r/#
}

pub trait RPCServerTraits {
    // $q 请求
    // $r 回应
    // 最终变为：$q，payload; 返回值, $r/$id
    //
    fn register(
        &self,
        topic: Atom,
        sync: bool,
        handle: Arc<
            Handler<
                A = u8,
                B = Option<SocketAddr>,
                C = Arc<Vec<u8>>,
                D = (),
                E = (),
                F = (),
                G = (),
                H = (),
                HandleResult = (),
            >,
        >,
    ) -> Result<()>;

    // fn register_async(func_name: Atom, fn: Box<Fn(&[u8], Arc<Fn(&[u8])>>));

    fn unregister(&self, topic: Atom) -> Result<()>;
}

// sub($q/$id, function () {
//     var func ();
//     pub();
// })

// sub($q/$id, function () {
//     bf=Arc
//     var func (, bf);

// })
