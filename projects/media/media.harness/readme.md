# 🔬 media-harness

媒体执行追踪与失败分析：渲染与合成链把实际发生的事记录成事件与 span，本 crate 把它变成可解释的 findings 和 Markdown 报告。

## 边界

- 不打开解码器、编码器或 GPU，不持有媒体状态，只建模调用方观察到的事实。
- 不在内部计时，唯一的时间来源是 `Tracer::span` 包裹调用方已经拥有的闭包。
- 分析是规则式、可解释的：每条 finding 保留来源事件序号与下一步建议。
- 电平测量（`measure_pcm16` / `measure_f32`）只做观测，不做混音、归一化或门限处理。

## 事件模型

- 阶段：`Load` → `Validate` → `Synthesize` → `Mix` → `Paint` → `Encode` → `Publish`。
- 资源：加载成功、必需缺失、可选缺失（文档化静默）、解码失败、字体解析/缺失。
- 声音：合成成功（时长、峰值、RMS、削波）与失败；混音成功与失败。
- 画面：场景绘制帧数、漏帧、绘制失败。
- 输出：编码器打开、编码完成、编码失败、取消、发布成功/失败、降级。
- 自由备注：`Note`。

## 典型用法

```rust
use media_harness::{EventKind, Tracer, analyze, render_markdown};

let mut tracer = Tracer::new();
tracer.record(EventKind::ProjectLoaded { scenes: 2, frames: 180, fps: 30, width: 1920, height: 1080 });
tracer.span("paint frames", |tracer| {
    tracer.record(EventKind::ScenePainted { scene: 0, frames: 180 });
});
let trace = tracer.finish();

let findings = analyze(&trace, &Default::default());
println!("{}", render_markdown(&trace, &findings));
```

## 能发现什么

- 必需资源缺失、解码失败、字体不可用与项目校验失败。
- 合成与混音失败，以及合成/混音峰值达到 0 dBFS 的削波。
- 非平凡时长内混音接近数字静音。
- 绘制或编码失败、漏帧、取消与发布失败。
- 降级路径（例如 GPU → CPU、编码器回退）。
- 超过预算的 paint / synthesize / mix / encode span。
- 空 trace：通常意味着探针没有真正接进执行链。

## 与上层的关系

本 crate 不依赖任何 `media.*` 领域 crate；`media.render` 以可选 `trace` feature 依赖它并提供 traced 入口，宿主只需把 `Trace` 交给 `analyze` / `render_markdown`。
