# 🧩 hardware-input-glsl

GLSL input for the UCF pipeline. Text is parsed by `oak-glsl`, converted into the language-neutral shader syntax owned by `hardware-frontend`, and lowered into UCF IR.

## Mapping

| GLSL | Neutral |
|------|---------|
| `layout(local_size_x = N) in;` | `workgroup_size(N, 1, 1)` plus `compute` on `main` |
| `gl_GlobalInvocationID` | `builtin(global_invocation_id)` |
| `layout(binding = N)` | `binding(N)` with `group(0)` |
| `buffer` block | `storage` binding of `array<T>` |
| `uniform` block | `uniform` binding typed by the block name, one slot per scalar member |
| `uniform float x;` | scalar `uniform` binding |
| `readonly` / `writeonly` | `read` / `write` access |
| `float`, `vec2..4`, `uint`, `int` | `f32`, `vec2..4`, `u32`, `i32` |

Interface `in`/`out` variables and `switch` are not lowered yet and return a structured error. Uniform blocks with nested or array members are also rejected.
