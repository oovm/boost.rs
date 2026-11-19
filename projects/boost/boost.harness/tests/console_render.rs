use boost_console::fmt::Paint;
use boost_harness::log::{
    DiagnosticPayload, FieldValue, Level, LogEvent, MessagePayload,
    console::{render, render_plain},
};
/// 同一测试二进制内串行化触碰进程全局状态的用例，替代已移除的 `serial_test`。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn render_plain_log_event() {
    let event = LogEvent::log(Level::Info, "worker").field("message", FieldValue::str("ready")).build();
    assert_eq!(render_plain(&event), "[info] worker: ready");
}

#[test]
fn render_plain_log_event_with_structured_fields() {
    let event = LogEvent::log(Level::Info, "hs_problems::progress")
        .field("message", FieldValue::str("progress"))
        .field("phase", FieldValue::str("golden_full"))
        .field("current", FieldValue::from_value(3_u64))
        .field("total", FieldValue::from_value(10_u64))
        .field("pct", FieldValue::from_value(30_u64))
        .build();
    assert_eq!(render_plain(&event), "[info] hs_problems::progress: progress phase=golden_full current=3 total=10 pct=30");
}

#[test]
fn render_plain_log_event_without_message() {
    let event = LogEvent::log(Level::Debug, "worker").field("attempt", FieldValue::from_value(2_u64)).build();
    assert_eq!(render_plain(&event), "[debug] worker: attempt=2");
}

#[test]
fn render_plain_diagnostic_event() {
    let payload = DiagnosticPayload::new("oak.syntax.unexpected-token", "error", MessagePayload::new("oak.unexpected"));
    let event = LogEvent::diagnostic(payload);
    assert_eq!(render_plain(&event), "[error] diagnostic: diagnostic error [oak.syntax.unexpected-token] oak.unexpected");
}

#[test]
fn styled_render_applies_level_color() {
    let _serial = serial_guard();
    Paint::enable();
    let event = LogEvent::log(Level::Error, "worker").field("message", FieldValue::str("failed")).build();
    let styled = render(&event);
    assert!(styled.contains("\x1B[31m"), "error level should be red");
    assert!(styled.contains("failed"));
}

#[test]
fn styled_render_matches_plain_when_disabled() {
    let _serial = serial_guard();
    let event = LogEvent::log(Level::Warn, "worker").field("message", FieldValue::str("retry")).build();
    Paint::disable();
    assert_eq!(render(&event), render_plain(&event));
    Paint::enable();
}
