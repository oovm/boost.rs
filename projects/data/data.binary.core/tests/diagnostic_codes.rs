use data_binary_core::diagnostic::{BinaryDiagnosticCode, BudgetUsage, DiagnosticSet, DiagnosticSeverity, MessageArg, actions, error, warning};

#[test]
fn diagnostic_codes_use_dotted_wire_identifiers() {
    assert_eq!(BinaryDiagnosticCode::NeedRange.as_str(), "binary.layout.need-range");
    assert_eq!(BinaryDiagnosticCode::OffsetOverflow.as_str(), "binary.layout.offset-overflow");
    assert_eq!(BinaryDiagnosticCode::MagicMismatch.as_str(), "binary.probe.magic-mismatch");
}

#[test]
fn builders_emit_unified_diagnostic_records() {
    let diagnostic = error(BinaryDiagnosticCode::Truncated, "input ended early")
        .with_action(actions::provide_ranges().with_arg("expected", MessageArg::U64(64)));

    assert_eq!(diagnostic.code().as_str(), "binary.layout.truncated");
    assert_eq!(diagnostic.severity(), DiagnosticSeverity::Error);
    assert_eq!(diagnostic.origin().namespace(), "binary");
    assert_eq!(diagnostic.message().fallback(), Some("input ended early"));
    assert_eq!(diagnostic.actions().len(), 1);
}

#[test]
fn diagnostic_set_collects_binary_records() {
    let mut set = DiagnosticSet::new();
    set.push(warning(BinaryDiagnosticCode::NeedRange, "need tail bytes"));
    assert_eq!(set.diagnostics().len(), 1);
}

#[test]
fn budget_usage_defaults_to_zero() {
    assert_eq!(BudgetUsage::default().read_bytes, 0);
}
