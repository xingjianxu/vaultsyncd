//! 对象回收规则；标记不授权删除，删除前必须重新核对引用（design §19.4）。
pub mod mark;
pub mod sweep;
