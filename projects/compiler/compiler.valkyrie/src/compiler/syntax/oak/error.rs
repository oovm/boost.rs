//! Valkyrie 语言前端诊断错误（语法与 lowering 边界）。
//!
//! 由 `data.text` 前端与 HIR lowering 产出，不依赖 `vcc-data` parser 类型。
use std::{
    fmt::{Display, Formatter},
    ops::Range,
};

/// Valkyrie 前端解析与 lowering 错误。
#[derive(Debug)]
pub enum ParseError {
    /// 文件读取失败。
    Io(std::io::Error),
    /// 语法或 lowering 失败。
    Invalid {
        /// 错误消息。
        message: String,
        /// 可选源码范围。
        span: Option<Range<usize>>,
    },
}

impl ParseError {
    /// 将 `data.text` 解析错误映射为前端 `ParseError`（保留 `source_offset`）。
    pub fn from_oak(error: data_text_core::TextError) -> Self {
        let message = error.to_string();
        if let Some(start) = error.source_offset() {
            Self::Invalid { message, span: Some(start..start.saturating_add(1)) }
        }
        else {
            Self::Invalid { message, span: None }
        }
    }
}

impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, f),
            Self::Invalid { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<std::io::Error> for ParseError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<data_text_core::TextError> for ParseError {
    fn from(value: data_text_core::TextError) -> Self {
        Self::from_oak(value)
    }
}
