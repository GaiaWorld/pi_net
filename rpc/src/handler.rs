use std::sync::{Arc, RwLock};

use string_cache::DefaultAtom as Atom;
use rpc_server::Session;

/*
* Topic处理
*/
pub trait TopicHandle {
	//设置会话
	fn set_session(&mut self, session: Arc<RwLock<Session>>) -> Option<Arc<RwLock<Session>>>;
	//处理请求
	fn handle(&self, topic: Atom, version: u8, bin: Arc<Vec<u8>>);
}