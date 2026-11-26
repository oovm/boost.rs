//! TypeScript formatter errors use the shared `data.text` diagnostic type.

/// 格式化错误：复用 `data.text` 的共享诊断类型。
pub type FormatError = data_text_core::errors::TextError;
