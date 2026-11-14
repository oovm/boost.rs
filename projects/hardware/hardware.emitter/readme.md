# hardware-emitter

UCF backend emitter. Legacy `ShaderProgram` operations remain available for compatibility.
Generic UCF `Program` values are lowered from the UCF IR directly to backend-loadable bytes.

| ISA | Bytes | Loaded by |
|-----|-------|-----------|
| PTX | text | CUDA Driver (`cuModuleLoadData`) |
| DXBC | bytecode | D3D12 |
| SPIR-V | words | Vulkan |
| HSACO | ELF code object | HIP (`hipModuleLoadData`; OpenCL ICD used only inside emitter) |

Authors never write PTX / HLSL / OpenCL / HIP C, and domain crates never write shader source.
Every backend blob is assembled by this crate: the SPIR-V encoder lives in `src/spv.rs`, the DXBC
container and token encoder in `src/dxbc.rs`, no shader source is parsed and no external shader
compiler is invoked. Backends only load and launch the blobs.
