//! DL 执行路径验收台：对照 eager、放置计划与调优三条路径。
//!
//! 本 crate 只做对照与报告，不定义任何执行语义：`Graph::run` 是参考实现，计划执行与
//! 调优执行必须在给定容差内与它一致；重复执行之间也必须一致（确定性）。findings 可枚举，
//! [`render_markdown`] 产出可读报告。任何一条路径失败都会落成 finding，而不是静默跳过。

#![warn(missing_docs)]

use dl_graph::{Bindings, Graph};
use dl_plan::{CpuOracle, PlannedGraph, plan_graph};
use dl_tensor::Tensor;
use hardware_capability::MemoryBudget;

/// 验收发现。
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// 参考（eager）路径本身失败——图或绑定有问题，后续对照无意义。
    ReferenceFailed {
        /// 错误说明。
        detail: String,
    },
    /// 某条路径规划或执行失败。
    PathFailed {
        /// 路径名。
        path: String,
        /// 错误说明。
        detail: String,
    },
    /// 某条路径的输出形状与参考不一致。
    ShapeMismatch {
        /// 路径名。
        path: String,
        /// 输出下标。
        output: usize,
        /// 参考形状。
        expected: Vec<usize>,
        /// 实际形状。
        actual: Vec<usize>,
    },
    /// 某条路径的输出数值超出容差。
    NumericMismatch {
        /// 路径名。
        path: String,
        /// 输出下标。
        output: usize,
        /// 最大绝对偏差。
        max_delta: f32,
    },
    /// 同一路径重复执行的结果不一致。
    NonDeterministic {
        /// 最大绝对偏差。
        max_delta: f32,
    },
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::ReferenceFailed { detail } => write!(f, "REFERENCE_FAILED: {detail}"),
            Finding::PathFailed { path, detail } => write!(f, "PATH_FAILED[{path}]: {detail}"),
            Finding::ShapeMismatch { path, output, expected, actual } => {
                write!(f, "SHAPE_MISMATCH[{path}] output {output}: expected {expected:?}, got {actual:?}")
            }
            Finding::NumericMismatch { path, output, max_delta } => write!(f, "NUMERIC_MISMATCH[{path}] output {output}: max delta {max_delta}"),
            Finding::NonDeterministic { max_delta } => write!(f, "NON_DETERMINISTIC: max delta {max_delta}"),
        }
    }
}

/// 验收报告。
#[derive(Debug, Clone, PartialEq)]
pub struct DlReport {
    /// 计划执行相对参考的最大绝对偏差。
    pub max_planned_delta: f32,
    /// 计划重复执行之间的最大绝对偏差。
    pub max_repeat_delta: f32,
    /// 调优执行相对参考的最大绝对偏差。
    pub max_tuned_delta: f32,
    /// 调优选中的候选标签；调优失败时为 `None`。
    pub tuned_label: Option<String>,
    /// 全部发现；为空即通过。
    pub findings: Vec<Finding>,
}

impl DlReport {
    /// 是否没有任何发现。
    pub fn passed(&self) -> bool {
        self.findings.is_empty()
    }
}

/// 输出两组张量的最大绝对偏差；形状不一致返回 `None`。
pub fn max_abs_delta(reference: &Tensor, candidate: &Tensor) -> Option<f32> {
    if reference.shape() != candidate.shape() {
        return None;
    }
    Some(
        reference
            .to_vec()
            .iter()
            .zip(candidate.to_vec())
            .fold(0.0f32, |max, (expected, actual)| max.max((expected - actual).abs())),
    )
}

/// 对照三条执行路径并生成报告。
///
/// 参考路径是 `Graph::run`；计划路径用给定 oracle 规划一次后执行两遍（第二遍用于确定性）；
/// 调优路径走 `dl-autotune` 的基线测量。任何失败都落成 finding。
pub fn check(graph: &Graph, bindings: &Bindings, budget: &MemoryBudget, tolerance: f32) -> DlReport {
    let mut findings = Vec::new();
    let reference = match graph.run(bindings) {
        Ok(outputs) => outputs,
        Err(error) => {
            findings.push(Finding::ReferenceFailed { detail: error.to_string() });
            return DlReport { max_planned_delta: 0.0, max_repeat_delta: 0.0, max_tuned_delta: 0.0, tuned_label: None, findings };
        }
    };

    let mut max_planned_delta = 0.0f32;
    let mut max_repeat_delta = 0.0f32;
    let planned = match plan_graph(graph, &[], &CpuOracle::new(), budget) {
        Err(error) => {
            findings.push(Finding::PathFailed { path: "planned".to_owned(), detail: error.to_string() });
            None
        }
        Ok(plan) => {
            let runner = PlannedGraph::from_plan(graph.clone(), plan);
            match runner.run(bindings).and_then(|first| Ok((first, runner.run(bindings)?))) {
                Ok(pair) => Some(pair),
                Err(error) => {
                    findings.push(Finding::PathFailed { path: "planned".to_owned(), detail: error.to_string() });
                    None
                }
            }
        }
    };
    if let Some((first, second)) = &planned {
        compare("planned", &reference, &first.outputs, tolerance, &mut findings, &mut max_planned_delta);
        let mut repeat_delta = 0.0f32;
        compare("repeat", &first.outputs, &second.outputs, tolerance, &mut findings, &mut repeat_delta);
        max_repeat_delta = repeat_delta;
        if repeat_delta > tolerance {
            findings.push(Finding::NonDeterministic { max_delta: repeat_delta });
        }
    }

    let mut max_tuned_delta = 0.0f32;
    let mut tuned_label = None;
    match dl_autotune::tune(graph, &[], &CpuOracle::new(), budget, bindings, &dl_autotune::AutotuneOptions::default()) {
        Ok(tuned) => {
            tuned_label = Some(tuned.label);
            match PlannedGraph::from_plan(graph.clone(), tuned.plan).run(bindings) {
                Ok(executed) => compare("tuned", &reference, &executed.outputs, tolerance, &mut findings, &mut max_tuned_delta),
                Err(error) => findings.push(Finding::PathFailed { path: "tuned".to_owned(), detail: error.to_string() }),
            }
        }
        Err(error) => findings.push(Finding::PathFailed { path: "tuned".to_owned(), detail: error.to_string() }),
    }

    DlReport { max_planned_delta, max_repeat_delta, max_tuned_delta, tuned_label, findings }
}

