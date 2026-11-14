# diagnostic

Structured diagnostic facts for the rust-boost workspace.

## Modules

```text
model     one diagnostic fact
collect   DiagnosticSet and DiagnosticSink
wire      versioned serialization
source    opaque SourceRef contracts
render    renderer-neutral projection helpers
terminal  optional text rendering via `boost-console` and `boost-terminal`
```

## Core

- `Diagnostic`, `DiagnosticCode`, `DiagnosticSeverity`, `DiagnosticOrigin`
- `DiagnosticLocation`, `DiagnosticLabel`, `Message`, `DiagnosticAction`
- `DiagnosticSet`, `DiagnosticSink`

Enable optional features explicitly:

| Feature | Purpose |
|---|---|
| `serde` | JSON wire encoding via `boost_harness::diagnostic::wire` |
| `terminal` | Text rendering via `boost-console` and `boost-terminal` |

Log projection is always available through `boost_harness::log`.

The default feature set is empty. Parser cores, WASM builds, and binary kernels should not enable terminal rendering unless they write to a TTY.

## Boundaries

The core crate does not provide business result wrappers, quick error facades,
source storage, or terminal policy. Enable the `terminal` feature to render text
diagnostics through `boost-console` and `boost-terminal`. Oak, Acorn, and Panduck keep their
own result types and only share `DiagnosticSet` as the common diagnostic container.
