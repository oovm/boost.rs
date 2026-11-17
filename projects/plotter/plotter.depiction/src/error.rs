//! 结构描绘的致命失败模型。

use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    ops::Range,
};

use plotter_types::{
    Diagnostic, DiagnosticLabel, DiagnosticLocation, DiagnosticSeverity, LabelRole, Message, NAMESPACE, SourceRef, codes, diagnostic,
};

/// 整例被拒绝时的致命失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepictionError {
    code: &'static str,
    message: String,
    span: Option<Range<usize>>,
    diagnostic: Box<Diagnostic>,
}

impl DepictionError {
    /// 从稳定诊断码与消息创建错误。
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        let message = message.into();
        let diagnostic = diagnostic("depiction", code, DiagnosticSeverity::Error, message.clone());
        Self { code, message, span: None, diagnostic: Box::new(diagnostic) }
    }

    /// 附加出问题的字节范围。
    pub fn with_span(mut self, span: Range<usize>) -> Self {
        if span.start < span.end {
            self.span = Some(span);
        }
        self
    }

    /// 出问题的字节范围（若有）。
    pub fn span(&self) -> Option<Range<usize>> {
        self.span.clone()
    }

    /// 稳定的 `davinci.depiction.*` 诊断码。
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// 人类可读的失败描述。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 结构化诊断记录。
    pub fn diagnostic(&self) -> &Diagnostic {
        &self.diagnostic
    }

    /// 带主标签的结构化诊断。
    pub fn labelled(&self, source: &str) -> Diagnostic {
        let Some(span) = &self.span
        else {
            return (*self.diagnostic).clone();
        };
        let Ok(source_ref) = SourceRef::new(NAMESPACE, source)
        else {
            return (*self.diagnostic).clone();
        };
        let Ok(range) = plotter_types::ByteRange::new(span.start as u64, span.end as u64)
        else {
            return (*self.diagnostic).clone();
        };
        (*self.diagnostic).clone().with_primary(DiagnosticLabel::new(
            DiagnosticLocation::Text { source: source_ref, range },
            Message::new(self.code).with_fallback(self.message.clone()),
            LabelRole::Primary,
        ))
    }

    /// 结构超出 v0 描绘子集，整例拒绝。
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(codes::DEPICTION_UNSUPPORTED, message)
    }

    /// 图无法放置，整例拒绝。
    pub fn unplaceable(message: impl Into<String>) -> Self {
        Self::new(codes::DEPICTION_UNPLACEABLE, message)
    }
}

impl Display for DepictionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DepictionError {}
