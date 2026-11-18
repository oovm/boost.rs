/// 包管理器错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// 注册表适配器错误。
    #[error(transparent)]
    Registry(#[from] package_registry::RegistryError),
    /// 清单 / lock / 版本错误。
    #[error(transparent)]
    Resolve(#[from] package_resolve::PackageError),
    /// 包缓存读写错误。
    #[error(transparent)]
    Cache(#[from] package_cache::CacheError),
    /// 打包与发布错误。
    #[error(transparent)]
    Publish(#[from] package_publish::PublishError),
    /// 本地文件系统读写失败。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 带用户可读说明的一般性失败。
    #[error("{0}")]
    Message(String),
}

impl ManagerError {
    /// 构造仅包含说明文本的错误。
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// 包管理器操作的 `Result` 别名。
pub type Result<T> = std::result::Result<T, ManagerError>;
