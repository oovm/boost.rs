/// 包解析层的错误。
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// 注册表适配器返回的传输或协议错误。
    #[error(transparent)]
    Registry(#[from] package_registry::RegistryError),
    /// VON 文本解析或序列化失败。
    #[error("VON error: {0}")]
    Von(String),
    /// 本地文件系统读写失败。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 带调用方说明的一般性失败。
    #[error("{0}")]
    Message(String),
}

impl PackageError {
    /// 由说明文本构造错误。
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    /// 由 VON 错误构造错误。
    pub fn von(error: impl std::fmt::Display) -> Self {
        Self::Von(error.to_string())
    }
}

/// 包解析操作的 `Result` 别名。
pub type Result<T> = std::result::Result<T, PackageError>;
