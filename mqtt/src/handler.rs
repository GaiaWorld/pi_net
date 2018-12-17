use std::sync::{Arc};

use atom::Atom;
use session::Session;

/*
* Topic处理
*/
pub trait TopicHandle {
	//处理请求
	fn handle(&self, Atom, u8, Arc<Session>, Arc<Vec<u8>>);
}