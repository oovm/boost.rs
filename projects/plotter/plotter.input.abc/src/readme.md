# plotter-input-abc

ABC 记谱前端：Oak AST → [`ScoreSpec`](../plotter.engraving/readme.md) / scene。

只接受 v0 engraving 能表达的单一谱表子集：单 tune、单 voice、高音或低音谱号、大调或小调调号、
1/2/4/8/16 单位拍。多 tune、多 voice、中音谱号、教会调式调号以结构化诊断拒绝或记
`davinci.engraving.approximated` 命名损失（附字节跨度）。装饰音只降 `staccato` / `tenuto`，
其余按 approximations 记录；连音、圆滑线、连音组、歌词与和弦均映射到 `ScoreSpec`。

本 crate 不拼 scene 节点或 SVG，也不解析文本语法——语法归 `oak-abc`。

