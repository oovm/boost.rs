# plotter-input-mdl

MDL molfile / SDF 前端：Oak AST → [`MoleculeSpec`](../plotter.depiction/readme.md) / scene。

只描绘 v0 平面图：作者写的 2D/3D 坐标被确定性的描绘布局替代，因此每条记录都会记一条
`davinci.depiction.approximated`；质量差、立体奇偶与 `M  RAD` 等无表示字段同样记命名损失。
`M  CHG` 与 `M  ISO` 覆盖原子行上的电荷与同位素，`MdlBondOrder::Other` 退化为单键并记录原因。
SDF 多记录只描绘第一条并记录被跳过的记录数。原子下标从 1 基转成 0 基，越界整例拒绝。

