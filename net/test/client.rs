use std::sync::{Arc, RwLock};
use std::io::Result;
use std::net::SocketAddr;

use net::{Config, NetManager, Protocol, Socket, Stream};

fn handle_close(stream_id: usize, reason: Result<()>) {
    println!(
        "client handle_close, stream_id = {}, reson = {:?}",
        stream_id, reason
    );
}

fn handle_recv(socket: Socket, stream: Arc<RwLock<Stream>>, begin: usize, end: usize) {
    let s = stream.clone();
    println!("client, request recv [{}, {}]", begin, end);

    let func = Box::new(move |data: Result<Arc<Vec<u8>>>| {
        let mut new_end = end + end - begin;
        if new_end > 1024 * 1024 {
            new_end = 1024 * 1024;
        }

        {
            let _s_borrow = &s.read().unwrap();

            let b = data.unwrap();
            for (i, &d) in b.iter().enumerate() {
                assert_eq!((begin + i) as u8, d);
            }

            let mut buf: Vec<u8> = vec![];
            for i in end..new_end {
                buf.push(i as u8);
            }
            socket.send(Arc::new(buf));

            println!("client recv, valid, begin = {}, end = {}", begin, end);
        }

        if end == 1024 * 1024 {
            socket.close(true);
            return;
        }

        handle_recv(socket, s, end, new_end);
    });
    
    // (RecvFn, Result<Arc<Vec<u8>>>)
    let r = stream.write().unwrap().recv(end - begin, func);
    if let Some((func, data)) = r {
        func(data);
    }
}

fn handle_connect(peer: Result<(Socket, Arc<RwLock<Stream>>)>, addr: Result<SocketAddr>) {
    println!("client handle_connect: addr = {:?}", addr.unwrap());

    let (socket, stream) = peer.unwrap();
    {
        let stream = &mut stream.write().unwrap();

        stream.set_close_callback(Box::new(|id, reason| handle_close(id, reason)));
        stream.set_send_buf_size(1024 * 1024);
        stream.set_recv_timeout(5 * 1000);
    }

    let mut buf: Vec<u8> = vec![];
    for i in 0..1024 {
        buf.push(i as u8);
    }
    socket.send(Arc::new(buf));

    handle_recv(socket, stream.clone(), 0, 1 * 1024);
}

pub fn start_client() -> NetManager {
    let mgr = NetManager::new();
    let config = Config {
        protocol: Protocol::TCP,
        server_addr: Some("127.0.0.1:1234".parse().unwrap()),
    };
    mgr.connect(config, Box::new(|peer, addr| handle_connect(peer, addr)));

    return mgr;
}
