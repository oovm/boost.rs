use boost_harness::diagnostic::{DiagnosticSeverity, MessageArg};
use data_text_core::{
    diagnostic::{TextDiagnosticsExt, diagnostic_set_from_output, from_text_error, text_wire_code},
    errors::{TextDiagnostics, TextError},
};

#[test]
fn text_error_maps_to_dotted_wire_code() {
    let error = TextError::unexpected_token("foo", 12, None);
    let diagnostic = from_text_error(&error);
    assert_eq!(diagnostic.code().as_str(), "text.syntax.unexpected_token");
    assert_eq!(diagnostic.severity(), DiagnosticSeverity::Error);
    assert!(diagnostic.primary().is_some());
    let args = diagnostic.message().args();
    assert!(args.iter().any(|(name, value)| name == "token" && matches!(value, MessageArg::Text(text) if text == "foo")));
    assert!(args.iter().any(|(name, value)| name == "offset" && matches!(value, MessageArg::U64(12))));
}

#[test]
fn text_error_expected_token_carries_structured_args() {
    let error = TextError::expected_token("Semicolon", 8, None);
    let diagnostic = from_text_error(&error);
    let args = diagnostic.message().args();
    assert!(args.iter().any(|(name, value)| name == "expected" && matches!(value, MessageArg::Text(text) if text == "Semicolon")));
    assert!(args.iter().any(|(name, value)| name == "offset" && matches!(value, MessageArg::U64(8))));
}

#[test]
fn text_diagnostics_collect_fatal_and_non_fatal() {
    let mut output = TextDiagnostics::success(());
    output.diagnostics.push(TextError::syntax_error("warn", 4, None));
    let set = output.unified_set();
    assert_eq!(set.diagnostics().len(), 1);
    assert_eq!(diagnostic_set_from_output(&output).diagnostics().len(), 1);

    let fatal: TextDiagnostics<()> = TextDiagnostics::error(TextError::unexpected_eof(0, None));
    let set = fatal.unified_set();
    assert_eq!(set.diagnostics().len(), 1);
    assert_eq!(text_wire_code(fatal.result.as_ref().err().unwrap().kind()).as_str(), "text.syntax.unexpected_eof");
}
