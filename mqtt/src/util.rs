use std::sync::Arc;
use std::fmt::Debug;
use std::cell::RefCell;

use mqtt311::{Topic, TopicPath};

use atom::Atom;
use hash::XHashMap;

use tcp::util::SocketContext;

/*
* 异步回应结果
*/
#[derive(Clone)]
pub struct AsyncResult(Arc<RefCell<Option<std::io::Result<()>>>>);

unsafe impl Send for AsyncResult {}
unsafe impl Sync for AsyncResult {}

impl AsyncResult {
    //构建异步回应结果
    pub fn new() -> Self {
        AsyncResult(Arc::new(RefCell::new(None)))
    }

    //构建指定结果的异步回应结果
    pub fn with(result: std::io::Result<()>) -> Self {
        AsyncResult(Arc::new(RefCell::new(Some(result))))
    }

    //是否需要异步等待回应结果
    pub fn is_wait(&self) -> bool {
        self.0.borrow().is_none()
    }

    //获取异步回应结果
    pub fn get(&self) -> Option<std::io::Result<()>> {
        self.0.borrow_mut().take()
    }

    //设置异步回应结果
    pub fn set(&self, result: std::io::Result<()>) {
        *self.0.borrow_mut() = Some(result);
    }
}

/*
* 值指针相等
*/
pub trait ValueEq {
    fn value_eq(this: &Self, other: &Self) -> bool;
}

/*
* 路径节点
*/
#[derive(Clone)]
struct PathNode<V: ValueEq + Ord + Debug + Clone> {
    topic:      Topic,                          //节点主题名
    level:      usize,                          //节点级别
    unmatch:    Vec<V>,                         //待匹配列表
    childs:     XHashMap<Atom, PathNode<V>>,    //子节点
}

unsafe impl<V: ValueEq + Ord + Debug + Clone> Send for PathNode<V> {}
unsafe impl<V: ValueEq + Ord + Debug + Clone> Sync for PathNode<V> {}

impl<V: ValueEq + Ord + Debug + Clone> PathNode<V> {
    //构建一个指定节点主题名的路径节点
    pub fn new(topic: Topic, level: usize) -> Self {
        PathNode {
            topic,
            level,
            unmatch: Vec::new(),
            childs: XHashMap::default(),
        }
    }
}

/*
* 路径匹配树
*/
#[derive(Clone)]
pub struct PathTree<V: ValueEq + Ord + Debug + Clone> {
    root:   PathNode<V>,    //根节点
    size:   usize,          //值数量
}

impl<V: ValueEq + Ord + Debug + Clone> PathTree<V> {
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

        let mut map = Vec::new(); //用于记录并去重
        lookup(Some(&self.root), &path, path.len(), 0, &mut map);

        Some(map)
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
                            if let Err(index) = n.unmatch.binary_search_by(|v: &V| {
                                v.cmp(&value)
                            }) {
                                //不在待匹配列表中，则插入指定位置，保证列表有序
                                n.unmatch.insert(index, value)
                            }

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
                            if let Err(index) = n.unmatch.binary_search_by(|v: &V| {
                                v.cmp(&value)
                            }) {
                                //不在待匹配列表中，则插入指定位置，保证列表有序
                                n.unmatch.insert(index, value)
                            }
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
                            if let Ok(index) = n.unmatch.binary_search_by(|s| {
                                s.cmp(&value)
                            }) {
                                //从待匹配列表中找到指定会话，则移除
                                n.unmatch.remove(index);
                            }
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
fn lookup<V: ValueEq + Ord + Debug + Clone>(mut path_node: Option<&PathNode<V>>, path: &TopicPath, path_len: usize, index: usize, values: &mut Vec<V>) {
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
                            values.extend_from_slice(&n.unmatch[..])
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
                            values.extend_from_slice(&r.unmatch[..]);
                        }

                        values.extend_from_slice(&n.unmatch[..]);
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
fn all_values<V: ValueEq + Ord + Debug + Clone>(node: &PathNode<V>, values: &mut Vec<V>) {
    values.extend_from_slice(&node.unmatch[..]);
}

/*
* Mqtt代理会话
*/
pub struct BrokerSession {
    client_id:  String,         //客户端id
    keep_alive: u16,            //连接保持间隔时长，单位秒，服务器端在1.5倍间隔时长内没有收到任何控制报文，则主动关闭连接
    is_clean:   bool,           //是否清理会话
    user:       Option<String>, //用户名
    pwd:        Option<String>, //用户密码
    context:    SocketContext,  //会话上下文
}

unsafe impl Send for BrokerSession {}

impl BrokerSession {
    //构建Mqtt代理会话
    pub fn new(client_id: String,
               keep_alive: u16,
               is_clean: bool,
               user: Option<String>,
               pwd: Option<String>) -> Self {
        BrokerSession {
            client_id,
            keep_alive,
            is_clean,
            user,
            pwd,
            context: SocketContext::empty(),
        }
    }

    //获取客户端id
    pub fn get_client_id(&self) -> &String {
        &self.client_id
    }

    //获取连接保持间隔时长
    pub fn get_keep_alive(&self) -> u16 {
        self.keep_alive
    }

    //是否清理会话
    pub fn is_clean_session(&self) -> bool {
        self.is_clean
    }

    //获取用户名
    pub fn get_user(&self) -> Option<&String> {
        self.user.as_ref()
    }

    //获取用户密码
    pub fn get_pwd(&self) -> Option<&String> {
        self.pwd.as_ref()
    }

    //获取Mqtt代理会话上下文的只读引用
    pub fn get_context(&self) -> &SocketContext {
        &self.context
    }

    //获取Mqtt代理会话上下文的可写引用
    pub fn get_context_mut(&mut self) -> &mut SocketContext {
        &mut self.context
    }
}
