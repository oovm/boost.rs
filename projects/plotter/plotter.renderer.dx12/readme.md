# `plotter.renderer.dx12`

Optional accelerated backend: turns `plotter-view` scenes into a triangle list
and rasterises it through the repository's own Direct3D 12 pipeline
(`hardware-backend-dx12`, DXBC emitted in-house). No external GPU runtime and no
shader text are involved.

Public surface:

- `is_available()` — probes a D3D12 device, `false` when none exists or off Windows
- `render_rgba8(scene, width, height)` — RGBA8 readback on a transparent background

Node coverage: polyline (offset quad per segment), points (cross), 2D mesh,
projected `Mesh3` / `Points3` (painter order), axis (line plus ticks). Text has no
triangle conversion and fails with `RenderError::UnsupportedNode` instead of being
dropped silently.

```sh
cargo test -p plotter-renderer-dx12
```
