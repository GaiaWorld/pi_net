use std::collections::{hash_map::Entry, BTreeMap};

use mqtt311::{Topic, TopicPath};

use atom::Atom;
use hash::XHashMap;

/*
* 路径节点
*/
#[derive(Clone)]
struct PathNode<V: Ord + Clone> {
    topic:      Topic,                          //节点主题名
    level:      usize,                          //节点级别
    unmatch:    BTreeMap<V, ()>,                //待匹配
    childs:     XHashMap<Atom, PathNode<V>>,    //子节点
}

unsafe impl<V: Ord + Clone> Send for PathNode<V> {}
unsafe impl<V: Ord + Clone> Sync for PathNode<V> {}

impl<V: Ord + Clone> PathNode<V> {
    //构建一个指定节点主题名的路径节点
    pub fn new(topic: Topic, level: usize) -> Self {
        PathNode {
            topic,
            level,
            unmatch: BTreeMap::new(),
            childs: XHashMap::default(),
        }
    }
}

/*
* 路径匹配树
*/
#[derive(Clone)]
pub struct PathTree<V: Ord + Clone> {
    root:   PathNode<V>,    //根节点
    size:   usize,          //值数量
}

impl<V: Ord + Clone> PathTree<V> {
    //构建空路径匹配树
    pub fn empty() -> Self {
        PathTree {
            root: PathNode::new(Topic::Blank, 0),
            size: 0,
        }
    }

    //是否为空路径匹配树
    pub fn is_empty(&self) -> bool {
        self.root.unmatch.is_empty() && self.root.childs.is_empty()
    }

    //获取值数量
    pub fn len(&self) -> usize {
        self.size
    }

    //查找指定路径匹配的值
    pub fn lookup(&self, path: TopicPath) -> Option<Vec<V>> {
        if path.wildcards {
            //路径有通配符，则返回空
            return None;
        }

        let mut map = BTreeMap::new(); //用于记录并去重
        lookup(Some(&self.root), &path, path.len(), 0, &mut map);

        Some(map.keys().cloned().collect())
    }

    //插入一个路径对应的值
    pub fn insert(&mut self, path: TopicPath, value: V) -> Result<(), (TopicPath, V)> {
        if !path.wildcards {
            //路径没有通配符，则返回路径和值
            return Err((path, value));
        }

        let mut key;
        let mut string: String;
        let mut node = &mut self.root;
        for index in 0..path.len() {
            if let Some(topic) = path.get(index) {
                //获取指定路径的主题
                string = topic.clone().into();
                key = Atom::from(string);
                match node.childs.get_mut(&key) {
                    None => {
                        //指定主题的节点不存在
                        let mut n = PathNode::new(topic.clone(), index);
                        if path.is_final(index) {
                            //路径已结束，则将值插入当前节点的待匹配列表中
                            n.unmatch.insert(value, ());
                            node.childs.insert(key.clone(), n);
                            break;
                        }

                        //插入新的主题节点
                        node.childs.insert(key.clone(), n);
                    },
                    Some(n) => {
                        //指定主题的节点存在
                        if path.is_final(index) {
                            //路径已结束，则将值插入当前节点的待匹配列表中
                            n.unmatch.insert(value, ());
                            break;
                        }
                    },
                }

                //路径还未结束，继续处理路径的下一个主题
                node = node.childs.get_mut(&key).unwrap();
            }
        }

        self.size += 1;

        Ok(())
    }

    //移除一个路径对应的值
    pub fn remove(&mut self, path: TopicPath, value: V) -> Result<(), (TopicPath, V)> {
        if !path.wildcards {
            //路径没有通配符，则返回路径和值
            return Err((path, value));
        }

        let mut key;
        let mut string: String;
        let mut node = &mut self.root;
        for index in 0..path.len() {
            if let Some(topic) = path.get(index) {
                //获取指定路径的主题
                string = topic.clone().into();
                key = Atom::from(string);
                match node.childs.get_mut(&key) {
                    None => {
                        //指定路径的主题不存在，则忽略
                        return Ok(())
                    },
                    Some(n) => {
                        //指定主题的节点存在
                        if path.is_final(index) {
                            //路径已结束，则将值移除当前节点的待匹配列表中
                            n.unmatch.remove(&value);
                            return Ok(())
                        }
                    },
                }

                //路径还未结束，继续处理路径的下一个主题
                node = node.childs.get_mut(&key).unwrap();
            }
        }

        self.size -= 1;

        Ok(())
    }
}

//递归遍历查找匹配指定路径的值
fn lookup<V: Ord + Clone>(mut path_node: Option<&PathNode<V>>, path: &TopicPath, path_len: usize, index: usize, values: &mut BTreeMap<V, ()>) {
    if path_node.is_none() {
        //当前路径树节点为空，则返回
        return;
    }

    for i in index..path_len {
        if let Some(topic) = path.get(i) {
            //获取指定路径的主题
            let string: String = topic.clone().into();
            let key = Atom::from(string);
            let node = path_node.take().unwrap();
//            println!("!!!!!!1, topic: {:?}, childs: {:?}", topic, node.childs.keys());
            match node.childs.get(&key) {
                None => {
//                    println!("!!!!!!4");
                    //路径树的当前节点中不存在指定路径的主题
                    if let Some(r) = node.childs.get(&Atom::from("#")) {
                        //当前路径下有全匹配路径，则获取当前节点下的所有值
                        all_values(&r, values);
                    }

                    //指定路径的主题已结束
                    if path.is_final(i) {
                        //指定路径的主题已结束
                        if let Some(n) = node.childs.get(&Atom::from("+")) {
//                            println!("!!!!!!7");
                            for value in n.unmatch.keys() {
                                values.insert(value.clone(), ());
                            }
                        }
                    } else {
                        //指定路径的主题未结束, 路径树的当前节点中存在当级匹配路径，则尝试查找当级匹配路径，并返回
                        if let Some(n) = node.childs.get(&Atom::from("+")) {
                            lookup(Some(&n), path, path_len, i + 1, values);
                        }
                    }

                    break;
                },
                Some(n) => {
//                    println!("!!!!!!11");
                    //路径树的当前节点中存在指定路径的主题
                    if let Some(r) = node.childs.get(&Atom::from("#")) {
                        //当前路径下有全匹配路径，则获取当前节点下的所有值，并继续
                        all_values(&r, values);
                    }

                    if path.is_final(i) {
                        //指定路径的主题已结束
                        if let Some(r) = node.childs.get(&Atom::from("+")) {
                            for value in r.unmatch.keys() {
                                values.insert(value.clone(), ());
                            }
                        }

                        for value in n.unmatch.keys() {
                            values.insert(value.clone(), ());
                        }
                        break;
                    } else {
                        //路径树的当前节点中存在当级匹配路径，则查找后继续
                        if let Some(r) = node.childs.get(&Atom::from("+")) {
                            lookup(Some(&r), path, path_len, i + 1, values);
                        }
                    }

                    //指定路径的主题未结束，则继续查找
                    path_node = Some(&n);
                },
            }
        }
    }
}

//获取指定全匹配节点下的所有值
fn all_values<V: Ord + Clone>(node: &PathNode<V>, values: &mut BTreeMap<V, ()>) {
    for value in node.unmatch.keys() {
        values.insert(value.clone(), ());
    }
}
