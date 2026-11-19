//! 分层计时与火焰数据：用显式 span 把一次运行拆成可归因的阶段。
//!
//! 本模块只做一件事：记录调用方声明的 span。它不做采样、不读信号、不抓
//! 调用栈，也不认识任何产品语义；名字由调用方给定，嵌套关系由 guard 的
//! 生命周期决定。因此它给出的结论是「这条代码路径花了多久」，而不是
//! 「CPU 在哪个指令上停顿」；两者互补，前者可复现、可跨机器对比。
//!
//! 每个 span 同时累计三个量：`total_ns`（含子 span）、`self_ns`（扣除子
//! span 的直接耗时）与 `hits`（进入次数）。[`Profiler::rows`] 按 `self_ns`
//! 降序给出热点，[`Profiler::folded`] 给出 folded stacks，可直接交给
//! `inferno` / `flamegraph.pl` 渲染火焰图；它是纯文本、逐字节确定，适合
//! 作为报告附件冻结。
//!
//! 时间来自 [`std::time::Instant`]，单位纳秒，单调但受平台时钟粒度影响：
//! 单次几微秒的 span 只应看 `hits` 与占比，不应把单点数值当成绝对耗时。
//! span 名字不能包含 `;`（folded stacks 的路径分隔符），构建期用
//! `debug_assert` 拒绝。

use std::{collections::BTreeMap, sync::Mutex, time::Instant};

/// 一次 profile 的聚合结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileReport {
    /// 顶层 span 的含子耗时之和，单位纳秒。顶层 span 允许重叠，重叠部分会被
    /// 重复累计；需要墙钟时间时请用单个包住全部工作的顶层 span。
    pub total_ns: u64,
    /// 每个 span 路径的统计行，按 `self_ns` 降序、同值按路径升序。
    pub rows: Vec<ProfileRow>,
}

/// 一条 span 路径的聚合行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    /// 从顶层到当前 span 的 `;` 连接路径。
    pub path: String,
    /// 当前 span 扣除直接子 span 后的耗时，单位纳秒。
    pub self_ns: u64,
    /// 当前 span 含子 span 的耗时，单位纳秒。
    pub total_ns: u64,
    /// 进入当前 span 的次数。
    pub hits: u64,
}

impl ProfileReport {
    /// 从统计行重建 folded stacks：只保留叶子路径，权重取含子耗时。
    ///
    /// 报告本身是序列化证据；重建 folded 让同一份证据在不重新采样的情况下
    /// 也能出图，且输出与 [`Profiler::folded`] 的排序一致（路径升序）。
    pub fn folded(&self) -> String {
        let mut leaves: Vec<&ProfileRow> = self
            .rows
            .iter()
            .filter(|row| row.total_ns > 0)
            .filter(|row| {
                let prefix = format!("{};", row.path);
                !self.rows.iter().any(|other| other.path.starts_with(&prefix))
            })
            .collect();
        leaves.sort_by(|left, right| left.path.cmp(&right.path));
        leaves.into_iter().map(|row| format!("{} {}\n", row.path, row.total_ns)).collect()
    }
}

/// 分层计时器，可跨线程共享。
///
/// 所有 span 共享一棵按名字分层的树；同一路径在多线程上并发进入时合并到
/// 同一行。锁只在进入和退出 span 时持有，临界区只做计数，不执行调用方的
/// 工作。
#[derive(Debug, Default)]
pub struct Profiler {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    stack: Vec<Frame>,
    root: Node,
}

#[derive(Debug)]
struct Frame {
    name: String,
    started: Instant,
    /// 已结束的直接子 span 耗时之和，用于计算 self time。
    children_ns: u64,
}

#[derive(Debug, Default, Clone)]
struct Node {
    total_ns: u64,
    self_ns: u64,
    hits: u64,
    children: BTreeMap<String, Node>,
}

/// 一个已打开的 span；drop 时记录。
///
/// guard 必须存活到被测代码结束；否则只会记录到提前 drop 的那一段。
#[must_use = "span 记录到 guard 被 drop 为止"]
pub struct SpanGuard<'a> {
    profiler: &'a Profiler,
}

