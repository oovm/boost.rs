//! Verification that `diagnose!` builds a structured diagnostic record.

use boost_harness::diagnostic::{DiagnosticSeverity, diagnose};

#[test]
fn diagnose_builds_a_structured_record() {
    let record = diagnose! {
        code: "macro.test.sample",
        severity: error,
        message: "sample message",
    };

    assert_eq!(record.code().as_str(), "macro.test.sample");
    assert_eq!(record.severity(), DiagnosticSeverity::Error);
    assert_eq!(record.origin().to_string(), "macro/diagnostic-macro");
    assert_eq!(record.message().fallback(), Some("sample message"));
}
