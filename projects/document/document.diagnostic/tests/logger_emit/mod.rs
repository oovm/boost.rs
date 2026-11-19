/// 同一测试二进制内串行化触碰进程全局状态的用例，替代已移除的 `serial_test`。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
use std::sync::{Arc, Mutex};

use boost_harness::log::{EventKind, Filter, Level, LogEvent, LogSink, VecSink, clear_global_sink, dropped_events, reset_dropped_events};
use document_diagnostic::{
    adapter_error_to_log_event, diagnostics_from_graph, emit_adapter_error, emit_diagnostic_set, from_adapter_error, to_log_event,
};
use document_ir::{DocumentGraph, IdAllocator, LossMarker, SemanticStatus};
use document_types::AdapterError;

struct SharedSink(Arc<Mutex<VecSink>>);

impl LogSink for SharedSink {
    fn emit(&mut self, event: &LogEvent) {
        self.0.lock().expect("vec sink mutex poisoned").emit(event);
    }
}

#[test]
fn adapter_error_to_log_event_preserves_diagnostic_kind() {
    let event = adapter_error_to_log_event(&AdapterError::not_implemented("docx writer"));
    assert!(matches!(event.kind(), EventKind::Diagnostic(_)));
    assert_eq!(event.level(), Level::Error);
}

#[test]
fn emit_adapter_error_uses_global_logger_sink() {
    let _serial = serial_guard();
    clear_global_sink();
    reset_dropped_events();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    boost_harness::log::set_global_sink(Box::new(SharedSink(sink.clone())));
    boost_harness::log::set_global_filter(Filter::new(Level::Trace));

    emit_adapter_error(&AdapterError::unsupported_format("doc", "read"));

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert_eq!(guard.events().len(), 1);
    assert!(matches!(guard.events()[0].kind(), EventKind::Diagnostic(_)));
    assert_eq!(dropped_events(), 0);
    clear_global_sink();
}

#[test]
fn emit_diagnostic_set_projects_graph_losses() {
    let _serial = serial_guard();
    clear_global_sink();
    reset_dropped_events();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    boost_harness::log::set_global_sink(Box::new(SharedSink(sink.clone())));
    boost_harness::log::set_global_filter(Filter::new(Level::Trace));

    let mut graph = DocumentGraph::new(IdAllocator::default().document_id());
    graph.push_loss(LossMarker {
        code: "panduck.writer.unsupported-feature".to_string(),
        message: "strike not expressible".to_string(),
        status: SemanticStatus::Lossy,
    });
    emit_diagnostic_set(&diagnostics_from_graph(&graph));

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert_eq!(guard.events().len(), 1);
    let payload = match guard.events()[0].kind() {
        EventKind::Diagnostic(payload) => payload,
        _ => panic!("expected diagnostic event"),
    };
    assert_eq!(payload.code(), "panduck.writer.unsupported-feature");
    clear_global_sink();
}

#[test]
fn to_log_event_round_trips_existing_adapter_mapping() {
    let diagnostic = from_adapter_error(&AdapterError::invalid_input("bad header"));
    let event = to_log_event(&diagnostic);
    assert!(matches!(event.kind(), EventKind::Diagnostic(payload) if payload.code() == "panduck.adapter.invalid-input"));
}
