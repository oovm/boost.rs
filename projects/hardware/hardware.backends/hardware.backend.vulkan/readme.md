# hardware-backend-vulkan

Vulkan compute backend using the in-crate driver binding (`vulkan-1` / `libvulkan`).

- Device buffers (`prepare` / `write_f32` / `read_f32` / `read_u8`)
- `Copy` via `vkCmdCopyBuffer`
- `Fill` / `MatMul` compute from [`hardware-emitter`](../../hardware-emitter/) SPIR-V
- `Raster` thin gate: `vkCmdClearColorImage` / optional `draw=tri` → packed `RGBA8` (`read_u8`)
- Soft-skips when no Vulkan device is available (`tests/parity_cpu.rs`, `tests/raster_clear.rs`, `tests/raster_tri.rs`)
- Degrade wiring: `strategies()` / `last_*_path` from `FeatureSet` picks (`ucf` `tests/vulkan_degrade_wire.rs`)
