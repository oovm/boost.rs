# boost-console

Terminal text styling primitives: colors, paint, style, and stream-aware formatting.

The crate is `no_std` with `alloc`; styling never requires the host operating system.

Log-event rendering, stderr sinks, and `install_global_subscriber` now live in `boost-harness::log::console`.

Repository context: [boost README](../../readme.md).

## Features

| Feature | What you get |
|---------|----------------|
| `fmt` (default) | `boost_console::fmt::*` |
| `concolor` | External color forcing hooks |

## When to use

| Need | Import |
|------|--------|
| ANSI colors, paint, display width for diagnostics or CLIs | `boost_console::fmt` or `boost::fmt` |
| Render `LogEvent` to stderr / install default subscriber | `boost_harness::log::console` or `boost::console` |
| Emit events, filters, file sinks | `boost_harness::log` |

## Example

```rust
use boost_console::fmt::{Color, Paint};

let line = Paint::green("ready").to_string();
```

## Related packages

- [`boost-harness`](../boost.harness/readme.md) — log events and human-visible rendering
- [`boost`](../boost/readme.md) — `fmt` and `console` features