impl Profiler {
    /// 创建一个空计时器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次 `body` 的执行，嵌套关系由调用位置决定。
    pub fn span<T>(&self, name: &str, body: impl FnOnce() -> T) -> T {
        let _guard = self.guard(name);
        body()
    }

    /// 打开一个 span，直到 guard 被 drop 时结束。
    ///
    /// 适合无法包成闭包的场景（例如借用守卫跨越多条语句）。
    pub fn guard(&self, name: &str) -> SpanGuard<'_> {
        debug_assert!(!name.contains(';'), "span 名字不能包含 folded stacks 的分隔符 `;`");
        {
            let mut state = self.lock();
            state.stack.push(Frame { name: name.to_owned(), started: Instant::now(), children_ns: 0 });
        }
        SpanGuard { profiler: self }
    }

    /// 清空全部记录，计时器可继续使用。
    pub fn reset(&self) {
        let mut state = self.lock();
        state.stack.clear();
        state.root = Node::default();
    }

    /// 当前聚合结果。
    pub fn report(&self) -> ProfileReport {
        let state = self.lock();
        let mut rows = Vec::new();
        collect(&state.root, &mut Vec::new(), &mut rows);
        rows.sort_by(|left, right| right.self_ns.cmp(&left.self_ns).then_with(|| left.path.cmp(&right.path)));
        ProfileReport { total_ns: state.root.total_ns, rows }
    }

    /// 当前统计行，等价于 [`Self::report`] 的 `rows`。
    pub fn rows(&self) -> Vec<ProfileRow> {
        self.report().rows
    }

    /// folded stacks：每个叶子路径一行 `"a;b;c <纳秒>"`，按路径升序。
    ///
    /// 只输出叶子，权重是该叶子的含子耗时；渲染器会把同名前缀合并成火焰图
    /// 的父框。输出逐字节确定，可作为报告附件做跨运行对比。
    pub fn folded(&self) -> String {
        let state = self.lock();
        let mut output = String::new();
        collect_folded(&state.root, &mut Vec::new(), &mut output);
        output
    }

    /// 把当前样本渲染成火焰图 SVG（根在底部）。
    pub fn flamegraph(&self, options: &crate::FlameOptions) -> crate::Result<String> {
        crate::flame::render(&self.folded(), crate::FlameOrientation::Flame, options)
    }

    /// 把当前样本渲染成深渊图/冰柱图 SVG（根在顶部）。
    pub fn icicle(&self, options: &crate::FlameOptions) -> crate::Result<String> {
        crate::flame::render(&self.folded(), crate::FlameOrientation::Icicle, options)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 结束最近打开的 span 并把它记到当前路径上。
    fn finish_top(&self) {
        let mut state = self.lock();
        let Some(frame) = state.stack.pop()
        else {
            // guard 与 push 一一对应；走到这里说明内部状态被破坏。
            debug_assert!(false, "span guard 没有对应的已打开 span");
            return;
        };
        let elapsed = frame.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let self_ns = elapsed.saturating_sub(frame.children_ns);
        // 拆成两个字段借用：stack 提供路径，root 接收累计。
        let State { stack, root } = &mut *state;
        if let Some(parent) = stack.last_mut() {
            parent.children_ns += elapsed;
        }
        else {
            // 顶层 span：它的耗时就是这次 profile 的墙钟上界。
            root.total_ns = root.total_ns.saturating_add(elapsed);
        }
        let node = node_at(root, &stack.iter().map(|frame| frame.name.as_str()).collect::<Vec<_>>(), &frame.name);
        node.total_ns = node.total_ns.saturating_add(elapsed);
        node.self_ns = node.self_ns.saturating_add(self_ns);
        node.hits = node.hits.saturating_add(1);
    }
}

impl Drop for SpanGuard<'_> {
    fn drop(&mut self) {
        self.profiler.finish_top();
    }
}

/// 沿 `path` 找到（必要时创建）节点，再进入 `leaf`。
fn node_at<'a>(root: &'a mut Node, path: &[&str], leaf: &str) -> &'a mut Node {
    let mut current = root;
    for name in path {
        current = current.children.entry((*name).to_owned()).or_default();
    }
    // 根节点本身不占一行；路径为空时 leaf 就是顶层 span。
    current.children.entry(leaf.to_owned()).or_default()
}

