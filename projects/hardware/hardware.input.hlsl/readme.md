# 🧩 hardware-input-hlsl

HLSL input for the UCF pipeline. Text is parsed by `oak-hlsl`, converted into the language-neutral shader syntax owned by `hardware-frontend`, and lowered into UCF IR.

## Mapping

| HLSL | Neutral |
|------|---------|
| `[numthreads(x, y, z)]` | `workgroup_size(x, y, z)` plus `compute` |
| `SV_DispatchThreadID` | `builtin(global_invocation_id)` |
| `register(uN)` / `register(tN)` | `binding(N)` with `group(0)` |
| `RWStructuredBuffer<T>` | `storage` + `read_write` binding of `array<T>` |
| `StructuredBuffer<T>` | `storage` + `read` binding of `array<T>` |
| `float`, `float2..4`, `uint`, `int` | `f32`, `vec2..4`, `u32`, `i32` |

`RWByteAddressBuffer` and `cbuffer` are not lowered yet and return a structured error.
