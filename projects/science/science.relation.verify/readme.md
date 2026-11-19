# `science-relation-verify`

Relation path/proof search and verification

Path search and proof verification for Relation islands (object-logic layer).

- **EQ**: reflexivity, symmetry, transitivity of `Equal` judgment edges.
- **ZFC-lite**: finite sets — see [`zfc`].

Core-aligned queries use [`query_equal`] / [`query_member`] and return
[`QueryStatus`]. Absence of a path is [`QueryStatus::Unknown`], never automatic negation.

[`Goal::NotEqual`] / [`Goal::NotMember`] remain as **finite-fragment** helpers that treat
missing witnesses as negation. They must not be used as Relation's general negation semantics.
