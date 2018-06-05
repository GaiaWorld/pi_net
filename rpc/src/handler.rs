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
	len: 		AtomicUsize,										//处理器消息队列最大长度
	factory: 	Arc<VMFactory>,										//默认虚拟机工厂
	mgr: 		Mgr,												//默认事务管理器
	gray_tab: 	Arc<RwLock<FnvHashMap<u8, (Arc<VMFactory>, Mgr)>>>,	//灰度表
	session:	Option<Arc<RwLock<Session>>>,						//会话
}

impl TopicHandler {
	//构建一个处理器
	pub fn new(len: usize, factory: VMFactory, default: Mgr) -> Self {
		TopicHandler {
			len: AtomicUsize::new(len),
			factory: Arc::new(factory),
			mgr: default,
			gray_tab: Arc::new(RwLock::new(FnvHashMap::default())),
			session: None,
		}
	}

	//获取默认虚拟机工厂和事务管理器
	pub fn get_default(&self) -> (Arc<VMFactory>, Mgr) {
		(self.factory.clone(), self.mgr.clone())
	}

	//设置指定灰度为默认版本
	pub fn set_default(&mut self, gray: u8) {
		if self.session.is_none() {
			return;
		}
		
		match self.gray_tab.write().unwrap().remove(&gray) {
			None => return,
			Some((f, m)) => {
				self.factory = f;
				self.mgr = m;
			},
		}
		self.session.as_ref().unwrap().write().unwrap().set_gray(gray);
	}

	//获取指定灰度的事务管理器
	pub fn get_gray_mgr(&self, gray: u8) -> Option<(Arc<VMFactory>, Mgr)> {
		match self.gray_tab.read().unwrap().get(&gray) {
			_ => None,
			Some((factory, mgr)) => Some((factory.clone(), mgr.clone())),
		}
	}

	//设置指定灰度的管理器
	pub fn set_gray_mgr(&self, gray: u8, factory: VMFactory, mgr: Mgr) {
		self.gray_tab.write().unwrap().insert(gray, (Arc::new(factory), mgr));
	}

	//移除指定灰度的管理器
	pub fn remove(&self, gray: u8) {
		self.gray_tab.write().unwrap().remove(&gray);
	}

	//设置新的会话，返回旧的会话
	pub fn set_session(&mut self, session: Arc<RwLock<Session>>) -> Option<Arc<RwLock<Session>>> {
		let old = self.session.take();
		self.session = Some(session);
		old
	}

	//处理方法
	pub fn handle(&self, topic: Atom, version: u8, bin: Arc<Vec<u8>>) {
		let copy = self.session.clone();
		let (factory, mgr) = self.get(self.session.as_ref().unwrap());
		let args = Box::new(move |vm: JS| -> JS {
			vm.new_str((*topic).to_string());
			let array = vm.new_uint8_array(bin.len() as u32);
			array.from_bytes(bin.as_slice());
			vm.new_native_object(Arc::into_raw(Arc::new(mgr)) as usize);
			vm.new_native_object(Arc::into_raw(Arc::new(copy)) as usize);
			vm
		});
		factory.call(0, args, "");
	}

	//获取指定的虚拟机工厂和事务管理器
	fn get(&self, session: &Arc<RwLock<Session>>) -> (Arc<VMFactory>, Mgr) {
		match session.read().unwrap().get_gray() {
			Some(gray) => {
				match self.get_gray_mgr(gray) {
					None => self.get_default(),
					Some(r) => r,
				}
			},
			_ => self.get_default(),
		}
	}
}