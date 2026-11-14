# plotter.depiction

`MoleculeSpec / ReactionSpec → DepictionPlan → SemanticScene → DisplayScene` 的化学结构
描绘链，与 `plotter.diagram` / `plotter.chart` / `plotter.engraving` 平行，只共享
`plotter.types` + `plotter.scene`。

- 本 crate 不含语言前端：SMILES / MDL molfile 的解析在 `plotter.input.*`（语法归 Oak）。
- 配色由 [`DepictionOptions::palette`](src/plan.rs) 选择：`Palette::Element`（默认）是
  行业通行的 2D 元素配色——键保持黑色细线，杂原子标签按元素着色（氧红、氮蓝、硫黄、
  卤素绿紫）；`Palette::Ink` 是全黑单色，适合印刷。
- v0 布局是确定性的环优先平面布局：3–8 元简单环按正多边形放置，稠环按共享键反向展开，
  链原子按 zigzag（±60°）优先、直行兜底、回头最后的顺序贪心避碰；无法放置时报
  `davinci.depiction.unplaceable`。
- 诊断：`davinci.depiction.unsupported` / `davinci.depiction.unplaceable` 整例拒绝，
  `davinci.depiction.approximated` 接受但记录命名损失。
- 原子标签用正文字体，仅杂原子与带电荷/同位素的碳绘制符号；碳骨架默认不标符号。
- CIP 立体化学、大环、聚合物、Markush 基团与 3D 投影不在 v0 范围内，fail-closed。
