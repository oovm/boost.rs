//! MongoDB 方言客户端。
//!
//! 帧与压缩合同在 `database-protocol-mongo`；本模块只拥有远程会话的
//! pending / 请求完成规则，socket I/O 留给宿主（N-API / CLI）。

/// 远程会话帧队列：拼帧、请求标识、`responseTo` 配对、握手门禁与中断清算。
pub mod session;

pub use session::{MongoWireSession, MORE_TO_COME_FLAG, SessionError, WireEvent};
