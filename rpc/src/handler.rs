use std::sync::{Arc, RwLock};

use string_cache::DefaultAtom as Atom;
use rpc_server::Session;

/*
* Topic处理
*/
pub trait TopicHandle {
	//处理请求
	fn handle(&self, Atom, u8, Arc<Session>, Arc<Vec<u8>>);
}