# `interpreter-gc`

Language-agnostic managed object heap: layout descriptors, roots, write barriers, generational and concurrent-mark protocols

Managed object heap and collector for interpreter runtimes.

语言无关：不依赖任何具体语言前端 crate。guest 布局由加载器转换为
[`LayoutDescriptor`]；策略提示见 [`GcPolicy`] / [`WorkloadHints`]。
宿主持久根见 [`HostRoots`] / [`RootHandle`]；分代标签见 [`Generation`]。
工作负载意图见 [`WorkloadIntent`] / [`StrategyController`]。
并发标记协议骨架见 [`ConcurrentMarkController`]；SATB 缓冲挂在 [`WriteBarrier`]。
