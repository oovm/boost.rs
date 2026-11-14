# `boost-console`

## `fmt` module owns

- color, paint, and style values
- stream-aware foreground/background wrappers
- ANSI formatting helpers

`fmt` depends only on the standard library.

## Does not own

- `LogEvent`, spans, filters, or generic sinks (`boost-harness::log`)
- Log rendering, stderr sinks, or the default subscriber (`boost-harness::log::console`)
- TTY detection, ANSI capability, or device handles (`boost-terminal`)
