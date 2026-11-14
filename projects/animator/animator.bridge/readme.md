# animator.bridge

把 `animator.core` 的时间轴逐帧取样，并交给 `plotter.scene` 的 SVG 序列化，得到可交付的
帧序列与确定性指纹。

```text
Compiled -> plan_frames(fps) -> 逐帧 DisplayScene -> to_svg() -> frame-0000.svg ...
```

## 边界

- 只做“取样 → SVG 文档”的转送，不插值、不改几何、不重新排版。
- 帧内容来自 `Compiled::sample_frame`，与直接调用 `DisplayScene::to_svg` 完全同源。
- 不读时钟、不并发、不决定文件名以外的目录结构。
- 画布尺寸在整条时间轴上必须一致；尺寸变化显式失败，不静默缩放。

## 验收

`cargo test -p animator-bridge` 覆盖：帧数由 `frame_count` 决定、单帧 SVG 与同一时刻静态
场景逐字节一致、写出文件的命名与内容、重复写出指纹不变、`fps == 0` 与越界帧被拒绝、
尺寸变化的程序被拒绝。
