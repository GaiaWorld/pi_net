
use std::io::Result;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use net::{Config, NetManager, Protocol, Socket, Stream};
use mqtt::session::{Session};
use mqtt::server::{ServerNode};
use mqtt::data::Server;
use mqtt::handler::TopicHandle;
use rpc::server::{RPCServer};
use rpc::traits::RPCServerTraits;

use pi_lib::atom::Atom;


struct Handle {
    _id: u8,
}

impl Handle {
    pub fn new() -> Self {
        Handle{
            _id: 1,
        }
    }
}

impl TopicHandle for Handle {
    fn handle(&self, atom: Atom, _vsn: u8, session: Arc<Session>, msg: Arc<Vec<u8>>){
        let msg = &*msg;
        println!("topic_handle!!!!!!!atom:{}, msg:{:?}", *atom, String::from_utf8(msg.clone()));
        session.respond(atom, String::from("ok!!!!").into_bytes());
    }
}

fn handle_close(stream_id: usize, reason: Result<()>) {
    println!(
        "server handle_close, stream_id = {}, reason = {:?}",
        stream_id, reason
    );
}


fn handle_bind(peer: Result<(Socket, Arc<RwLock<Stream>>)>, addr: Result<SocketAddr>) {
    
    let (socket, stream) = peer.unwrap();
    println!("server handle_bind: addr = {:?}, socket:{}", addr.unwrap(), socket.socket);
    {
        let s = &mut stream.write().unwrap();

        s.set_close_callback(Box::new(|id, reason| handle_close(id, reason)));
        s.set_send_buf_size(1024 * 1024);
        s.set_recv_timeout(500 * 1000);
    }

    let mut server = ServerNode::new();
    server.add_stream(socket, stream);
    
    //rpc服务
    let mut rpc = RPCServer::new(server);
    let topic_handle = Handle::new();
    //通过rpc注册topic
    rpc.register(Atom::from(String::from("a/b/c").as_str()), true, Arc::new(topic_handle)).is_ok();
}

pub fn start_server() -> NetManager {
    let mgr = NetManager::new();
    let config = Config {
        protocol: Protocol::TCP,
        server_addr: None,
    };
    let addr = "127.0.0.1:1234".parse().unwrap();
    mgr.bind(addr, config, Box::new(move |peer, addr| handle_bind(peer, addr)));
    return mgr;
}
