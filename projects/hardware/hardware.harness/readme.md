# 🔬 hardware-harness

Tracing and failure analysis for UCF execution. Backends, the scheduler, and
render paths record what they actually did, and this crate turns that record
into a readable report plus concrete findings.

## Boundary

- Owns no hardware state and loads no driver. It only models events.
- Does not capture timings by itself except through [`Tracer::span`], which wraps
  a closure the caller already owns.
- Analysis is rule-based and explainable: every finding keeps the sequence
  numbers it came from.

## Typical use

```rust
use hardware_harness::{EventKind, Tracer, analyze, render_markdown};

let mut tracer = Tracer::new();
tracer.record(EventKind::BackendSelected { backend: "cpu".into(), reason: "default".into() });
let trace = tracer.finish();

let findings = analyze(&trace, &Default::default());
println!("{}", render_markdown(&trace, &findings));
```

## What it can detect

- No backend available or no backend selected.
- Shader emission and dispatch failures, with the ISA and message.
- Readback mismatches against a reference value.
- Capability gaps and degradation paths.
- Dispatches slower than the configured budget.
- Empty traces, which usually mean instrumentation was never wired.
