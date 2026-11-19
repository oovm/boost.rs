# `data-binary-host`

Minimal host executable wrappers per platform (PE / ELF / Mach-O) for the `data.binary` domain

`data-binary-host` — minimal host executable wrappers per platform.

Dispatches `(platform, native_logic, rodata_tail)` onto the container
encoders owned by `data-binary-x64` (`PE32+` / `ELF64`) and
`data-binary-mach-o` (`Mach-O`). Only the dispatch and the minimal entry
stub live here: container layout is never re-implemented, and no linking,
loading or execution is performed.

The code originates from the former `acorn.rs` project (imported at commit
`beccbdf7`, MPL-2.0) and has been internalized and de-branded for Boost.
