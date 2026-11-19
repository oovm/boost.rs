//! Capability probes must answer without panicking on any host, including
//! when standard streams are pipes or closed files under a test harness.

use std::time::{Duration, Instant};

use boost_terminal::{ReadEvent, TerminalError, disable_echo, is_tty_input, is_tty_output, read_char_timeout};

#[test]
fn tty_probes_answer_without_panic() {
    let _input: bool = is_tty_input();
    let _output: bool = is_tty_output();
}

#[test]
fn echo_guard_reports_capability_when_not_a_tty() {
    if !is_tty_input() {
        assert!(matches!(disable_echo(), Err(TerminalError::EchoUnavailable)), "echo control must refuse on a non-tty instead of pretending");
    }
    else {
        let guard = disable_echo().expect("echo control must succeed on a real console");
        drop(guard);
    }
}

#[test]
fn bounded_read_never_hangs() {
    let started = Instant::now();
    let event = read_char_timeout(50);
    assert!(started.elapsed() < Duration::from_secs(5), "a 50 ms bound must be honored");
    assert!(matches!(event, ReadEvent::Byte(_) | ReadEvent::Eof | ReadEvent::Timeout));
}

#[test]
fn zero_timeout_polls_immediately() {
    let started = Instant::now();
    let _ = read_char_timeout(0);
    assert!(started.elapsed() < Duration::from_secs(2));
}
