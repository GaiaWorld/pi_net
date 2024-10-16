use std::str::FromStr;
use std::time::Instant;
use std::net::{ToSocketAddrs, SocketAddr};

use crossbeam_channel::bounded;
use pi_dns_resolver::{DNSResolver, init_global_dns_resolve_service, join_dns_resolver, async_resolve_ip, local_domain};

#[test]
fn test_local_system_resolver() {
    let mut addrs_iter = "www.google.com:443".to_socket_addrs().unwrap();
    while let Some(address) = addrs_iter.next() {
        println!("address: {:?}", address);
    }
}

#[test]
fn test_async_resolve_ip() {
    init_global_dns_resolve_service();
    join_dns_resolver("sys", DNSResolver::with_system().unwrap());

    let (sender, receiver) = bounded(1);
    let sender_copy = sender.clone();
    if let Err(e) = async_resolve_ip(None,
                     "translate.googleapis.com",
                     move |result| {
                         match result {
                             Err(e) => panic!("{:?}", e),
                             Ok(addrs) => {
                                 assert!(addrs.len() > 0);
                                 let _ = sender_copy.send(());
                             },
                         }
                     }) {
        panic!("{:?}", e);
    }

    if let Err(e) = receiver.recv() {
        panic!("{:?}", e);
    }

    let sender_copy = sender.clone();
    if let Err(e) = async_resolve_ip(Some("sys"),
                                     "translate.googleapis.com",
                                     move  |result| {
                                         match result {
                                             Err(e) => panic!("{:?}", e),
                                             Ok(addrs) => {
                                                 assert!(addrs.len() > 0);
                                                 let _ = sender_copy.send(());
                                             },
                                         }
                                     }) {
        panic!("{:?}", e);
    }

    if let Err(e) = receiver.recv() {
        panic!("{:?}", e);
    }
}

#[test]
fn test_local_domain() {
    println!("local: {:?}", local_domain());
}

#[test]
fn test_with_local_hosts_conf() {
    let resolver = DNSResolver::with_local_hosts_conf().unwrap();
    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("www.qq.com") {
        assert!(addrs.is_empty());
    } else {
        panic!("lookup_ip www.qq.com failed");
    }
    println!("lookup_ip www.qq.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("translate.googleapis.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip translate.googleapis.com failed");
    }
    println!("lookup_ip translate.googleapis.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("test.17youx.cn") {
        assert!(addrs.len() == 1);
        assert_eq!(addrs[0], SocketAddr::from_str("127.0.0.1:0").unwrap());
    } else {
        panic!("lookup_ip test.17youx.cn failed");
    }
    println!("lookup_ip test.17youx.cn, time: {:?}", now.elapsed());
}

#[test]
fn test_with_system() {
    let resolver = DNSResolver::with_system().unwrap();
    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("www.qq.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip www.qq.com failed");
    }
    println!("lookup_ip www.qq.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("translate.googleapis.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip translate.googleapis.com failed");
    }
    println!("lookup_ip translate.googleapis.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("test.17youx.cn") {
        assert!(addrs.len() == 1);
        assert_eq!(addrs[0], SocketAddr::from_str("127.0.0.1:0").unwrap());
    } else {
        panic!("lookup_ip test.17youx.cn failed");
    }
    println!("lookup_ip test.17youx.cn, time: {:?}", now.elapsed());
}

#[test]
fn test_with_system_conf() {
    let resolver = DNSResolver::with_system_conf().unwrap();
    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("www.qq.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip www.qq.com failed");
    }
    println!("lookup_ip www.qq.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("translate.googleapis.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip translate.googleapis.com failed");
    }
    println!("lookup_ip translate.googleapis.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("test.17youx.cn") {
        assert!(addrs.len() == 1);
        assert_eq!(addrs[0], SocketAddr::from_str("127.0.0.1:0").unwrap());
    } else {
        panic!("lookup_ip test.17youx.cn failed");
    }
    println!("lookup_ip test.17youx.cn, time: {:?}", now.elapsed());
}

#[test]
fn test_with_google() {
    let resolver = DNSResolver::with_google().unwrap();
    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("www.qq.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip www.qq.com failed");
    }
    println!("lookup_ip www.qq.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("translate.googleapis.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip translate.googleapis.com failed");
    }
    println!("lookup_ip translate.googleapis.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("test.17youx.cn") {
        assert!(addrs.len() == 1);
        assert_eq!(addrs[0], SocketAddr::from_str("127.0.0.1:0").unwrap());
    } else {
        panic!("lookup_ip test.17youx.cn failed");
    }
    println!("lookup_ip test.17youx.cn, time: {:?}", now.elapsed());
}

#[test]
fn test_with_cloudflare() {
    let resolver = DNSResolver::with_cloudflare().unwrap();
    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("www.qq.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip www.qq.com failed");
    }
    println!("lookup_ip www.qq.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("translate.googleapis.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip translate.googleapis.com failed");
    }
    println!("lookup_ip translate.googleapis.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("test.17youx.cn") {
        assert!(addrs.len() == 1);
        assert_eq!(addrs[0], SocketAddr::from_str("127.0.0.1:0").unwrap());
    } else {
        panic!("lookup_ip test.17youx.cn failed");
    }
    println!("lookup_ip test.17youx.cn, time: {:?}", now.elapsed());
}

#[test]
fn test_with_quad9() {
    let resolver = DNSResolver::with_quad9().unwrap();
    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("www.qq.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip www.qq.com failed");
    }
    println!("lookup_ip www.qq.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("translate.googleapis.com") {
        assert!(addrs.len() > 0);
    } else {
        panic!("lookup_ip translate.googleapis.com failed");
    }
    println!("lookup_ip translate.googleapis.com, time: {:?}", now.elapsed());

    let now = Instant::now();
    if let Ok(addrs) = resolver.lookup_ip("test.17youx.cn") {
        assert!(addrs.len() == 1);
        assert_eq!(addrs[0], SocketAddr::from_str("127.0.0.1:0").unwrap());
    } else {
        panic!("lookup_ip test.17youx.cn failed");
    }
    println!("lookup_ip test.17youx.cn, time: {:?}", now.elapsed());
}