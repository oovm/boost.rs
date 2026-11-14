# plotter

Plotter 稳定公共门面：再导出 `plotter-*` 各层类型与入口， **不**新增业务语义。渲染为 SVG / CPU；GPU 后端待 D3D12 内部实现。

架构与资产边界见上级 [`../readme.md`](../readme.md)；产品总览见根 [`../../../readme.md`](../../../readme.md)。

## Features

| Feature       | 作用                      |
|---------------|---------------------------|
| `svg`（默认） | `plotter::renderer::svg`  |

## 再导出表面

| 模块                                                 | 底层                     |
|------------------------------------------------------|--------------------------|
| `types` / `data` / `grammar` / `view` / `layout`     | GoG 链                   |
| `renderer`（含 `svg` / `gpu`）                       | 渲染合同与后端           |
| `scene` / `diagram` / `chart` / `runtime` / `motion` | 矢量场景与图表           |
| `input::{mermaid,dot,plantuml,tikz}`                 | 语言前端 → `DiagramSpec` / scene |

`renderer::gpu::is_available()` 恒为 `false`，`renderer::gpu::render` 返回 `GpuUnavailable`，GPU 请求不会被静默降级。

## Example

```rust
use plotter::grammar::{CompileOptions, PlotSpec, compile_plot};
use plotter::data::ColumnTable;

let table = ColumnTable::new()
    .push_float("x", vec![1.0, 2.0])?
    .push_float("y", vec![3.0, 5.0])?;
let plot = PlotSpec::new(table);
let _scene = compile_plot(&plot, CompileOptions::default())?;
```
