//! 测量驱动的放置策略选择。
//!
//! 本 crate 只产出 `dl-plan` 的 [`GraphPlan`]，没有第二个执行内核：对每组候选放置
//! （基线逐节点放置，或带已声明子图候选的放置）用 [`PlannedGraph`] 实际运行若干次，
//! 取最短墙钟时间，在噪声范围内再按估计代价定胜负。选中结果仍由调用方交给
//! `dl_plan::PlannedGraph::from_plan` 执行——调优换的是计划，不是执行路径。
//!
//! 当前所有后端仍共享 CPU 内核，测量差异在噪声内、选择会落到估计代价；接入真实
//! 设备后端后，由测量决定。在此之前不得对外宣称已具备调优收益。

#![warn(missing_docs)]

use std::time::Instant;

use dl_graph::{Bindings, Graph};
use dl_plan::{CapabilityOracle, GraphPlan, PlannedGraph, SubgraphCandidate, plan_graph};
use hardware_capability::MemoryBudget;

/// 测量选项。
#[derive(Clone, Copy, Debug)]
pub struct AutotuneOptions {
    /// 每个候选的预热执行次数（不计时）。
    pub warmup: usize,
    /// 每个候选的计时执行次数；取最短一次。
    pub repetitions: usize,
    /// 相对噪声幅度（百分比）。与最快测量值差距在此范围内的候选视为平手，
    /// 按估计代价定胜负；设为 100 表示完全信任估计代价。
    pub noise_percent: u64,
}

impl Default for AutotuneOptions {
    fn default() -> Self {
        Self { warmup: 1, repetitions: 3, noise_percent: 15 }
    }
}

/// 一个被测量过的候选放置。
#[derive(Debug, Clone)]
pub struct PlanCandidate {
    /// 候选标签；`baseline` 表示纯逐节点放置，其余为子图候选名以 `+` 连接。
    pub label: String,
    /// 计划的估计总代价；规划失败时为 0。
    pub estimated_cost: u64,
    /// 计时执行的最短纳秒数；规划或执行失败时为 `None`。
    pub measured_nanos: Option<u64>,
    /// 失败原因；成功时为 `None`。
    pub failure: Option<String>,
    /// 规划出的放置计划；规划失败时为 `None`。
    pub plan: Option<GraphPlan>,
}

impl PlanCandidate {
    fn failed(label: String, detail: String) -> Self {
        Self { label, estimated_cost: 0, measured_nanos: None, failure: Some(detail), plan: None }
    }
}

/// 调优选中的放置计划。
#[derive(Debug, Clone)]
pub struct TunedPlan {
    /// 选中的计划；交给 `PlannedGraph::from_plan` 执行。
    pub plan: GraphPlan,
    /// 选中标签。
    pub label: String,
    /// 选中候选的最短计时执行。
    pub measured_nanos: u64,
    /// 全部候选，按尝试顺序（含失败者，便于复盘）。
    pub candidates: Vec<PlanCandidate>,
}

/// 调优错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutotuneError {
    /// 没有任何候选可用；每个失败候选的原因都保留。
    NoViablePlan {
        /// `(标签, 失败原因)`。
        failures: Vec<(String, String)>,
    },
}

impl std::fmt::Display for AutotuneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutotuneError::NoViablePlan { failures } => {
                write!(f, "no viable placement after autotune: ")?;
                let parts: Vec<String> = failures.iter().map(|(label, reason)| format!("{label}: {reason}")).collect();
                write!(f, "{}", parts.join("; "))
            }
        }
    }
}

impl std::error::Error for AutotuneError {}

