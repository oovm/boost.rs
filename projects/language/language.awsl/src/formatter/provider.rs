//! AWSL 源码格式化 provider 注册（依赖方向：前端 → 平台）。
//!
//! `language-format` 平台不反向依赖本 crate；消费者装配入口（`legion` / `asgard` CLI）
//! 在格式化 `.awsl` / `.w` 前调用 [`register_source_formatter`]。

use language_format::formatter::{AWSL_EXTENSIONS, FormatError, FormatOptions, FormattedOutput, SourceFormatter, SourceFormatterProvider};

use super::options::FormatOptions as AwslFormatOptions;

struct AwslSourceFormatter;

impl SourceFormatter for AwslSourceFormatter {
    fn language_id(&self) -> &str {
        "awsl"
    }

    fn format_document(&self, source: &str, options: &FormatOptions) -> Result<FormattedOutput, FormatError> {
        let local = AwslFormatOptions { indent_width: options.indent_width.min(255) as u8, line_width: options.max_width };
        let text = super::format_source(source, &local).map_err(|error| FormatError::Parse { path: None, message: error.to_string() })?;
        let mut out = text;
        if options.ensure_trailing_newline && !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(FormattedOutput::text_only(out))
    }
}

/// AWSL 源码 formatter 的 provider（`.awsl` 规范拼写 / `.w` 同义缩写）。
pub struct AwslSourceFormatterProvider;

impl SourceFormatterProvider for AwslSourceFormatterProvider {
    fn language_ids(&self) -> &[&str] {
        &["awsl"]
    }

    fn extensions(&self) -> &[&str] {
        AWSL_EXTENSIONS
    }

    fn formatter(&self) -> Box<dyn SourceFormatter> {
        Box::new(AwslSourceFormatter)
    }
}

/// 向 `language-format` 平台注册 AWSL formatter（幂等，先注册者优先）。
pub fn register_source_formatter() {
    language_format::formatter::register_source_provider(&AwslSourceFormatterProvider);
}
