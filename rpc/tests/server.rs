
use std::io::Result;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use mqtt::data::Server;
use mqtt::server::ServerNode;
use mqtt::session::Session;
use net::{Config, NetManager, Protocol, Socket, Stream};
use pi_lib::handler::{Args, Env, Handler};
use rpc::server::RPCServer;
use rpc::traits::RPCServerTraits;

use pi_lib::atom::Atom;


struct Handle {
    _id: u8,
}

impl Handle {
    pub fn new() -> Self {
        Handle { _id: 1 }
    }
}

impl Handler for Handle {
    type A = u8;
    type B = Arc<Vec<u8>>;
    type C = ();
    type D = ();
    type E = ();
    type F = ();
    type G = ();
    type H = ();
    type HandleResult = ();
    fn handle(
        &self,
        session: Arc<dyn Env>,
        atom: Atom,
        args: Args<Self::A, Self::B, Self::C, Self::D, Self::E, Self::F, Self::G, Self::H>,
    ) -> Self::HandleResult {
        if let Args::TwoArgs(_vsn, msg) = args {
            println!(
                "topic_handle!!!!!!!atom:{}, msg:{:?}",
                *atom,
                String::from_utf8(msg.to_vec())
            );
        }
        unsafe {
            let session = Arc::from_raw(Arc::into_raw(session) as *const Session);
            session.respond(atom, String::from("ok!!!!").into_bytes());
        }
    }
}

fn handle_close(stream_id: usize, reason: Result<()>) {
    println!(
        "server handle_close, stream_id = {}, reason = {:?}",
        stream_id, reason
    );
}

fn handle_bind(
    peer: Result<(Socket, Arc<RwLock<Stream>>)>,
    addr: Result<SocketAddr>,
    mut mqtt: ServerNode,
    mut rpc: RPCServer,
) {
    let (socket, stream) = peer.unwrap();
    println!(
        "server handle_bind: addr = {:?}, socket:{}",
        addr.unwrap(),
        socket.socket
    );
    {
        let s = &mut stream.write().unwrap();

        // s.set_close_callback(Box::new(|id, reason| handle_close(id, reason)));
        //调用mqtt注册遗言
        mqtt.set_close_callback(s, Box::new(|id, reason| handle_close(id, reason)));
        s.set_send_buf_size(1024 * 1024);
        s.set_recv_timeout(500 * 1000);
    }

    mqtt.add_stream(socket, stream);
    
    let topic_handle = Handle::new();
    //通过rpc注册topic
    rpc.register(
        Atom::from(String::from("a/b/c").as_str()),
        true,
        Arc::new(topic_handle),
    ).is_ok();
    let topic_handle = Handle::new();
    //注册遗言
    rpc.register(
        Atom::from(String::from("$last_will").as_str()),
        true,
        Arc::new(topic_handle),
    ).is_ok();
}

pub fn start_server() -> NetManager {
    let mgr = NetManager::new();
    let config = Config {
        protocol: Protocol::TCP,
        addr: "127.0.0.1:1234".parse().unwrap(),
    };
    let mut mqtt = ServerNode::new();
    //rpc服务
    let mut rpc = RPCServer::new(mqtt.clone());
    mgr.bind(
        config,
        Box::new(move |peer, addr| handle_bind(peer, addr, mqtt.clone(), rpc.clone())),
    );
    return mgr;
}
