//! AWSL 语言与静态渲染栈的操作错误。

use std::fmt::{Display, Formatter};

/// AWSL API 的 `Result` 别名。
pub type AwslResult<T> = std::result::Result<T, AwslError>;

/// AWSL 操作失败。
#[derive(Debug)]
pub enum AwslError {
    /// 裸 I/O 错误。
    Io(std::io::Error),
    /// 一句说明。
    Message(String),
}

impl AwslError {
    /// 构造 `Message` 变体。
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

impl Display for AwslError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, f),
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AwslError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Message(_) => None,
        }
    }
}

impl From<std::io::Error> for AwslError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
