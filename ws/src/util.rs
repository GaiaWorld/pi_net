use std::sync::Arc;
use std::collections::HashMap;

use fnv::FnvBuildHasher;

/*
* Websocket子协议
*/
pub trait ChildProtocol: Send + Sync + 'static {
    //获取子协议名称
    fn protocol_name(&self) -> &str;
}
