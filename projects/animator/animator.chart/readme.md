# animator.chart

图表数据动画：把同一张图表的多个数据快照编译成 `animator.core` 的时间轴。

```text
ChartSpec + 多份 DataSnapshot（各自带时间）
  -> 每份快照按 plotter.chart 编译并解析成 DisplayScene
  -> animator.core 的状态序列
  -> Compiled::sample(t) / sample_frame(fps, frame)
```

## 边界

- 只负责“数据快照 → 场景状态”的编译，不发明新的插值：过渡仍由 `animator.core`（也就是
  `plotter.motion`）执行，通道为 `Transform`、`Extent` 与 `Opacity`。
- 不加载数据、不排版、不渲染、不读时钟。
- 图元稳定 id 来自 `plotter.chart` 的命名（`chart-bar-{i}`、`chart-line-{i}` 等），
  所以插入或过滤系列后，动画仍按 id 对应而不是按下标。

## 共享定义域

`ChartTimeline::shared_domain = true` 时，所有状态的连续位置通道共用由全部快照
求并集得到的定义域（柱与面积包含零）。默认 `false` 保持“每份快照各自推断”的
行为：值翻三倍、轴也翻三倍，柱高在像素上可能完全不动；要做“柱子长高”的动画必须
开启共享定义域，再配合 `animator.core` 的 `Extent` 通道。

## 验收

`cargo test -p animator-chart` 覆盖空快照拒绝、状态数量与过渡数量校验、起始/结束/中间取样不同、
共享定义域下的柱高线性插值，以及同一输入重复编译的确定性。
