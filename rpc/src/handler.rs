use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::io::{Read, Write, Result};

// /*
// * 消息处理
// */
// pub trait MsgHandle {
//     type PrevResult;
//     type NextResult;

//     fn handle(&self, Self::PrevResult) -> Self::NextResult;
// }

// struct MsgHandler {

// }

// impl MsgHandle for MsgHandler {
//     type PrevResult = Result<(Arc<ClientStub>, Arc<[u8]>)>;
//     type NextResult = Result<(Arc<ClientStub>, Atom, BonBuffer)>;

//     fn handle(&self, data: Self::PrevResult) -> Self::NextResult {
// 		data.and_then(|(stub, bin)| {
// 			let tail = bin.len();
// 			Ok((stub, Atom::from(""), BonBuffer::with_bytes(bin.to_vec(), Some(0), Some(tail))))
// 		})
//     }
// }

// impl MsgProtocolHandler {
// 	//构建一个消息协议处理器
// 	pub fn new(msg: Arc<[u8]>) -> Self {
// 		MsgProtocolHandler {
// 			msg: msg,
// 		}
// 	}
// }

// /*
// * 消息事务处理器
// */
// pub struct MsgTxHandler {

// }

// impl MsgHandle for MsgTxHandler {
// 	type PrevResult = <MsgProtocolHandler as MsgHandle>::NextResult;
// 	type NextResult = bool;

// 	fn handle(&self, data: Self::PrevResult) -> Self::NextResult {
// 		true
// 	}
// }

// /*
// * 消息处理链
// */
// pub trait MsgHandleChain {
//     //创建一个没有队列的链，消息的所有处理都交由当前线程完成
// 	fn new() -> Self;

// 	//设置指定消息队列长度的链，当前线程只负责将消息队列头中的消息投递到任务池中执行
// 	fn with_queue(usize) -> Self;

// 	//为链尾部增加处理器，当前链上的所有处理器，都会在一个线程中同步执行完成
// 	fn link<T: MsgHandle>(self, Arc<T>) -> Self;

//     //获取当前链的消息队列长度
// 	fn len(&self) -> usize;

// 	//获取当前链的消息数量
// 	fn size(&self) -> usize;
// }

/*
* Topic处理器
*/
pub struct TopicHandler {
	len: AtomicUsize,
}