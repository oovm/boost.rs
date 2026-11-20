//! VON 源码格式化 provider 注册（依赖方向：前端 → 平台）。
//!
//! `language-format` 平台不反向依赖本 crate；消费者装配入口（`legion` / `asgard` CLI）
//! 在格式化 `.von` 前调用 [`register_source_formatter`]。

use language_format::formatter::{FormatError, FormatOptions, FormattedOutput, SourceFormatter, SourceFormatterProvider, VON_EXTENSIONS};

use super::options::FormatOptions as VonFormatOptions;

struct VonSourceFormatter;

impl SourceFormatter for VonSourceFormatter {
    fn language_id(&self) -> &str {
        "von"
    }

    fn format_document(&self, source: &str, options: &FormatOptions) -> Result<FormattedOutput, FormatError> {
        let local = VonFormatOptions { indent_width: options.indent_width.min(255) as u8, line_width: options.max_width };
        let text = super::format_source(source, &local).map_err(|error| FormatError::Parse { path: None, message: error.to_string() })?;
        let mut out = text;
        if options.ensure_trailing_newline && !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(FormattedOutput::text_only(out))
    }
}

/// VON 源码 formatter 的 provider（`.von`）。
pub struct VonSourceFormatterProvider;

impl SourceFormatterProvider for VonSourceFormatterProvider {
    fn language_ids(&self) -> &[&str] {
        &["von"]
    }

    fn extensions(&self) -> &[&str] {
        VON_EXTENSIONS
    }

    fn formatter(&self) -> Box<dyn SourceFormatter> {
        Box::new(VonSourceFormatter)
    }
}

/// 向 `language-format` 平台注册 VON formatter（幂等，先注册者优先）。
pub fn register_source_formatter() {
    language_format::formatter::register_source_provider(&VonSourceFormatterProvider);
}
