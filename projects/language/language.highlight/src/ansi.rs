//! ANSI 终端着色导出。
//!
//! 终端（REPL、CLI 预览）把 [`HighlightSpan`] 渲染成 ANSI 转义序列；
//! 颜色只属于呈现适配，不进入语言或编辑器语义。

use std::fmt::Write;

use super::{HighlightKind, HighlightSpan};

/// 高亮结果：源码文本与已分类片段。
#[derive(Debug, Clone, Copy)]
pub struct HighlightResult<'a> {
    /// 被高亮的源码文本。
    pub source: &'a str,
    /// 已分类片段，按顺序覆盖 `source`。
    pub spans: &'a [HighlightSpan],
}

impl<'a> HighlightResult<'a> {
    /// 组合源码与片段。
    pub fn new(source: &'a str, spans: &'a [HighlightSpan]) -> Self {
        Self { source, spans }
    }
}

/// 把高亮结果渲染为字符串的导出器。
pub trait Exporter {
    /// 渲染高亮结果。
    fn export(&self, result: &HighlightResult<'_>) -> String;
}

/// ANSI 转义着色导出器。
#[derive(Debug, Clone, Copy, Default)]
pub struct AnsiExporter;

impl AnsiExporter {
    /// 创建导出器。
    pub fn new() -> Self {
        Self
    }
}

impl Exporter for AnsiExporter {
    fn export(&self, result: &HighlightResult<'_>) -> String {
        let mut out = String::with_capacity(result.source.len() + result.spans.len() * 12);
        let mut cursor = 0;
        for span in result.spans {
            if span.range.start < cursor || span.range.end > result.source.len() || span.range.start > span.range.end {
                continue;
            }
            out.push_str(&result.source[cursor..span.range.start]);
            let code = ansi_code(span.kind);
            match code {
                Some(code) => {
                    let _ = write!(out, "\u{1b}[{code}m{}\u{1b}[0m", &result.source[span.range.clone()]);
                }
                None => out.push_str(&result.source[span.range.clone()]),
            }
            cursor = span.range.end;
        }
        out.push_str(&result.source[cursor..]);
        out
    }
}

fn ansi_code(kind: HighlightKind) -> Option<&'static str> {
    match kind {
        HighlightKind::Keyword | HighlightKind::ControlKeyword => Some("35"),
        HighlightKind::String => Some("32"),
        HighlightKind::Number => Some("33"),
        HighlightKind::Comment => Some("90"),
        HighlightKind::Operator | HighlightKind::Punctuation => Some("37"),
        HighlightKind::TypeIdentifier => Some("36"),
        HighlightKind::FunctionIdentifier | HighlightKind::VariantIdentifier => Some("94"),
        HighlightKind::Parameter | HighlightKind::Property | HighlightKind::Field => Some("96"),
        _ => None,
    }
}