/// 对候选放置组做测量选择。
///
/// 基线（纯逐节点放置）总是第一个被尝试；`candidate_sets` 里每组子图候选单独与基线比较。
/// 选择规则：最短测量时间优先；与最快值差距在 `noise_percent` 内视为平手，按估计代价
/// 定胜负。返回的 [`TunedPlan`] 仍由 `dl-plan` 执行，本 crate 不引入第二执行路径。
pub fn tune(
    graph: &Graph,
    candidate_sets: &[Vec<SubgraphCandidate>],
    oracle: &dyn CapabilityOracle,
    budget: &MemoryBudget,
    bindings: &Bindings,
    options: &AutotuneOptions,
) -> Result<TunedPlan, AutotuneError> {
    let mut candidates = vec![measure(graph, "baseline".to_owned(), Vec::new(), oracle, budget, bindings, options)];
    for set in candidate_sets {
        let label = if set.is_empty() { "baseline".to_owned() } else { set.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("+") };
        candidates.push(measure(graph, label, set.clone(), oracle, budget, bindings, options));
    }

    let mut best: Option<usize> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.plan.is_none() || candidate.measured_nanos.is_none() {
            continue;
        }
        match best {
            None => best = Some(index),
            Some(current) if beats(candidate, &candidates[current], options) => best = Some(index),
            Some(_) => {}
        }
    }

    let Some(index) = best else {
        let failures = candidates
            .iter()
            .map(|candidate| (candidate.label.clone(), candidate.failure.clone().unwrap_or_else(|| "not measured".to_owned())))
            .collect();
        return Err(AutotuneError::NoViablePlan { failures });
    };
    let winner = candidates[index].clone();
    let mut candidates = candidates;
    candidates.remove(index);
    let label = winner.label.clone();
    let measured_nanos = winner.measured_nanos.expect("best candidate is measured");
    let plan = winner.plan.clone().expect("best candidate has a plan");
    Ok(TunedPlan { plan, label, measured_nanos, candidates: std::iter::once(winner).chain(candidates).collect() })
}

/// 挑战者是否胜过当前最优：明显更快直接胜；噪声范围内按估计代价。
fn beats(challenger: &PlanCandidate, best: &PlanCandidate, options: &AutotuneOptions) -> bool {
    let (Some(challenge), Some(current)) = (challenger.measured_nanos, best.measured_nanos) else { return false };
    if challenge == current {
        return challenger.estimated_cost < best.estimated_cost;
    }
    let noise = options.noise_percent.min(100);
    let clearly_faster = challenge.saturating_mul(100) < current.saturating_mul(100 - noise);
    if clearly_faster {
        return true;
    }
    let clearly_slower = challenge.saturating_mul(100) > current.saturating_mul(100 + noise);
    if clearly_slower {
        return false;
    }
    challenger.estimated_cost < best.estimated_cost
}

