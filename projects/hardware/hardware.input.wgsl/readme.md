# 🧩 hardware-input-wgsl

WGSL input for the UCF pipeline. Text is parsed by `data-text-wgsl`, converted into the language-neutral shader syntax owned by `hardware-frontend`, and lowered into UCF IR.

## Boundary

- Owns WGSL spellings only: attribute names, address spaces, constructors, and type names.
- Emits no target ISA. PTX, DXBC, SPIR-V, and HSACO all come from `hardware-emitter`.
- Unsupported constructs return a structured error instead of a partial program.

## Example

```rust
use hardware_input_wgsl::lower_wgsl;

let source = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(4, 1, 1)
fn fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    out[gid.x] = 1.5;
}
"#;

let lowered = lower_wgsl(source).expect("valid WGSL");
assert_eq!(lowered.entry, "fill");
```
