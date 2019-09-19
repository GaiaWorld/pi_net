#![feature(test)]

extern crate test;

use test::Bencher;

use std::thread;
use std::time::Duration;
use std::sync::{Arc, RwLock as StdRwLock};

use dashmap::DashMap;
use parking_lot::RwLock;
use mqtt311::{TopicPath, Topic};

use hash::XHashMap;

use mqtt::util::PathTree;

#[bench]
fn bench_topic_tree_by_base(b: &mut Bencher) {
    let mut tree: PathTree<usize> = PathTree::empty();

    //初始化数据
    tree.insert(TopicPath::from(r"sport/tennis/#"), 100);
    tree.insert(TopicPath::from(r"sport/tennis/+"), 300);
    tree.insert(TopicPath::from(r"sport/tennis/player1/#"), 1000);
    tree.insert(TopicPath::from(r"sport/tennis/player1/+"), 3000);
    tree.insert(TopicPath::from(r"sport/tennis/+/#"), 10000);
    tree.insert(TopicPath::from(r"sport/tennis/+/abc"), 30000);
    tree.insert(TopicPath::from(r"sport/tennis/+/abc/#"), 100000);
    tree.insert(TopicPath::from(r"sport/tennis/+/abc/+"), 300000);
    tree.insert(TopicPath::from(r"sport/tennis/+/abc/+/#"), 1000000);
    tree.insert(TopicPath::from(r"sport/tennis/+/abc/+/abc"), 3000000);
    tree.insert(TopicPath::from(r"sport/tennis/player1/+/abc/+/+/abc/+/#"), 10000000);
    tree.insert(TopicPath::from(r"sport/tennis/+/+/abc/+/+/abc/+/+/+"), 30000000);

    b.iter(|| {
        if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/player1/abc/abc/abc/abc/abc/abc/abc/abc")) {
            assert_eq!(&vec[..], &[100, 1000, 10000, 100000, 1000000, 10000000, 30000000]);
        }
    });
}

#[bench]
fn bench_topic_tree_by_fix_two(b: &mut Bencher) {
    let mut tree: PathTree<usize> = PathTree::empty();

    //实始化数据
    let prefix = String::from(r"sport/");
    let suffix = "/#";
    for n in 0..10000 {
       tree.insert(TopicPath::from(prefix.clone() + &n.to_string() + suffix), n);
    }

    b.iter(move || {
        if let Some(vec) = tree.lookup(TopicPath::from(r"sport/9999/player1")) {
            assert_eq!(&vec[..], &[9999]);
        }
    });
}

#[bench]
fn bench_topic_tree_by_fix_three(b: &mut Bencher) {
    let mut tree: PathTree<usize> = PathTree::empty();

    //实始化数据
    let prefix = String::from(r"sport/tennis/");
    let suffix = "/#";
    for n in 0..10000 {
        tree.insert(TopicPath::from(prefix.clone() + &n.to_string() + suffix), n);
    }

    b.iter(move || {
        if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/9999/player1")) {
            assert_eq!(&vec[..], &[9999]);
        }
    });
}

#[bench]
fn bench_topic_tree_by_fix_five(b: &mut Bencher) {
    let mut tree: PathTree<usize> = PathTree::empty();

    //实始化数据
    let prefix = String::from(r"sport/tennis/");
    let suffix = "/+/abc/+";
    for n in 0..10000 {
        tree.insert(TopicPath::from(prefix.clone() + &n.to_string() + suffix), n);
    }

    b.iter(move || {
        if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/9999/player1/abc/abc")) {
            assert_eq!(&vec[..], &[9999]);
        }
    });
}

#[bench]
fn bench_topic_tree_by_fix_nine(b: &mut Bencher) {
    let mut tree: PathTree<usize> = PathTree::empty();

    //实始化数据
    let prefix = String::from(r"sport/tennis/");
    let suffix = "/+/abc/+/+/abc/+/+";
    for n in 0..10000 {
        tree.insert(TopicPath::from(prefix.clone() + &n.to_string() + suffix), n);
    }

    b.iter(move || {
        if let Some(vec) = tree.lookup(TopicPath::from(r"sport/tennis/9999/player1/abc/abc/abc/abc/abc/abc")) {
            assert_eq!(&vec[..], &[9999]);
        }
    });
}

