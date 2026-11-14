# plotter

Plotter 稳定公共门面：再导出 `plotter-*` 各层类型与入口， **不**新增业务语义。默认渲染为 SVG / CPU；可选 `dx12` feature 接入自研 D3D12 后端。

架构与资产边界见上级 [`../readme.md`](../readme.md)；产品总览见根 [`../../../readme.md`](../../../readme.md)。

## Features

| Feature       | 作用                                   |
|---------------|----------------------------------------|
| `svg`（默认） | `plotter::renderer::svg`               |
| `dx12`        | `plotter::renderer::gpu` 走自研 D3D12 后端（仅 Windows 有效） |

## 再导出表面

| 模块                                                 | 底层                     |
|------------------------------------------------------|--------------------------|
| `types` / `data` / `grammar` / `view` / `layout`     | GoG 链                   |
| `renderer`（含 `svg` / `gpu`）                       | 渲染合同与后端           |
| `scene` / `diagram` / `chart` / `runtime` / `motion` | 矢量场景与图表           |
| `input::{mermaid,dot,plantuml,tikz}`                 | 语言前端 → `DiagramSpec` / scene |

`renderer::gpu::is_available()` 默认构建恒为 `false`，`renderer::gpu::render` 返回 `GpuError::Unavailable`；启用 `dx12` feature 且探测到 D3D12 设备时走真实渲染，文本节点按系统字体在 CPU 栅格后合成，缺字体或缺字形返回结构化错误而不是被静默丢弃。

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
