use std::sync::{Arc, Mutex};

use boost_harness::log::{LogEvent, LogSink, VecSink, clear_global_sink, console::install_global_subscriber, event, set_global_sink};
/// 同一测试二进制内串行化触碰进程全局状态的用例，替代已移除的 `serial_test`。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct SharedSink(Arc<Mutex<VecSink>>);

impl LogSink for SharedSink {
    fn emit(&mut self, event: &LogEvent) {
        self.0.lock().expect("vec sink mutex poisoned").emit(event);
    }
}

#[test]
fn install_global_subscriber_replaces_sink() {
    let _serial = serial_guard();
    clear_global_sink();
    let sink = Arc::new(Mutex::new(VecSink::new()));
    set_global_sink(Box::new(SharedSink(sink.clone())));

    install_global_subscriber();
    event!(Info, "console.subscriber", message = "visible");

    let guard = sink.lock().expect("vec sink mutex poisoned");
    assert!(guard.events().is_empty(), "stderr subscriber should replace the test sink");
    clear_global_sink();
}
