//! 乐谱降级的致命失败模型。

use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    ops::Range,
};

use plotter_types::{
    Diagnostic, DiagnosticLabel, DiagnosticLocation, DiagnosticSeverity, LabelRole, Message, NAMESPACE, SourceRef, codes, diagnostic,
};

/// 整张谱被拒绝时的致命失败。
///
/// 与 `plotter_diagram::DiagramError` 同形：稳定码 + 消息 + 可选字节跨度。
/// 跨度由语言前端附加；纯程序化降级（`ScoreSpec` 入口）通常没有跨度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngravingError {
    code: &'static str,
    message: String,
    span: Option<Range<usize>>,
    diagnostic: Box<Diagnostic>,
}

impl EngravingError {
    /// 从稳定诊断码与消息创建错误。
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        let message = message.into();
        let diagnostic = diagnostic("engraving", code, DiagnosticSeverity::Error, message.clone());
        Self { code, message, span: None, diagnostic: Box::new(diagnostic) }
    }

    /// 附加出问题的字节范围。
    pub fn with_span(mut self, span: Range<usize>) -> Self {
        if span.start < span.end {
            self.span = Some(span);
        }
        self
    }

    /// 前端能定位时附加字节范围。
    pub fn with_optional_span(self, span: Option<Range<usize>>) -> Self {
        match span {
            Some(span) => self.with_span(span),
            None => self,
        }
    }

    /// 出问题的字节范围（若有）。
    pub fn span(&self) -> Option<Range<usize>> {
        self.span.clone()
    }

    /// 稳定的 `davinci.engraving.*` 诊断码。
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

    /// 带主标签的结构化诊断，标签覆盖 [`EngravingError::span`]。
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

    /// 谱面为空（无小节或全部小节无事件）。
    pub fn empty_score(message: impl Into<String>) -> Self {
        Self::new(codes::ENGRAVING_EMPTY_SCORE, message)
    }

    /// 构造超出 v0 engraving 场景，整例拒绝。
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(codes::ENGRAVING_UNSUPPORTED, message)
    }
}

impl Display for EngravingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EngravingError {}
