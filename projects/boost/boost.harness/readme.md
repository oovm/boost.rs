# `boost-harness`

Structured log events, diagnostics, tests, fixtures, and benchmarks for the rust-boost family.

The harness is one library with four explicit modules:

| Module | Owns |
|--------|------|
| `boost_harness::log` | `LogEvent`, spans, filters, sinks, task identity, and stderr rendering |
| `boost_harness::diagnostic` | Diagnostic facts, collections, wire encoding, and terminal rendering |
| `boost_harness::test` | Test identity, assertions, fixtures, cases, and structured results |
| `boost_harness::benchmark` | Explicit samples, warmup, and structured benchmark reports |

```rust
use boost_harness::log::{Level, VecSink, event, set_global_sink};

let sink = VecSink::new();
set_global_sink(Box::new(sink));
event!(Info, "worker", message = "ready");
```

Stderr rendering and the default subscriber live in `boost_harness::log::console`. Diagnostics project into
log events through `boost_harness::diagnostic` without reversing the module boundary.

The test attributes `#[test]`, `#[fixture]`, and `#[case]` are re-exported from `boost_harness::test`, and the
`diagnose!` macro expands against `boost_harness::diagnostic`.

## Features

| Feature | What you get |
|---------|----------------|
| `std` (default) | `log`, `test`, `benchmark`, diagnostic log projection, and console rendering |
| `serde` | Diagnostic wire encoding |
| `terminal` | Diagnostic terminal rendering (implies `std`) |
| `concolor` | Console color forcing hooks (implies `std`) |

Without `std` the crate is `no_std` + `alloc` and exposes the diagnostic core plus the log event
model (`LogEvent`, fields, payloads, spans, tasks, filters, and the sink trait). Global dispatch,
file and JSON sinks, async sinks, console rendering, tests, and benchmarks need host time, threads,
panics, and stdout, so they stay behind `std`.
