# boost

Single entry point for the boost standard-library enhancement family. Each enabled feature opens a `boost::…` module that re-exports one runtime crate — same types, no duplicate wrappers.

Product overview and design constraints: [repository README](../../readme.md).

## Features

Default features are **empty**. Enable only what you need:

| Feature | Module | Underlying crate |
|---------|--------|------------------|
| `source` | `boost::source` | `boost-source` |
| `random` | `boost::random` | `boost-random` |
| `fmt` | `boost::fmt` | `boost-console` (`fmt` module only) |
| `terminal` | `boost::terminal` | `boost-terminal` |
| `console` | `boost::console` | `boost-console` plus `boost-harness::log::console` |
| `fs` | `boost::fs` | `boost-fs` |
| `log` | `boost::log` | `boost-harness::log` |
| `diagnostic` | `boost::diagnostic` | `boost-harness::diagnostic` |
| `test` | `boost::test` | `boost-harness::test` |
| `benchmark` | `boost::benchmark` | `boost-harness::benchmark` |

`console` opens the styling primitives plus log rendering, stderr sinks, and the default subscriber. `diagnostic` and `log` share one `boost-harness` dependency.

## Example

```toml
[dependencies]
boost = { version = "0.1", default-features = false, features = ["log", "console"] }
```

```rust
use boost::console::install_global_subscriber;
use boost::log::{Level, event};

fn main() {
    install_global_subscriber();
    event!(Info, "worker", message = "started");
}
```

## Direct crates

Libraries that expose only one concern should depend on `boost-harness`, `boost-console`, or another member crate directly. The facade is for applications and integration tests that want one manifest line and explicit feature toggles.

## Related packages

- [`boost-harness`](../boost.harness/readme.md) — log events, diagnostics, tests, and benchmarks
- [`boost-console`](../boost-console/readme.md) — terminal styling primitives