/// 规划并计时一个候选；任何一步失败都记录为失败候选而不是中断整体调优。
fn measure(
    graph: &Graph,
    label: String,
    subgraphs: Vec<SubgraphCandidate>,
    oracle: &dyn CapabilityOracle,
    budget: &MemoryBudget,
    bindings: &Bindings,
    options: &AutotuneOptions,
) -> PlanCandidate {
    let plan = match plan_graph(graph, &subgraphs, oracle, budget) {
        Ok(plan) => plan,
        Err(error) => return PlanCandidate::failed(label, error.to_string()),
    };
    let estimated_cost = plan.total_estimated_cost;
    let runner = PlannedGraph::from_plan(graph.clone(), plan.clone());
    for _ in 0..options.warmup {
        if let Err(error) = runner.run(bindings) {
            return PlanCandidate::failed(label, error.to_string());
        }
    }
    let mut fastest = u64::MAX;
    for _ in 0..options.repetitions.max(1) {
        let start = Instant::now();
        if let Err(error) = runner.run(bindings) {
            return PlanCandidate::failed(label, error.to_string());
        }
        fastest = fastest.min(start.elapsed().as_nanos() as u64);
    }
    PlanCandidate { label, estimated_cost, measured_nanos: Some(fastest), failure: None, plan: Some(plan) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_graph::GraphBuilder;
    use dl_plan::Attempt;
    use dl_tensor::Tensor;
    use hardware_capability::{ExecPlan, ExecRefusal, ExecSignature};

    fn graph_and_bindings() -> (Graph, Bindings) {
        let mut builder = GraphBuilder::new();
        let x = builder.input("x", [2, 3]).expect("input");
        let w = builder.parameter("w", [3, 4]).expect("parameter");
        let product = builder.matmul(x, w).expect("matmul");
        let activated = builder.relu(product).expect("relu");
        let graph = builder.finish(vec![activated]).expect("graph");
        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], [2, 3]).expect("x"));
        bindings.insert_parameter("w", Tensor::from_vec(vec![0.1; 12], [3, 4]).expect("w"));
        (graph, bindings)
    }

    struct TableOracle {
        table: std::collections::BTreeMap<String, Vec<Attempt>>,
    }

    impl CapabilityOracle for TableOracle {
        fn candidates(&self, signature: &ExecSignature) -> Vec<Attempt> {
            self.table.get(&signature.op).cloned().unwrap_or_default()
        }
    }

    fn cpu_table() -> TableOracle {
        TableOracle {
            table: [("matmul".to_owned(), vec![cpu_attempt(100)]), ("relu".to_owned(), vec![cpu_attempt(100)])].into_iter().collect(),
        }
    }

    fn cpu_attempt(cost: u64) -> Attempt {
        Attempt::accepted("cpu", "cpu0", ExecPlan::new("cpu", "cpu0").with_estimated_cost(cost))
    }

    #[test]
    fn baseline_is_measured_and_selected_by_default() {
        let (graph, bindings) = graph_and_bindings();
        let tuned = tune(&graph, &[], &cpu_table(), &MemoryBudget::unlimited(), &bindings, &AutotuneOptions::default()).expect("tune");
        assert_eq!(tuned.label, "baseline");
        assert_eq!(tuned.candidates.len(), 1);
        assert_eq!(tuned.candidates[0].estimated_cost, 200);
        assert!(tuned.measured_nanos > 0);
        assert!(tuned.plan.assignments.len() == 2);
    }

    #[test]
    fn a_cheap_subgraph_wins_when_measurements_are_within_noise() {
        let (graph, bindings) = graph_and_bindings();
        let mut oracle = cpu_table();
        oracle.table.insert(
            "fused_mm_relu".to_owned(),
            vec![Attempt::accepted("npu", "npu0", ExecPlan::new("npu", "npu0").with_estimated_cost(30))],
        );
        let fused = vec![SubgraphCandidate {
            name: "fused_mm_relu".into(),
            nodes: vec![dl_graph::NodeId(2), dl_graph::NodeId(3)],
            signature: ExecSignature::subgraph("fused_mm_relu", vec!["f32".into()], vec![]),
        }];
        let options = AutotuneOptions { noise_percent: 100, ..AutotuneOptions::default() };
        let tuned = tune(&graph, &[fused], &oracle, &MemoryBudget::unlimited(), &bindings, &options).expect("tune");
        assert_eq!(tuned.label, "fused_mm_relu", "噪声范围内的平手必须按估计代价定胜负");
        assert_eq!(tuned.plan.subgraphs.len(), 1);
        assert_eq!(tuned.candidates.len(), 2);
    }

    #[test]
    fn broken_candidates_are_recorded_not_fatal() {
        let (graph, bindings) = graph_and_bindings();
        let broken = vec![SubgraphCandidate {
            name: "bad".into(),
            nodes: vec![dl_graph::NodeId(0)],
            signature: ExecSignature::subgraph("bad", vec!["f32".into()], vec![]),
        }];
        let tuned = tune(&graph, &[broken], &cpu_table(), &MemoryBudget::unlimited(), &bindings, &AutotuneOptions::default()).expect("tune");
        assert_eq!(tuned.label, "baseline");
        let failure = tuned.candidates.iter().find(|candidate| candidate.label == "bad").expect("recorded");
        assert!(failure.failure.as_deref().expect("reason").contains("unknown node"));
    }

    #[test]
    fn all_candidates_failing_reports_every_reason() {
        let (graph, bindings) = graph_and_bindings();
        let refusing = TableOracle {
            table: [
                ("matmul".to_owned(), vec![Attempt::refused("gpu", "gpu0", ExecRefusal::UnsupportedOp("matmul".into()))]),
                ("relu".to_owned(), vec![Attempt::refused("gpu", "gpu0", ExecRefusal::UnsupportedOp("relu".into()))]),
            ]
            .into_iter()
            .collect(),
        };
        let error = tune(&graph, &[], &refusing, &MemoryBudget::unlimited(), &bindings, &AutotuneOptions::default()).expect_err("no plan");
        let AutotuneError::NoViablePlan { failures } = error else { panic!("unexpected {error}") };
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "baseline");
        assert!(failures[0].1.contains("matmul"));
    }
}
