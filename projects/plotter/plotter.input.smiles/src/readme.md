# plotter-input-smiles

SMILES 前端：Oak AST → [`MoleculeSpec` / `ReactionSpec`](../plotter.depiction/readme.md) / scene。

纯分子与反应式都支持：`.` 分隔的片段合并成一个分子图（断开组分由描绘链横向排开），
`>` / `>>` 分隔的反应式降成 `ReactionSpec`，试剂片段按箭头条件文本保留。

立体信息（`@` / `@@` 与 `/` `\`）没有 v0 描绘表示，键级 `~` 也没有——两者都按
`davinci.depiction.approximated` 记录并给出源字节跨度，画面退化为无立体标记的平面图。
`*` 通配原子按 `*` 标签描绘。本 crate 不拼 scene 节点或 SVG。

