use std::sync::{Arc, Mutex};

use boost_harness::{
    diagnostic::{DiagnosticSet, emit_diagnostic, emit_diagnostic_set},
    log::{EventKind, Filter, Level, LogEvent, LogSink, VecSink, clear_global_sink, set_global_filter, set_global_sink},
};
/// 同一测试二进制内串行化触碰进程全局状态的用例，替代已移除的 `serial_test`。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

use super::fixtures::text_diagnostic;

struct SharedSink(Arc<Mutex<VecSink>>);

impl LogSink for SharedSink {
    fn emit(&mut self, event: &LogEvent) {
        self.0.lock().expect("vec sink mutex poisoned").emit(event);
    }
}

#[test]
fn emit_diagnostic_posts_log_event() {
    let _serial = serial_guard();
    clear_global_sink();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    set_global_sink(Box::new(SharedSink(sink.clone())));
    set_global_filter(Filter::new(Level::Trace));

    emit_diagnostic(&text_diagnostic());

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert_eq!(guard.events().len(), 1);
    let payload = match guard.events()[0].kind() {
        EventKind::Diagnostic(payload) => payload,
        _ => panic!("expected diagnostic log event"),
    };
    assert_eq!(payload.code(), "oak.syntax.unexpected-token");
    assert_eq!(payload.severity(), "error");

    clear_global_sink();
}

#[test]
fn emit_diagnostic_set_posts_all_events() {
    let _serial = serial_guard();
    clear_global_sink();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    set_global_sink(Box::new(SharedSink(sink.clone())));
    set_global_filter(Filter::new(Level::Trace));

    let mut set = DiagnosticSet::new();
    set.push(text_diagnostic());
    set.push(text_diagnostic());
    emit_diagnostic_set(&set);

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert_eq!(guard.events().len(), 2);

    clear_global_sink();
}
