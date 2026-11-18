# `schema-types`

Foundational schema models shared by every VOS consumer: canonical types and
scalars, durable identity manifests and bindings, resolved contracts with
deterministic semantic fingerprints, artifact envelopes with stage constants,
structured diagnostics, and the `bytes` / `file` / `vector` value models. This
crate has no dependency on Oak or on any processing facade.

Parsing, Oak projection, name resolution and identity evolution are provided by
`schema-input-vos`. Hosts access models through `schema_input_vos::types` or its
root exports. Artifact formats retain their own version identifiers
independently of Rust crate names.
