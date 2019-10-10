use tcp::util::SocketContext;

/*
* 基础协议会话
*/
pub struct BaseSession {
    context:    SocketContext,  //会话上下文
}

unsafe impl Send for BaseSession {}

impl BaseSession {
    //构建基础协议会话
    pub fn new() -> Self {
        BaseSession {
            context: SocketContext::empty(),
        }
    }

    //获取基础协议会话上下文的只读引用
    pub fn get_context(&self) -> &SocketContext {
        &self.context
    }

    //获取基础协议会话上下文的可写引用
    pub fn get_context_mut(&mut self) -> &mut SocketContext {
        &mut self.context
    }
}