fn collect(node: &Node, path: &mut Vec<String>, rows: &mut Vec<ProfileRow>) {
    for (name, child) in &node.children {
        path.push(name.clone());
        rows.push(ProfileRow { path: path.join(";"), self_ns: child.self_ns, total_ns: child.total_ns, hits: child.hits });
        collect(child, path, rows);
        path.pop();
    }
}

fn collect_folded(node: &Node, path: &mut Vec<String>, output: &mut String) {
    for (name, child) in &node.children {
        path.push(name.clone());
        if child.children.is_empty() {
            output.push_str(&path.join(";"));
            output.push(' ');
            output.push_str(&child.total_ns.to_string());
            output.push('\n');
        }
        else {
            collect_folded(child, path, output);
        }
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(report: &'a ProfileReport, path: &str) -> &'a ProfileRow {
        report.rows.iter().find(|row| row.path == path).unwrap_or_else(|| panic!("缺少路径 {path}: {:?}", report.rows))
    }

    #[test]
    fn nested_spans_split_inclusive_and_self_time() {
        let profiler = Profiler::new();
        profiler.span("load", || {
            profiler.span("parse", || {});
            profiler.span("apply", || {});
        });
        let report = profiler.report();
        let load = row(&report, "load");
        let parse = row(&report, "load;parse");
        let apply = row(&report, "load;apply");
        assert_eq!(load.hits, 1);
        assert_eq!(parse.hits, 1);
        assert_eq!(apply.hits, 1);
        assert_eq!(load.total_ns, parse.total_ns + apply.total_ns + load.self_ns, "self 与子 span 必须覆盖整体");
        assert!(load.total_ns >= parse.total_ns);
        assert_eq!(report.total_ns, load.total_ns);
    }

    #[test]
    fn repeated_paths_accumulate_hits() {
        let profiler = Profiler::new();
        for _ in 0..3 {
            profiler.span("query", || {});
        }
        let report = profiler.report();
        assert_eq!(row(&report, "query").hits, 3);
        assert_eq!(report.rows.len(), 1);
    }

    #[test]
    fn folded_output_is_leaf_only_and_deterministic() {
        let profiler = Profiler::new();
        profiler.span("outer", || {
            profiler.span("inner", || {});
        });
        profiler.span("other", || {});
        let folded = profiler.folded();
        let lines: Vec<&str> = folded.lines().collect();
        assert_eq!(lines.len(), 2, "{folded}");
        assert_eq!(lines[0].split(' ').next(), Some("other"), "路径按字典序输出");
        assert_eq!(lines[1].split(' ').next(), Some("outer;inner"), "只输出叶子路径");
        assert!(lines.iter().all(|line| line.split(' ').nth(1).and_then(|ns| ns.parse::<u64>().ok()).is_some()));
    }

    #[test]
    fn reset_discards_every_record() {
        let profiler = Profiler::new();
        profiler.span("work", || {});
        profiler.reset();
        let report = profiler.report();
        assert!(report.rows.is_empty());
        assert_eq!(report.total_ns, 0);
        profiler.span("again", || {});
        assert_eq!(profiler.rows().len(), 1);
    }

    #[test]
    fn a_guard_records_until_it_is_dropped() {
        let profiler = Profiler::new();
        {
            let _guard = profiler.guard("stage");
            assert!(profiler.rows().is_empty(), "span 未结束前不产生行");
        }
        assert_eq!(profiler.rows().len(), 1);
    }

    #[test]
    fn profiler_renders_flame_and_icicle_svg() {
        let profiler = Profiler::new();
        profiler.span("outer", || {
            profiler.span("inner", || std::thread::sleep(std::time::Duration::from_millis(1)));
        });
        let flame = profiler.flamegraph(&crate::FlameOptions::default()).expect("flame svg");
        let icicle = profiler.icicle(&crate::FlameOptions::default()).expect("icicle svg");
        assert!(flame.contains("flame-frame") && flame.contains("outer") && flame.contains("inner"));
        assert!(icicle.contains("flame-frame"));
        assert_ne!(flame, icicle);
        assert_eq!(profiler.report().folded(), profiler.folded(), "报告重建的 folded 与直接输出一致");
    }
}
