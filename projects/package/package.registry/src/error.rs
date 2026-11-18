/// 注册表层的错误。
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// 传输或连接失败。
    #[error("transport error: {0}")]
    Transport(#[from] boost_net::NetError),
    /// 注册表以非成功状态码响应。
    #[error("registry request failed with status {status}: {message}")]
    Status {
        /// 注册表返回的 HTTP 状态码。
        status: u16,
        /// 可读的错误正文或摘要。
        message: String,
    },
    /// JSON 序列化或反序列化失败。
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// 本地文件系统操作失败。
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// 请求的包或版本不存在。
    #[error("package not found: {0}")]
    NotFound(String),
    /// 适配器不支持该操作。
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    /// 其它注册表失败，携带调用方给出的说明。
    #[error("{0}")]
    Message(String),
}

impl RegistryError {
    /// 由 HTTP 状态码与说明构造错误。
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self::Status { status, message: message.into() }
    }

    /// 由纯文本说明构造错误。
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    /// 由不支持说明构造错误。
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }
}
