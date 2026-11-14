# animator.core

图表与示意图的动画程序：把一串已经排好版的静态场景编译成一条时间轴，并按秒或帧取样出可绘制的场景。

`plotter.motion` 只回答“两个静态场景之间的一段过渡怎么插值”。真实的科普图表动画需要更长的结构：先出现坐标轴，数据柱逐段长高，再切到下一个数据状态。本 crate 在这层过渡之上补齐状态序列、每段过渡参数、时长与帧取样：

```text
状态序列 + 每段过渡 -> Program -> compile -> Compiled::sample(t) -> DisplayScene
```

## 边界

- 只依赖 `plotter.motion` 与 `plotter.scene`，不依赖数据、布局、输入语言或渲染器。
- 不加载数据、不排版、不读时钟，也不做事件派发；宿主传入时间，输出场景。
- 动画只继承 `plotter.motion` 已声明的通道：`Transform`、`Extent` 与 `Opacity`。`Extent` 让 `Rect` / `RoundedRect` 节点从共享基线长高；路径形变（`PathTrim`）与颜色渐变不在本层承诺范围内。
- 每段过渡可以显式给出通道集合，缺省为三个通道全开；不支持的节点类型不会凭空获得几何通道。
- 状态顺序、错误与输出都是确定的：同样输入永远得到同样的场景。

## 验收

`cargo test -p animator-core` 覆盖保持段、过渡段、时长为零、边界钳制、帧取样、矩形长高与非法程序。
