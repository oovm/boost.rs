# `plotter.renderer`

渲染器合同与 CPU 确定性 reference：

- `Renderer` trait
- `PreparedScene` / `RenderTarget` / `RgbaImage`
- `CpuRasterRenderer` / `render_rgba8`
- `walk_drawables`（供各 backend 复用）

本 crate 只定义渲染器边界和 CPU 参考实现。SVG 是独立后端，位于 `plotter.renderer.svg`，不会把后端类型泄漏到
Scene IR。加速后端由门面侧的 `plotter::renderer::gpu` 接入：默认构建 `is_available()` 为 `false`、`render` 返回
结构化错误 `GpuError::Unavailable`；启用 `dx12` feature 后走自研 D3D12 后端，请求 GPU 渲染时不会静默退化为 CPU。

```sh
cargo test -p plotter-renderer
```