fn compare(path: &str, reference: &[Tensor], candidate: &[Tensor], tolerance: f32, findings: &mut Vec<Finding>, max_delta: &mut f32) {
    for (index, (expected, actual)) in reference.iter().zip(candidate).enumerate() {
        match max_abs_delta(expected, actual) {
            None => findings.push(Finding::ShapeMismatch {
                path: path.to_owned(),
                output: index,
                expected: expected.shape().to_vec(),
                actual: actual.shape().to_vec(),
            }),
            Some(delta) => {
                *max_delta = max_delta.max(delta);
                if delta > tolerance {
                    findings.push(Finding::NumericMismatch { path: path.to_owned(), output: index, max_delta: delta });
                }
            }
        }
    }
    if candidate.len() != reference.len() {
        findings.push(Finding::PathFailed {
            path: path.to_owned(),
            detail: format!("produced {} outputs, expected {}", candidate.len(), reference.len()),
        });
    }
}

/// 渲染 Markdown 报告。
pub fn render_markdown(report: &DlReport) -> String {
    let mut text = String::from("# DL 执行路径验收报告\n\n");
    text.push_str(&format!("- 计划执行最大偏差: {:e}\n", report.max_planned_delta));
    text.push_str(&format!("- 重复执行最大偏差: {:e}\n", report.max_repeat_delta));
    text.push_str(&format!("- 调优执行最大偏差: {:e}\n", report.max_tuned_delta));
    text.push_str(&format!("- 调优选中: {}\n", report.tuned_label.as_deref().unwrap_or("(失败)")));
    text.push_str(&format!("- 结论: {}\n\n", if report.passed() { "通过" } else { "不通过" }));
    text.push_str("## Findings\n\n");
    if report.findings.is_empty() {
        text.push_str("- NONE\n");
    } else {
        for finding in &report.findings {
            text.push_str(&format!("- {finding}\n"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_graph::GraphBuilder;

    fn graph_and_bindings() -> (Graph, Bindings) {
        let mut builder = GraphBuilder::new();
        let x = builder.input("x", [2, 2]).expect("input");
        let w = builder.parameter("w", [2, 2]).expect("parameter");
        let product = builder.matmul(x, w).expect("matmul");
        let activated = builder.relu(product).expect("relu");
        let graph = builder.finish(vec![activated]).expect("graph");
        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![1.0, -2.0, 0.5, 3.0], [2, 2]).expect("x"));
        bindings.insert_parameter("w", Tensor::from_vec(vec![0.2, -0.1, 0.4, 0.3], [2, 2]).expect("w"));
        (graph, bindings)
    }

    #[test]
    fn all_three_paths_agree_on_the_cpu_oracle() {
        let (graph, bindings) = graph_and_bindings();
        let report = check(&graph, &bindings, &MemoryBudget::unlimited(), 0.0);
        assert!(report.passed(), "findings: {:?}", report.findings);
        assert_eq!(report.max_planned_delta, 0.0);
        assert_eq!(report.max_repeat_delta, 0.0);
        assert_eq!(report.max_tuned_delta, 0.0);
        assert_eq!(report.tuned_label.as_deref(), Some("baseline"));
        let markdown = render_markdown(&report);
        assert!(markdown.contains("结论: 通过"));
        assert!(markdown.contains("- NONE"));
    }

    #[test]
    fn max_abs_delta_detects_shape_and_value_divergence() {
        let a = Tensor::from_vec(vec![1.0, 2.0], [1, 2]).expect("a");
        let b = Tensor::from_vec(vec![1.0, 2.5], [1, 2]).expect("b");
        let c = Tensor::from_vec(vec![1.0, 2.0, 3.0], [1, 3]).expect("c");
        assert_eq!(max_abs_delta(&a, &b), Some(0.5));
        assert_eq!(max_abs_delta(&a, &a), Some(0.0));
        assert_eq!(max_abs_delta(&a, &c), None);
    }

    #[test]
    fn failed_reference_is_reported_and_stops_comparison() {
        let (graph, _) = graph_and_bindings();
        let empty = Bindings::new();
        let report = check(&graph, &empty, &MemoryBudget::unlimited(), 0.0);
        assert!(!report.passed());
        assert!(matches!(&report.findings[..], [Finding::ReferenceFailed { .. }]));
    }
}
