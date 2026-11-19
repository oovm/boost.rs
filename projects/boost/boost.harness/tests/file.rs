use std::io::{Read, Seek, SeekFrom};

use boost_harness::log::{
    FieldValue, FileSink, Level, LogEvent, LogSink, VecSink, clear_global_sink, emit, event_to_json, install_global_file_sink, set_global_sink,
};
/// 同一测试二进制内串行化触碰进程全局状态的用例，替代已移除的 `serial_test`。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn file_sink_appends_json_lines() {
    let path = std::env::temp_dir().join(format!("logger-file-sink-{}.jsonl", std::process::id()));
    let mut sink = FileSink::open(&path).expect("open file sink");
    sink.emit(&LogEvent::log(Level::Info, "worker").field("message", FieldValue::str("ready")).build());
    sink.flush().expect("flush file sink");

    let mut file = std::fs::File::open(&path).expect("reopen output");
    let mut text = String::new();
    file.read_to_string(&mut text).expect("read output");
    assert!(text.contains("\"kind\":\"log\""));
    assert!(text.ends_with('\n'));

    let _ = std::fs::remove_file(path);
}

#[test]
fn install_global_file_sink_routes_events() {
    let _serial = serial_guard();
    let path = std::env::temp_dir().join(format!("logger-global-file-{}.jsonl", std::process::id()));
    install_global_file_sink(&path).expect("install global file sink");

    let event = LogEvent::log(Level::Warn, "cli").field("message", FieldValue::str("blocked")).build();
    let expected = event_to_json(&event);
    emit(event);

    clear_global_sink();

    let mut file = std::fs::File::open(&path).expect("reopen output");
    file.seek(SeekFrom::Start(0)).expect("rewind output");
    let mut text = String::new();
    file.read_to_string(&mut text).expect("read output");
    assert_eq!(text, format!("{expected}\n"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn vec_sink_still_collects_for_tests() {
    let _serial = serial_guard();
    let sink = VecSink::new();
    set_global_sink(Box::new(sink));
    emit(LogEvent::log(Level::Error, "test").field("message", FieldValue::str("x")).build());
    clear_global_sink();
}
