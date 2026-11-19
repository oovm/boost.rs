//! Adapters from text parser errors to the unified `boost-harness` diagnostic contract.

use boost_harness::diagnostic::{
    ByteRange, DiagnosticLabel, DiagnosticLocation, DiagnosticOrigin, DiagnosticSeverity, LabelRole, Message, MessageArg, SourceRef,
};
pub use boost_harness::diagnostic::{
    Diagnostic, DiagnosticAction, DiagnosticCode, DiagnosticEnvelope, DiagnosticLabel as UnifiedDiagnosticLabel,
    DiagnosticLocation as UnifiedDiagnosticLocation, DiagnosticOrigin as UnifiedDiagnosticOrigin, DiagnosticSet,
    DiagnosticSeverity as UnifiedSeverity, DiagnosticSink, Message as UnifiedMessage, SCHEMA_VERSION,
};

use crate::errors::{TextDiagnostics, TextError, TextErrorKind};

/// Convert a `data-text-core` i18n key (`error.*`) to a dotted wire code (`text.syntax.*`).
pub fn text_wire_code(kind: &TextErrorKind) -> DiagnosticCode {
    let key = kind.key();
    let suffix = key.strip_prefix("error.").unwrap_or(key);
    DiagnosticCode::new(format!("text.syntax.{suffix}"))
}

/// Map text error kinds to unified diagnostic severity.
pub fn text_severity(kind: &TextErrorKind) -> DiagnosticSeverity {
    match kind {
        TextErrorKind::TestRegenerated { .. } => DiagnosticSeverity::Info,
        TextErrorKind::InternalError { .. } | TextErrorKind::SerdeError { .. } => DiagnosticSeverity::Bug,
        _ => DiagnosticSeverity::Error,
    }
}

fn source_ref(source: Option<SourceRef>) -> SourceRef {
    source.unwrap_or_else(|| SourceRef::new("text", "anonymous").expect("text fallback source identity"))
}

fn text_label(source: Option<SourceRef>, offset: usize, span_len: usize) -> Option<DiagnosticLabel> {
    let end = offset.saturating_add(span_len.max(1));
    let range = ByteRange::new(offset as u64, end as u64).ok()?;
    let location = DiagnosticLocation::Text { source: source_ref(source), range };
    Some(DiagnosticLabel::new(location, Message::new("text.label.here").with_fallback("here"), LabelRole::Primary))
}

fn message_for_kind(kind: &TextErrorKind) -> Message {
    let key = kind.key();
    let fallback = kind.to_string();
    let mut message = Message::new(key).with_fallback(fallback);
    if let Some(offset) = kind.source_offset() {
        message = message.with_arg("offset", MessageArg::U64(offset as u64));
    }
    match kind {
        TextErrorKind::IoError { source_ref, .. } => {
            if let Some(reference) = source_ref {
                message = message.with_arg("source_ref", MessageArg::Text(reference.wire_id()));
            }
        }
        TextErrorKind::SyntaxError { message: text, .. } => {
            message = message.with_arg("message", MessageArg::Text(text.clone()));
        }
        TextErrorKind::UnexpectedCharacter { character, .. } => {
            message = message.with_arg("character", MessageArg::Text(character.to_string()));
        }
        TextErrorKind::UnexpectedToken { token, .. } => {
            message = message.with_arg("token", MessageArg::Text(token.clone()));
        }
        TextErrorKind::ExpectedToken { expected, .. } => {
            message = message.with_arg("expected", MessageArg::Text(expected.clone()));
        }
        TextErrorKind::ExpectedName { name_kind, .. } => {
            message = message.with_arg("name_kind", MessageArg::Text(name_kind.clone()));
        }
        TextErrorKind::CustomError { message: text }
        | TextErrorKind::InvalidTheme { message: text }
        | TextErrorKind::FormatError { message: text }
        | TextErrorKind::SemanticError { message: text }
        | TextErrorKind::ProtocolError { message: text }
        | TextErrorKind::SerdeError { message: text }
        | TextErrorKind::DeserializeError { message: text }
        | TextErrorKind::XmlError { message: text }
        | TextErrorKind::ZipError { message: text }
        | TextErrorKind::ParseError { message: text }
        | TextErrorKind::InternalError { message: text } => {
            message = message.with_arg("message", MessageArg::Text(text.clone()));
        }
        TextErrorKind::UnsupportedFormat { format } => {
            message = message.with_arg("format", MessageArg::Text(format.clone()));
        }
        TextErrorKind::ColorParseError { color } => {
            message = message.with_arg("color", MessageArg::Text(color.clone()));
        }
        TextErrorKind::TestFailure { path, expected, actual } => {
            message = message
                .with_arg("path", MessageArg::Text(path.display().to_string()))
                .with_arg("expected", MessageArg::Text(expected.clone()))
                .with_arg("actual", MessageArg::Text(actual.clone()));
        }
        TextErrorKind::TestRegenerated { path } => {
            message = message.with_arg("path", MessageArg::Text(path.display().to_string()));
        }
        TextErrorKind::UnexpectedEof { .. } | TextErrorKind::TrailingCommaNotAllowed { .. } => {}
    }
    message
}

