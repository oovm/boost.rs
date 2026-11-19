# 🧩 hardware-input-hlsl

HLSL input for the UCF pipeline. Text is parsed by `data-text-hlsl`, converted into the language-neutral shader syntax owned by `hardware-frontend`, and lowered into UCF IR.

## Mapping

| HLSL | Neutral |
|------|---------|
| `[numthreads(x, y, z)]` | `workgroup_size(x, y, z)` plus `compute` |
| `SV_DispatchThreadID` | `builtin(global_invocation_id)` |
| `register(uN)` / `register(tN)` | `binding(N)` with `group(0)` |
| `RWStructuredBuffer<T>` | `storage` + `read_write` binding of `array<T>` |
| `StructuredBuffer<T>` | `storage` + `read` binding of `array<T>` |
| `uniform` global (scalar or struct) | `uniform` binding, one slot per scalar member |
| `float`, `float2..4`, `uint`, `int` | `f32`, `vec2..4`, `u32`, `i32` |

`RWByteAddressBuffer` is not lowered yet and returns a structured error. `cbuffer` blocks are skipped by `data-text-hlsl` before the AST exists, so their members never reach this front-end and reads of them fail as unknown bindings.
