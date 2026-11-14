# 🎛️ hardware-frontend

UCF shader front-end. Authored WGSL text is parsed by `oak-wgsl` into a typed syntax tree, and this crate lowers that tree into the generic UCF program IR owned by `hardware-program`.

## Boundary

- This crate never lexes or parses shader text itself.
- It never emits a target ISA. Every PTX / DXBC / SPIR-V / HSACO byte comes from `hardware-emitter`.
- Lowering is a translation, not an optimizer. Unsupported constructs return a structured error instead of producing a partial program.

## Pipeline

```text
WGSL text
  → oak-wgsl typed AST
  → hardware-frontend (symbol table, resource slots, bounded control flow)
  → hardware_program::Program
  → hardware-emitter
```

## Supported subset

Module scope: `struct`, `alias`, module `const`, `var<storage, ...>` bindings with `@group` / `@binding`, and one `@compute` entry.

Statements: `var` / `let` / `const`, assignment and compound assignment, `if` / `else`, `for` with a literal bound, `while`, `loop`, `break`, `continue`, `return`, expression statements and nested blocks.

Expressions: numeric and boolean literals, local / constant references, `@builtin(global_invocation_id)` `.x`, storage reads and writes, `+ - * / %` and comparisons, `vec3` composition, `f32` casts, and the `abs` / `sqrt` / `sin` / `cos` / `min` / `max` / `pow` / `dot` / `length` / `normalize` builtins.

Everything else is reported as [`FrontendError::Unsupported`].

## Example

```rust
use hardware_frontend::lower_wgsl;

let source = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(4, 1, 1)
fn fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    out[gid.x] = 1.5;
}
"#;

let lowered = lower_wgsl(source).expect("valid WGSL");
assert_eq!(lowered.entry, "fill");
assert_eq!(lowered.resources[0].binding, 0);
```
