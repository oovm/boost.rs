# hardware-backend-dx12

DirectX 12 backend via raw **D3D12** API (`windows` crate).

- Device buffers (`prepare` / `write_f32` / `read_f32` / `read_u8`)
- `Copy` via `CopyBufferRegion`
- `Fill` / `MatMul` compute from [`hardware-emitter`](../../hardware-emitter/) DXBC (`cs_5_1`)
- `Raster` thin gate: RT clear / optional `draw=tri` yellow triangle → packed `RGBA8` (`read_u8`)
- `draw_vertex_buffer`: source-alpha blended vertex triangles with up to 4x MSAA resolved before readback, returning straight (non-premultiplied) RGBA8
- Windows only (`stub` on other targets)
- Degrade wiring: `strategies()` / `last_*_path` from `FeatureSet` picks (`ucf` `tests/dx12_degrade_wire.rs`)

Parity: `tests/parity_cpu.rs` soft-skips when no D3D12 device is available.
Raster: `tests/raster_clear.rs`, `tests/raster_tri.rs` soft-skip the same way.
Vertex raster: `tests/vertex_raster.rs` covers opaque, translucent, overlapping and diagonal antialiased geometry.