fn primary_label(kind: &TextErrorKind) -> Option<DiagnosticLabel> {
    match kind {
        TextErrorKind::SyntaxError { offset, source_ref, .. }
        | TextErrorKind::UnexpectedCharacter { offset, source_ref, .. }
        | TextErrorKind::UnexpectedToken { offset, source_ref, .. }
        | TextErrorKind::UnexpectedEof { offset, source_ref, .. }
        | TextErrorKind::ExpectedToken { offset, source_ref, .. }
        | TextErrorKind::ExpectedName { offset, source_ref, .. }
        | TextErrorKind::TrailingCommaNotAllowed { offset, source_ref, .. } => text_label(source_ref.clone(), *offset, 1),
        _ => None,
    }
}

/// Convert one `TextError` into a unified diagnostic record.
pub fn from_text_error(error: &TextError) -> Diagnostic {
    let kind = error.kind();
    let wire = text_wire_code(kind);
    let severity = text_severity(kind);
    let mut diagnostic = Diagnostic::new(wire, severity, DiagnosticOrigin::new("data.text", "parser"), message_for_kind(kind));
    if let Some(label) = primary_label(kind) {
        diagnostic = diagnostic.with_primary(label);
    }
    diagnostic
}

/// Collect unified diagnostics from a parse output.
pub fn diagnostic_set_from_output<T>(output: &TextDiagnostics<T>) -> DiagnosticSet {
    output.unified_set()
}

/// Extension methods for exporting parse output to the unified diagnostic contract.
pub trait TextDiagnosticsExt<T> {
    /// Collect fatal and non-fatal parser errors into a [`DiagnosticSet`].
    fn unified_set(&self) -> DiagnosticSet;
}

impl<T> TextDiagnosticsExt<T> for TextDiagnostics<T> {
    fn unified_set(&self) -> DiagnosticSet {
        let mut set = DiagnosticSet::new();
        if let Err(error) = &self.result {
            set.push(from_text_error(error));
        }
        for error in &self.diagnostics {
            set.push(from_text_error(error));
        }
        set
    }
}

/// Project parse output into unified diagnostics and emit each record through the harness log sink.
#[cfg(feature = "logger")]
pub fn emit_unified_output<T>(output: &TextDiagnostics<T>) {
    boost_harness::diagnostic::emit_diagnostic_set(&output.unified_set());
}

/// Project text parser errors and emit each unified diagnostic through the harness log sink.
#[cfg(feature = "logger")]
pub fn emit_text_errors(errors: &[TextError]) {
    let mut set = DiagnosticSet::new();
    for error in errors {
        set.push(from_text_error(error));
    }
    boost_harness::diagnostic::emit_diagnostic_set(&set);
}