#[bench]
fn bench_std_hash_map(b: &mut Bencher) {
    let mut map: Arc<StdRwLock<XHashMap<String, ()>>> = Arc::new(StdRwLock::new(XHashMap::default()));

    b.iter(move || {
        let map_ = map.clone();
        let map0 = map.clone();
        let map1 = map.clone();
        let map2 = map.clone();
        let map3 = map.clone();
        let map4 = map.clone();
        let map5 = map.clone();
        let map6 = map.clone();
        let map7 = map.clone();
        let map8 = map.clone();
        let map9 = map.clone();
        let map10 = map.clone();
        let map11 = map.clone();
        let map12 = map.clone();
        let map13 = map.clone();
        let map14 = map.clone();
        let map15 = map.clone();
        let map16 = map.clone();
        let map17 = map.clone();
        let map18 = map.clone();
        let map19 = map.clone();

        let p0 = thread::spawn(move || {
            for n in 0..100000 {
                map0.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p1 = thread::spawn(move || {
            for n in 100000..200000 {
                map1.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p2 = thread::spawn(move || {
            for n in 200000..300000 {
                map2.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p3 = thread::spawn(move || {
            for n in 300000..400000 {
                map3.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p4 = thread::spawn(move || {
            for n in 400000..500000 {
                map4.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p5 = thread::spawn(move || {
            for n in 500000..600000 {
                map5.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p6 = thread::spawn(move || {
            for n in 600000..700000 {
                map6.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p7 = thread::spawn(move || {
            for n in 700000..800000 {
                map7.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p8 = thread::spawn(move || {
            for n in 800000..900000 {
                map8.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p9 = thread::spawn(move || {
            for n in 900000..1000000 {
                map9.write().unwrap().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p10 = thread::spawn(move || {
            for n in 0..100000 {
                map10.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p11 = thread::spawn(move || {
            for n in 100000..200000 {
                map11.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p12 = thread::spawn(move || {
            for n in 200000..300000 {
                map12.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p13 = thread::spawn(move || {
            for n in 300000..400000 {
                map13.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p14 = thread::spawn(move || {
            for n in 400000..500000 {
                map14.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p15 = thread::spawn(move || {
            for n in 500000..600000 {
                map15.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p16 = thread::spawn(move || {
            for n in 600000..700000 {
                map16.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p17 = thread::spawn(move || {
            for n in 700000..800000 {
                map17.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p18 = thread::spawn(move || {
            for n in 800000..900000 {
                map18.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p19 = thread::spawn(move || {
            for n in 900000..1000000 {
                map19.read().unwrap().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        p0.join();
        p1.join();
        p2.join();
        p3.join();
        p4.join();
        p5.join();
        p6.join();
        p7.join();
        p8.join();
        p9.join();
        p10.join();
        p11.join();
        p12.join();
        p13.join();
        p14.join();
        p15.join();
        p16.join();
        p17.join();
        p18.join();
        p19.join();
        assert_eq!(map_.read().unwrap().len(), 1000000);
    });
}

#[bench]
fn bench_hash_map(b: &mut Bencher) {
    let mut map: Arc<RwLock<XHashMap<String, ()>>> = Arc::new(RwLock::new(XHashMap::default()));

    b.iter(move || {
        let map_ = map.clone();
        let map0 = map.clone();
        let map1 = map.clone();
        let map2 = map.clone();
        let map3 = map.clone();
        let map4 = map.clone();
        let map5 = map.clone();
        let map6 = map.clone();
        let map7 = map.clone();
        let map8 = map.clone();
        let map9 = map.clone();
        let map10 = map.clone();
        let map11 = map.clone();
        let map12 = map.clone();
        let map13 = map.clone();
        let map14 = map.clone();
        let map15 = map.clone();
        let map16 = map.clone();
        let map17 = map.clone();
        let map18 = map.clone();
        let map19 = map.clone();

        let p0 = thread::spawn(move || {
            for n in 0..100000 {
                map0.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p1 = thread::spawn(move || {
            for n in 100000..200000 {
                map1.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p2 = thread::spawn(move || {
            for n in 200000..300000 {
                map2.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p3 = thread::spawn(move || {
            for n in 300000..400000 {
                map3.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p4 = thread::spawn(move || {
            for n in 400000..500000 {
                map4.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p5 = thread::spawn(move || {
            for n in 500000..600000 {
                map5.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p6 = thread::spawn(move || {
            for n in 600000..700000 {
                map6.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p7 = thread::spawn(move || {
            for n in 700000..800000 {
                map7.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p8 = thread::spawn(move || {
            for n in 800000..900000 {
                map8.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p9 = thread::spawn(move || {
            for n in 900000..1000000 {
                map9.write().insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p10 = thread::spawn(move || {
            for n in 0..100000 {
                map10.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p11 = thread::spawn(move || {
            for n in 100000..200000 {
                map11.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p12 = thread::spawn(move || {
            for n in 200000..300000 {
                map12.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p13 = thread::spawn(move || {
            for n in 300000..400000 {
                map13.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p14 = thread::spawn(move || {
            for n in 400000..500000 {
                map14.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p15 = thread::spawn(move || {
            for n in 500000..600000 {
                map15.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p16 = thread::spawn(move || {
            for n in 600000..700000 {
                map16.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p17 = thread::spawn(move || {
            for n in 700000..800000 {
                map17.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p18 = thread::spawn(move || {
            for n in 800000..900000 {
                map18.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p19 = thread::spawn(move || {
            for n in 900000..1000000 {
                map19.read().get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        p0.join();
        p1.join();
        p2.join();
        p3.join();
        p4.join();
        p5.join();
        p6.join();
        p7.join();
        p8.join();
        p9.join();
        p10.join();
        p11.join();
        p12.join();
        p13.join();
        p14.join();
        p15.join();
        p16.join();
        p17.join();
        p18.join();
        p19.join();
        assert_eq!(map_.read().len(), 1000000);
    });
}

#[bench]
fn bench_im_hash_map(b: &mut Bencher) {
    let mut map: Arc<DashMap<String, ()>> = Arc::new(DashMap::default());

    b.iter(move || {
        let map_ = map.clone();
        let map0 = map.clone();
        let map1 = map.clone();
        let map2 = map.clone();
        let map3 = map.clone();
        let map4 = map.clone();
        let map5 = map.clone();
        let map6 = map.clone();
        let map7 = map.clone();
        let map8 = map.clone();
        let map9 = map.clone();
        let map10 = map.clone();
        let map11 = map.clone();
        let map12 = map.clone();
        let map13 = map.clone();
        let map14 = map.clone();
        let map15 = map.clone();
        let map16 = map.clone();
        let map17 = map.clone();
        let map18 = map.clone();
        let map19 = map.clone();

        let p0 = thread::spawn(move || {
            for n in 0..100000 {
                map0.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p1 = thread::spawn(move || {
            for n in 100000..200000 {
                map1.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p2 = thread::spawn(move || {
            for n in 200000..300000 {
                map2.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p3 = thread::spawn(move || {
            for n in 300000..400000 {
                map3.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p4 = thread::spawn(move || {
            for n in 400000..500000 {
                map4.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p5 = thread::spawn(move || {
            for n in 500000..600000 {
                map5.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p6 = thread::spawn(move || {
            for n in 600000..700000 {
                map6.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p7 = thread::spawn(move || {
            for n in 700000..800000 {
                map7.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p8 = thread::spawn(move || {
            for n in 800000..900000 {
                map8.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p9 = thread::spawn(move || {
            for n in 900000..1000000 {
                map9.insert(r"sport/tennis/player1/".to_string() + &n.to_string(), ());
            }
        });

        let p10 = thread::spawn(move || {
            for n in 0..100000 {
                map10.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p11 = thread::spawn(move || {
            for n in 100000..200000 {
                map11.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p12 = thread::spawn(move || {
            for n in 200000..300000 {
                map12.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p13 = thread::spawn(move || {
            for n in 300000..400000 {
                map13.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p14 = thread::spawn(move || {
            for n in 400000..500000 {
                map14.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p15 = thread::spawn(move || {
            for n in 500000..600000 {
                map15.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p16 = thread::spawn(move || {
            for n in 600000..700000 {
                map16.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p17 = thread::spawn(move || {
            for n in 700000..800000 {
                map17.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p18 = thread::spawn(move || {
            for n in 800000..900000 {
                map18.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        let p19 = thread::spawn(move || {
            for n in 900000..1000000 {
                map19.get(&(r"sport/tennis/player1/".to_string() + &n.to_string()));
            }
        });

        p0.join();
        p1.join();
        p2.join();
        p3.join();
        p4.join();
        p5.join();
        p6.join();
        p7.join();
        p8.join();
        p9.join();
        p10.join();
        p11.join();
        p12.join();
        p13.join();
        p14.join();
        p15.join();
        p16.join();
        p17.join();
        p18.join();
        p19.join();
        assert_eq!(map_.len(), 1000000);
    });
}