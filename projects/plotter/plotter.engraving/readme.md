# plotter.engraving

`ScoreSpec → EngravingPlan → SemanticScene → DisplayScene` 的乐谱 engraving 链，与
`plotter.diagram` / `plotter.chart` 平行，只共享 `plotter.types` + `plotter.scene`。

- 本 crate 不含语言前端：ABC / MusicXML 的解析在 `plotter.input.*`（语法归 Oak）。
- 场景字形按 SMuFL 码位发出，`font-family` 为 `Bravura`，字体字节由下游（VuTeX 字体包）注入。
- 诊断两分模型：`davinci.engraving.unsupported` / `davinci.engraving.empty-score` 整例拒绝，
  `davinci.engraving.approximated` 接受但记录命名损失。
- v0 场景：单谱表、高音/低音谱号、调号 0–7、常见拍号、白名单内记号。多声部跨谱表、
  曲中改调改拍、`%%` 指令、三十秒音符及更短时值均 fail-closed。
