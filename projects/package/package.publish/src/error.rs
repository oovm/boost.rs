/// 发布层的错误。
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// 注册表适配器错误。
    #[error(transparent)]
    Registry(#[from] package_registry::RegistryError),
    /// 清单 / lock / 版本错误。
    #[error(transparent)]
    Resolve(#[from] package_resolve::PackageError),
    /// 本地文件系统读写失败。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 目标解析失败。
    #[error("target resolution failed: {0}")]
    Target(String),
    /// 带调用方说明的一般性失败。
    #[error("{0}")]
    Message(String),
}

impl PublishError {
    /// 由说明文本构造错误。
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// 发布操作的 `Result` 别名。
pub type Result<T> = std::result::Result<T, PublishError>;
