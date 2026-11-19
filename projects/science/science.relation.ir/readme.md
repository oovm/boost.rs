# `science-relation-ir`

Typed relation plans, bytecode format, and structural validation

Typed relation plans and bytecode for Relation.

This crate owns plan structure, register kinds, effects, and structural
validation. It does **not** hold an active heap, parse source, execute
instructions, or define admission. Interpreter and JIT share this IR.
