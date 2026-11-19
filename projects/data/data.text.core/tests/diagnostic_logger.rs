use std::sync::{Arc, Mutex};

use boost_harness::log::{
    EventKind, Filter, Level, LogSink, VecSink, clear_global_sink, reset_dropped_events, set_global_filter, set_global_sink,
};
use data_text_core::{
    diagnostic::{emit_text_errors, emit_unified_output},
    errors::{TextDiagnostics, TextError},
};
use serial_test::serial;

struct SharedSink(Arc<Mutex<VecSink>>);

impl LogSink for SharedSink {
    fn emit(&mut self, event: &boost_harness::log::LogEvent) {
        self.0.lock().expect("vec sink mutex poisoned").emit(event);
    }
}

#[test]
#[serial]
fn emit_unified_output_posts_log_events() {
    clear_global_sink();
    reset_dropped_events();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    set_global_sink(Box::new(SharedSink(sink.clone())));
    set_global_filter(Filter::new(Level::Trace));

    let output: TextDiagnostics<()> = TextDiagnostics::error(TextError::unexpected_token("foo", 12, None));
    emit_unified_output(&output);

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert_eq!(guard.events().len(), 1);
    assert!(matches!(guard.events()[0].kind(), EventKind::Diagnostic(_)));
    clear_global_sink();
}

#[test]
#[serial]
fn emit_text_errors_posts_all_events() {
    clear_global_sink();
    reset_dropped_events();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    set_global_sink(Box::new(SharedSink(sink.clone())));
    set_global_filter(Filter::new(Level::Trace));

    let errors = vec![TextError::unexpected_token("foo", 1, None), TextError::unexpected_eof(4, None)];
    emit_text_errors(&errors);

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert_eq!(guard.events().len(), 2);
    clear_global_sink();
}
