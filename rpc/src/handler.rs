use std::sync::{Arc, RwLock};
use std::sync::atomic::AtomicUsize;
use std::io::{Read, Write, Result};

use fnv::FnvHashMap;
use string_cache::DefaultAtom as Atom;
use rpc_server::Session;

use pi_vm::adapter::JS;
use pi_vm::pi_vm_impl::VMFactory;
use pi_db::mgr::Mgr;

/*
* Topic处理器
*/
pub struct TopicHandler {
	len: 		AtomicUsize,							//处理器消息队列最大长度
	factory: 	VMFactory,								//虚拟机工厂
	default: 	Mgr,									//默认事务管理器
	gray_tab: 	Arc<RwLock<FnvHashMap<usize, Mgr>>>,	//灰度表
}

impl TopicHandler {
	//构建一个处理器
	pub fn new(len: usize, factory: VMFactory, default: Mgr) -> Self {
		TopicHandler {
			len: AtomicUsize::new(len),
			factory: factory,
			default: default,
			gray_tab: Arc::new(RwLock::new(FnvHashMap::default())),
		}
	}

	//处理方法
	pub fn handle(&self, topic: Atom, session: Session, version: u8, bin: Arc<Vec<u8>>) {
		let args = move |vm: JS| -> JS {
			vm.new_str((*topic).to_string());
			let array = vm.new_uint8_array(bin.len() as u32);
			array.from_bytes(bin.as_slice());
			// vm.new_native_object(Arc::into_raw(Arc::new(mgr)) as usize);
			vm
		};
	}
}

//获取指定的事务管理器
// #[inline]
// get_mgr(session: Session, version: u8) -> Mgr {
	
// }