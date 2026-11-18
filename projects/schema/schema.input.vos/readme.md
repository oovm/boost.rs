# `schema-input-vos`

VOS schema input chain: consumes the formal `oak-vos` syntax product and drives
the four semantic stages — schema/operation/service projection, durable identity
binding, `using` resolution across source units, and the resolved contract with
fingerprint. Owns the identity manifest provider and the `.vos/identities.json`
registry layout.

The shared models live in `schema-types`; syntax facts belong exclusively to
Oak. Hosts such as ODM, database engines and code generators must call
`parse_oak` / `validate_schema` and consume the resolved contract APIs instead
of re-implementing any stage.

This crate keeps only smoke tests. The full conformance corpus — fixtures,
golden contracts, identity/fingerprint and negative matrices — runs downstream
in the `vos-language` repository's `vos-conformance` crate against the pinned
core, together with the detailed `schema-types` model tests.
