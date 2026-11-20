# Valkyrie formatter

Public module: `language_valkyrie::formatter` (CST rules live in private `formatter/` submodules).

- [`FormatOptions`](options.rs) — `indent_width`, `line_width`
- [`FormatError`](error.rs) — formatting failure message
- [`format_source`](engine.rs) — `source` in, formatted `String` out

Pipeline: lossless `ValkyrieLexer` → token-gap rules via `language_format::gap` → `apply_edits`.
