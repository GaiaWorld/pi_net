// use std::path::Path;
// use std::collections::{HashMap, BTreeMap};
// use std::collections::hash_map::Entry as HashMapEntry;
// use std::collections::btree_map::Entry as BtreeMapEntry;

// use pi_lib::atom::Atom;

// use iron::prelude::*;
// use mount::*;
// use staticfile::*;

// /*
// * 文件访问选项
// */
// pub struct FilesAccOptions {
//     router: BTreeMap<Atom, (Atom, Static)>,
//     vtabs:  HashMap<Atom, Vec<Atom>>,
// }

// impl FilesAccOptions {
//     //构建一个文件访问选项
//     pub fn new() -> Self {
//         FilesAccOptions {
//             router: BTreeMap::new(),
//             vtabs: HashMap::new(),
//         }
//     }

//     //获取路由数量
//     pub fn size(&self) -> usize {
//         self.router.len() + self.vtabs.len()
//     }

//     //获取指定路由映射的路径
//     pub fn get_route(&self, route: &Atom) -> Option<Atom> {
//         match self.router.get(route) {
//             None => None,
//             Some((root, _)) => Some(root.clone())
//         }
//     }

//     //增加一条路由，返回是否成功，如果目录没有指定文件，则通过路由访问时默认在对应目录下加载index.html, 最近一次增加的route会覆盖上一次相同的route, root如果为"/"，则默认为目录，不会被认为是路由
//     pub fn add_route(&mut self, route: Atom, root: Atom) -> bool {
//         let path = Path::new((&root).as_str());
//         if path.is_dir() || path.is_file() {
//             //root是目录或文件
//             match self.router.entry(route) {
//                 BtreeMapEntry::Occupied(ref mut e) => {
//                     e.insert((root.clone(), Static::new(path)));
//                     true
//                 },
//                 BtreeMapEntry::Vacant(e) => {
//                     e.insert((root.clone(), Static::new(path)));
//                     true
//                 },
//             }
//         } else {
//             //root是路由
//             if self.router.contains_key(&root) {
//                 //路由已存在，则复用路由并加入虚表
//                 match self.vtabs.entry(root.clone()) {
//                     HashMapEntry::Occupied(ref mut e) => {
//                         //虚表中存在，则增加
//                         e.get_mut().push(route);
//                     },
//                     HashMapEntry::Vacant(e) => {
//                         //虚表中不存在，则新建
//                         e.insert(vec![route]);    
//                     },
//                 }
//                 true
//             } else {
//                 //路由不存在
//                 false
//             }
//         }
//     }

//     //清理路由表
//     pub fn clear(&mut self) {
//         self.router.clear();
//         self.vtabs.clear();
//     }

//     //获取路由表
//     pub fn to_router(&self) -> Chain {
//         let mut mount = Mount::new();
//         for (route, (root, accesser)) in self.router.iter() {
//             println!("!!!!!!route: {:?}, root: {:?}", route, root);
//             mount.mount(route.as_str(), accesser.clone());
//             match self.vtabs.get(route) {
//                 None => (),
//                 Some(list) => {
//                     for vroute in list {
//                         println!("!!!!!!vroute: {:?}, root: {:?}", vroute, root);
//                         mount.mount(vroute.as_str(), accesser.clone());
//                     }
//                 },
//             }
//         }
//         Chain::new(mount)
//     }
// }


