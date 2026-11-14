# `boost-terminal`

Platform terminal device capabilities: virtual terminal processing, ANSI support probes, and future TTY helpers.

The crate is `no_std`. Platform probing is behind the default `std` feature; without `std` the crate assumes ANSI support and does not touch the host.

See `src/readme.md` for what this crate owns and what belongs to `boost-console::fmt` or `boost_harness::diagnostic`.
