//! AWSL formatter errors use the shared `data.text` diagnostic type.

/// AWSL 格式化失败（复用 `data-text-core` 文本错误类型）。
pub type FormatError = data_text_core::errors::TextError;
