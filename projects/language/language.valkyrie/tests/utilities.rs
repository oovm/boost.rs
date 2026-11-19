//! `utilities` 公开面 smoke：诊断 re-export 与命令出口文案。

use language_valkyrie::{
    report,
    utilities::{
        diagnostic::{DiagnosticCode, DiagnosticSeverity},
        display_failure, report_msg,
    },
};

#[test]
fn report_macro_formats_a_user_facing_sentence() {
    let message: String = report!("failed to build {} for {} target", "app", 3);
    assert_eq!(message, "failed to build app for 3 target");
}

#[test]
fn report_helpers_keep_plain_text() {
    assert_eq!(report_msg("missing doki.toml"), "missing doki.toml");
    assert_eq!(display_failure(std::io::Error::other("disk full")), "disk full");
}

#[test]
fn diagnostic_reexport_carries_a_stable_code_and_severity() {
    let code = DiagnosticCode::new("valkyrie.frontend.parse.syntax");
    assert_eq!(code.as_str(), "valkyrie.frontend.parse.syntax");
    assert_eq!(code.to_string(), "valkyrie.frontend.parse.syntax");
    assert_eq!(format!("{:?}", DiagnosticSeverity::Error), "Error");
}
