/// 同一测试二进制内串行化触碰进程全局状态的用例，替代已移除的 `serial_test`。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
use std::sync::{Arc, Mutex};

use boost_harness::log::{Filter, Level, LogEvent, LogSink, VecSink, clear_global_sink};
use crawler::{FetchRequest, HttpTransport, Transport, map_transport_error};
use url::Url;

struct SharedSink(Arc<Mutex<VecSink>>);

impl LogSink for SharedSink {
    fn emit(&mut self, event: &LogEvent) {
        self.0.lock().expect("vec sink mutex poisoned").emit(event);
    }
}

#[test]
#[cfg(feature = "logger")]
fn map_transport_error_emits_logger_event_when_feature_enabled() {
    let _serial = serial_guard();
    clear_global_sink();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    boost_harness::log::set_global_sink(Box::new(SharedSink(sink.clone())));
    boost_harness::log::set_global_filter(Filter::new(Level::Trace));

    let _ = map_transport_error("robots denied");

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert_eq!(guard.events().len(), 1);
    assert_eq!(guard.events()[0].target(), "crawler.transport");
    assert_eq!(guard.events()[0].level(), Level::Error);
    clear_global_sink();
}

#[test]
fn fetch_failure_maps_to_transport_error() {
    let _serial = serial_guard();
    let transport = HttpTransport::new();
    let request = FetchRequest { url: Url::parse("file:///missing.html").expect("url"), headers: Default::default() };
    let error = transport.fetch(&request).expect_err("fetch should fail");
    assert!(matches!(error, crawler_types::CrawlError::Diagnostic(_)));
    assert_eq!(error.code(), Some("crawler.fetch.transport-failed"));
}
