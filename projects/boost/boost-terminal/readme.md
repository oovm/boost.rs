# `boost-terminal`

Platform terminal device capabilities: virtual terminal processing, TTY probes, an echo save-and-restore guard, and pollable single-character reads.

The crate is `no_std`. Platform probing and terminal interaction are behind the default `std` feature; without `std` the crate assumes ANSI support and does not touch the host. Targets or streams without echo control report a structured `TerminalError::EchoUnavailable` instead of pretending the operation succeeded.

See `src/readme.md` for what this crate owns and what belongs to `boost-console::fmt` or `boost_harness::diagnostic`.
