//! 缓存层的错误。

use std::path::PathBuf;

/// 缓存机制产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// 未携带具体路径的文件系统 I/O 失败。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 携带缓存条目路径的文件系统 I/O 失败。
    #[error("cache IO error at {path}: {source}")]
    IoAt {
        /// 发生 I/O 错误的路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// 缓存条目编码失败。
    #[error("failed to encode cache entry: {0}")]
    Encode(String),
    /// 缓存条目 / 索引解码失败。
    #[error("failed to decode cache entry at {path}: {detail}")]
    Decode {
        /// 解码失败的路径。
        path: PathBuf,
        /// 解码错误详情。
        detail: String,
    },
    /// 带调用方说明的一般性失败。
    #[error("{0}")]
    Message(String),
}

impl CacheError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::IoAt { path: path.into(), source }
    }

    /// 由说明文本构造错误。
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// 缓存操作的 `Result` 别名。
pub type Result<T> = std::result::Result<T, CacheError>;
