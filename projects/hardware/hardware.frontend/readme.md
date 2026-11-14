# 🎛️ hardware-frontend

Language-neutral shader core. Per-language input crates convert their parser output into `hardware_frontend::ast::ShaderModule`, and this crate lowers that module into the generic UCF program IR.

## Boundary

- This crate never lexes or parses shader text. Parsing is owned by the `oak-*` parsers.
- Language spellings are owned by `hardware-input-*`: attributes, address spaces, constructors, and type names are normalized there.
- It never emits a target ISA. Every PTX / DXBC / SPIR-V / HSACO byte comes from `hardware-emitter`.
- Lowering is a translation, not an optimizer. Unsupported constructs return a structured error instead of producing a partial program.

## Pipeline

```text
WGSL / HLSL / GLSL text
  → oak-wgsl / oak-hlsl / oak-glsl
  → hardware-input-* (language spellings → neutral syntax)
  → hardware-frontend (symbol table, resource slots, bounded control flow)
  → hardware_program::Program
  → hardware-emitter
```

## Supported subset

Module scope: `struct`, `type alias`, module constants, storage bindings with group/binding attributes, and one compute entry.

Statements: local declarations, assignment and compound assignment, `if` / `else`, `for` with a literal bound, `while`, `loop`, `break`, `continue`, `return`, expression statements, and nested blocks.

Expressions: numeric and boolean literals, local and constant references, the global-invocation builtin's `.x`, storage reads and writes, `+ - * / %` and comparisons, `vec3` construction, `f32` / `u32` casts, and the `abs` / `sqrt` / `sin` / `cos` / `min` / `max` / `pow` / `dot` / `length` / `normalize` builtins.

Everything else is reported as [`FrontendError::Unsupported`].
