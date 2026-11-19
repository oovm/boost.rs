//! 把 DL 前向图翻译成硬件放置计划。
//!
//! 本 crate 只做计划，不执行：`dl-graph` 提供完整算子语义，[`CapabilityOracle`] 由调用方
//! 接上具体后端（CPU / GPU / NPU）。规划顺序与用户合同一致：
//!
//! 1. 先尝试调用方声明的子图候选（如 NPU 已编译量化子图），只有比逐节点方案更便宜才采用；
//! 2. 其余节点逐算子查询所有后端，先过滤健康/预算不可行的方案，再取端到端代价最小者；
//! 3. 没有任何可行方案时返回 [`PlanError::NoPlacement`]，并带上每个后端的拒绝原因。

#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use dl_graph::{Graph, NodeId, Op};
use hardware_capability::{BackendId, Conversion, DeviceId, ExecDecision, ExecPlan, ExecRefusal, ExecSignature, MemoryBudget};

mod stream;

pub use scheduler_oracle::SchedulerOracle;
pub use stream::{StreamReport, StreamStep, stream_plan};

mod scheduler_oracle;

/// 一个后端对一个签名的应答。
#[derive(Debug, Clone)]
pub struct Attempt {
    /// 应答的后端。
    pub backend: BackendId,
    /// 后端对应的物理设备。
    pub device: DeviceId,
    /// 接受方案或拒绝原因。
    pub decision: ExecDecision,
}

impl Attempt {
    /// 构造应答。
    pub fn new(backend: impl Into<BackendId>, device: impl Into<DeviceId>, decision: ExecDecision) -> Self {
        Self { backend: backend.into(), device: device.into(), decision }
    }

    /// 接受方案。
    pub fn accepted(backend: impl Into<BackendId>, device: impl Into<DeviceId>, plan: ExecPlan) -> Self {
        Self::new(backend, device, ExecDecision::Accept(plan))
    }

    /// 拒绝方案。
    pub fn refused(backend: impl Into<BackendId>, device: impl Into<DeviceId>, refusal: ExecRefusal) -> Self {
        Self::new(backend, device, ExecDecision::Refuse(refusal))
    }
}

/// 能力查询入口；实现方把 `hardware.*` 后端接到规划器。
pub trait CapabilityOracle {
    /// 返回所有后端对该签名的应答。
    fn candidates(&self, signature: &ExecSignature) -> Vec<Attempt>;
}

/// 调用方声明的子图候选（例如整段可编译到 NPU 的连通子图）。
#[derive(Debug, Clone)]
pub struct SubgraphCandidate {
    /// 子图名，写入放置结果。
    pub name: String,
    /// 子图覆盖的节点。
    pub nodes: Vec<NodeId>,
    /// 整体查询用的签名（含量化/形状约束）。
    pub signature: ExecSignature,
}

/// 单个节点的放置结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAssignment {
    /// 被放置的节点。
    pub node: NodeId,
    /// 执行后端。
    pub backend: BackendId,
    /// 执行设备。
    pub device: DeviceId,
    /// 该节点的端到端代价；属于子图时为 0，代价记在 [`SubgraphPlacement`] 上。
    pub estimated_cost: u64,
    /// 该节点方案的内存足迹；属于子图时为 0。
    pub footprint_bytes: u64,
    /// 若由子图整体放置，记录子图名。
    pub subgraph: Option<String>,
}

/// 被整体放置的子图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubgraphPlacement {
    /// 子图名。
    pub name: String,
    /// 覆盖的节点。
    pub nodes: Vec<NodeId>,
    /// 执行后端。
    pub backend: BackendId,
    /// 执行设备。
    pub device: DeviceId,
    /// 子图整体代价。
    pub estimated_cost: u64,
    /// 子图整体足迹。
    pub footprint_bytes: u64,
}

/// 整张图的放置计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPlan {
    /// 逐节点放置结果，按节点下标升序。
    pub assignments: Vec<NodeAssignment>,
    /// 被整体放置的子图。
    pub subgraphs: Vec<SubgraphPlacement>,
    /// 方案要求的转换（去重后按序）。
    pub conversions: Vec<Conversion>,
    /// 全部代价之和。
    pub total_estimated_cost: u64,
    /// 全部搬运字节之和。
    pub total_transfer_bytes: u64,
    /// 全部方案足迹之和（保守上界，不按生命周期复用）。
    pub total_footprint_bytes: u64,
}

impl GraphPlan {
    /// 查询某个节点的后端。
    pub fn backend_for(&self, node: NodeId) -> Option<&BackendId> {
        self.assignments.iter().find(|assignment| assignment.node == node).map(|assignment| &assignment.backend)
    }

    /// 计划涉及的后端集合。
    pub fn backends(&self) -> BTreeSet<&BackendId> {
        self.assignments.iter().map(|assignment| &assignment.backend).collect()
    }
}

/// 规划错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// 图本身未通过校验。
    InvalidGraph(String),
    /// 没有可放置的算子（空图或只有输入/参数）。
    NoOps,
    /// 节点不存在，或子图候选引用了输入/参数节点。
    UnknownNode(NodeId),
    /// 所有后端都拒绝或超出预算。
    NoPlacement {
        /// 失败节点。
        node: NodeId,
        /// 算子名。
        op: String,
        /// 每个后端的拒绝/超预算原因。
        reasons: Vec<String>,
    },
    /// 子图候选相互重叠。
    OverlappingSubgraph {
        /// 后处理时发现的子图名。
        name: String,
        /// 已由其它子图占用的节点。
        node: NodeId,
    },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::InvalidGraph(detail) => write!(f, "invalid DL graph: {detail}"),
            PlanError::NoOps => write!(f, "graph has no placeable operators"),
            PlanError::UnknownNode(node) => write!(f, "unknown node {}", node.0),
            PlanError::NoPlacement { node, op, reasons } => {
                write!(f, "no feasible placement for `{op}` at node {}: {}", node.0, reasons.join("; "))
            }
            PlanError::OverlappingSubgraph { name, node } => {
                write!(f, "subgraph `{name}` overlaps already assigned node {}", node.0)
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// 为整张图生成放置计划。
///
/// `subgraphs` 为空时就是纯逐算子放置；`budget` 是当前设备的实时可用预算。
pub fn plan_graph(
    graph: &Graph,
    subgraphs: &[SubgraphCandidate],
    oracle: &dyn CapabilityOracle,
    budget: &MemoryBudget,
) -> Result<GraphPlan, PlanError> {
    graph.validate().map_err(|error| PlanError::InvalidGraph(error.to_string()))?;

    let mut assigned: BTreeMap<usize, NodeAssignment> = BTreeMap::new();
    let mut claimed: BTreeSet<usize> = BTreeSet::new();
    let mut placements: Vec<SubgraphPlacement> = Vec::new();
    let mut conversions: BTreeSet<Conversion> = BTreeSet::new();
    let mut total_cost = 0u64;
    let mut total_transfer = 0u64;
    let mut total_footprint = 0u64;

    for candidate in subgraphs {
        for node in &candidate.nodes {
            let placeable =
                node.0 < graph.node_count() && !matches!(graph.op(*node), None | Some(Op::Input { .. }) | Some(Op::Parameter { .. }));
            if !placeable {
                return Err(PlanError::UnknownNode(*node));
            }
            if claimed.contains(&node.0) {
                return Err(PlanError::OverlappingSubgraph { name: candidate.name.clone(), node: *node });
            }
        }
        for node in &candidate.nodes {
            claimed.insert(node.0);
        }
        if candidate.nodes.is_empty() {
            continue;
        }

        let outcome = best_attempt(&candidate.signature, oracle, budget);
        let candidate_plan = match outcome.plan {
            Some(plan) => plan,
            None => continue,
        };
        let baseline = candidate.nodes.iter().map(|node| best_node_cost(graph, *node, oracle, budget)).sum::<Option<u64>>();
        let cheaper = baseline.map_or(true, |baseline| candidate_plan.estimated_cost < baseline);
        if !cheaper {
            continue;
        }

        for node in &candidate.nodes {
            assigned.insert(
                node.0,
                NodeAssignment {
                    node: *node,
                    backend: candidate_plan.backend.clone(),
                    device: candidate_plan.device.clone(),
                    estimated_cost: 0,
                    footprint_bytes: 0,
                    subgraph: Some(candidate.name.clone()),
                },
            );
        }
        conversions.extend(candidate_plan.required_conversions.iter().cloned());
        let footprint = candidate_plan.footprint_bytes();
        total_cost = total_cost.saturating_add(candidate_plan.estimated_cost);
        total_transfer = total_transfer.saturating_add(candidate_plan.transfer_bytes);
        total_footprint = total_footprint.saturating_add(footprint);
        placements.push(SubgraphPlacement {
            name: candidate.name.clone(),
            nodes: candidate.nodes.clone(),
            backend: candidate_plan.backend,
            device: candidate_plan.device,
            estimated_cost: candidate_plan.estimated_cost,
            footprint_bytes: footprint,
        });
    }

    for index in 0..graph.node_count() {
        let node = NodeId(index);
        if assigned.contains_key(&index) {
            continue;
        }
        let signature = match signature_for(graph, node) {
            Some(signature) => signature,
            None => continue,
        };
        let outcome = best_attempt(&signature, oracle, budget);
        let plan = outcome.plan.ok_or_else(|| PlanError::NoPlacement { node, op: signature.op.clone(), reasons: outcome.reasons })?;
        conversions.extend(plan.required_conversions.iter().cloned());
        total_cost = total_cost.saturating_add(plan.estimated_cost);
        total_transfer = total_transfer.saturating_add(plan.transfer_bytes);
        total_footprint = total_footprint.saturating_add(plan.footprint_bytes());
        assigned.insert(
            index,
            NodeAssignment {
                node,
                backend: plan.backend.clone(),
                device: plan.device.clone(),
                estimated_cost: plan.estimated_cost,
                footprint_bytes: plan.footprint_bytes(),
                subgraph: None,
            },
        );
    }

    if assigned.is_empty() {
        return Err(PlanError::NoOps);
    }

    Ok(GraphPlan {
        assignments: assigned.into_values().collect(),
        subgraphs: placements,
        conversions: conversions.into_iter().collect(),
        total_estimated_cost: total_cost,
        total_transfer_bytes: total_transfer,
        total_footprint_bytes: total_footprint,
    })
}

struct Outcome {
    plan: Option<ExecPlan>,
    reasons: Vec<String>,
}

/// 选择预算允许的最低代价方案；不可行的原因全部保留。
fn best_attempt(signature: &ExecSignature, oracle: &dyn CapabilityOracle, budget: &MemoryBudget) -> Outcome {
    let mut best: Option<ExecPlan> = None;
    let mut reasons = Vec::new();
    for attempt in oracle.candidates(signature) {
        match attempt.decision {
            ExecDecision::Accept(plan) => {
                if !plan.fits_budget(budget) {
                    reasons.push(format!(
                        "{}: budget exceeded (need {} bytes, have {})",
                        attempt.backend,
                        plan.footprint_bytes(),
                        budget.available_bytes()
                    ));
                    continue;
                }
                let better = best.as_ref().map_or(true, |current| plan.estimated_cost < current.estimated_cost);
                if better {
                    best = Some(plan);
                }
            }
            ExecDecision::Refuse(refusal) => reasons.push(format!("{}: {refusal}", attempt.backend)),
        }
    }
    Outcome { plan: best, reasons }
}

/// 某节点的逐算子最低代价；不可行时返回 `None`。
fn best_node_cost(graph: &Graph, node: NodeId, oracle: &dyn CapabilityOracle, budget: &MemoryBudget) -> Option<u64> {
    let signature = signature_for(graph, node)?;
    best_attempt(&signature, oracle, budget).plan.map(|plan| plan.estimated_cost)
}

/// 构造算子的可执行性签名；输入/参数节点返回 `None`。
fn signature_for(graph: &Graph, node: NodeId) -> Option<ExecSignature> {
    let op = match graph.op(node)? {
        Op::Add => "add",
        Op::Sub => "sub",
        Op::Mul => "mul",
        Op::MatMul => "matmul",
        Op::Relu => "relu",
        Op::Reshape { .. } => "reshape",
        Op::Transpose { .. } => "transpose",
        Op::SumAll => "sum_all",
        Op::MeanAll => "mean_all",
        Op::Input { .. } | Op::Parameter { .. } => return None,
    };
    let shapes = graph.node_inputs(node)?.iter().map(|input| graph.shape(*input).map(<[usize]>::to_vec)).collect();
    Some(ExecSignature::operator(op, vec!["f32".into()], shapes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_graph::capture;
    use hardware_capability::ExecKind;

    fn graph() -> Graph {
        capture(|trace| {
            let x = trace.input("x", [2, 3])?;
            let w = trace.parameter("w", [3, 4])?;
            let product = trace.matmul(x, w)?;
            let activated = trace.relu(product)?;
            Ok(vec![activated])
        })
        .expect("graph")
    }

    fn plan_for(cost: u64, temp: u64) -> ExecPlan {
        ExecPlan::new("gpu", "gpu0")
            .with_estimated_cost(cost)
            .with_temp_bytes(temp)
            .with_transfer_bytes(32)
            .with_conversion(Conversion::Layout("nchw -> nhwc".into()))
    }

    struct FakeOracle {
        table: BTreeMap<String, Vec<Attempt>>,
    }

    impl FakeOracle {
        fn new() -> Self {
            Self { table: BTreeMap::new() }
        }

        fn on(mut self, op: &str, attempts: Vec<Attempt>) -> Self {
            self.table.insert(op.to_string(), attempts);
            self
        }
    }

    impl CapabilityOracle for FakeOracle {
        fn candidates(&self, signature: &ExecSignature) -> Vec<Attempt> {
            self.table.get(&signature.op).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn picks_lowest_cost_backend_per_operator() {
        let oracle = FakeOracle::new()
            .on("matmul", vec![Attempt::accepted("gpu", "gpu0", plan_for(10, 64))])
            .on("relu", vec![Attempt::accepted("gpu", "gpu0", plan_for(10, 64))]);
        let plan = plan_graph(&graph(), &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");
        assert_eq!(plan.assignments.len(), 2);
        assert!(plan.assignments.iter().all(|assignment| assignment.backend.as_str() == "gpu"));
        assert_eq!(plan.total_estimated_cost, 20);
        assert_eq!(plan.total_transfer_bytes, 64);
        assert_eq!(plan.conversions, vec![Conversion::Layout("nchw -> nhwc".into())]);
        assert_eq!(plan.backend_for(NodeId(2)).map(BackendId::as_str), Some("gpu"));
    }

    #[test]
    fn falls_back_to_the_accepting_backend_with_reasons_kept() {
        let oracle = FakeOracle::new().on("matmul", vec![Attempt::accepted("gpu", "gpu0", plan_for(10, 64))]).on(
            "relu",
            vec![
                Attempt::refused("gpu", "gpu0", ExecRefusal::UnsupportedOp("relu".into())),
                Attempt::accepted("cpu", "cpu0", ExecPlan::new("cpu", "cpu0").with_estimated_cost(50).with_temp_bytes(32)),
            ],
        );
        let plan = plan_graph(&graph(), &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");
        assert_eq!(plan.backend_for(NodeId(2)).map(BackendId::as_str), Some("gpu"));
        assert_eq!(plan.backend_for(NodeId(3)).map(BackendId::as_str), Some("cpu"));
        assert_eq!(plan.total_estimated_cost, 60);
    }

    #[test]
    fn reports_no_placement_with_every_reason() {
        let oracle = FakeOracle::new()
            .on(
                "matmul",
                vec![
                    Attempt::refused("gpu", "gpu0", ExecRefusal::UnsupportedPrecision("f16".into())),
                    Attempt::refused("npu", "npu0", ExecRefusal::UnsupportedShape { detail: "dynamic".into() }),
                ],
            )
            .on("relu", vec![Attempt::accepted("gpu", "gpu0", plan_for(1, 1))]);
        let error = plan_graph(&graph(), &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect_err("no placement");
        match error {
            PlanError::NoPlacement { node, op, reasons } => {
                assert_eq!(node, NodeId(2));
                assert_eq!(op, "matmul");
                assert_eq!(reasons.len(), 2);
                assert!(reasons[0].contains("unsupported precision"));
            }
            other => panic!("unexpected {other}"),
        }
    }

    #[test]
    fn budget_removes_a_cheaper_but_too_large_plan() {
        let oracle = FakeOracle::new()
            .on("matmul", vec![Attempt::accepted("gpu", "gpu0", plan_for(1, 4024))])
            .on("relu", vec![Attempt::accepted("gpu", "gpu0", plan_for(1, 4024))]);
        // The oracle returns the same oversized plan, so both nodes must fail the budget.
        let error = plan_graph(&graph(), &[], &oracle, &MemoryBudget::new(512, 0, 0)).expect_err("over budget");
        assert!(matches!(error, PlanError::NoPlacement { node: NodeId(2), .. }));

        let oracle = FakeOracle::new()
            .on(
                "matmul",
                vec![
                    Attempt::accepted("gpu", "gpu0", plan_for(1, 4024)),
                    Attempt::accepted("cpu", "cpu0", ExecPlan::new("cpu", "cpu0").with_estimated_cost(40).with_temp_bytes(64)),
                ],
            )
            .on(
                "relu",
                vec![
                    Attempt::accepted("gpu", "gpu0", plan_for(1, 4024)),
                    Attempt::accepted("cpu", "cpu0", ExecPlan::new("cpu", "cpu0").with_estimated_cost(40).with_temp_bytes(64)),
                ],
            );
        let plan = plan_graph(&graph(), &[], &oracle, &MemoryBudget::new(512, 0, 0)).expect("plan");
        assert!(plan.assignments.iter().all(|assignment| assignment.backend.as_str() == "cpu"));
        assert_eq!(plan.total_estimated_cost, 80);
    }

    #[test]
    fn whole_subgraph_wins_when_it_beats_per_operator_placement() {
        let oracle = FakeOracle::new()
            .on("matmul", vec![Attempt::accepted("gpu", "gpu0", plan_for(100, 64))])
            .on("relu", vec![Attempt::accepted("gpu", "gpu0", plan_for(100, 64))])
            .on(
                "fused_mm_relu",
                vec![Attempt::accepted(
                    "npu",
                    "npu0",
                    ExecPlan::new("npu", "npu0").with_estimated_cost(30).with_temp_bytes(256).with_transfer_bytes(16),
                )],
            );
        let subgraph = SubgraphCandidate {
            name: "fused_mm_relu".into(),
            nodes: vec![NodeId(2), NodeId(3)],
            signature: ExecSignature::subgraph("fused_mm_relu", vec!["int8".into()], vec![Some(vec![2, 3])])
                .with_quantization("per-tensor-int8"),
        };
        let plan = plan_graph(&graph(), &[subgraph.clone()], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");
        assert_eq!(plan.subgraphs.len(), 1);
        assert_eq!(plan.subgraphs[0].backend.as_str(), "npu");
        assert!(plan.assignments.iter().all(|assignment| assignment.subgraph.as_deref() == Some("fused_mm_relu")));
        assert_eq!(plan.total_estimated_cost, 30);
        assert_eq!(plan.total_transfer_bytes, 16);
        assert_eq!(plan.total_footprint_bytes, 272);

        // A subgraph signature must stay a subgraph query, not a silent operator query.
        assert_eq!(subgraph.signature.kind, ExecKind::Subgraph);
    }

    #[test]
    fn rejects_overlapping_or_unknown_subgraph_nodes() {
        let oracle = FakeOracle::new();
        let budget = MemoryBudget::new(1 << 20, 0, 0);
        let base = SubgraphCandidate {
            name: "a".into(),
            nodes: vec![NodeId(2), NodeId(3)],
            signature: ExecSignature::subgraph("a", vec!["f32".into()], vec![]),
        };
        let overlap = SubgraphCandidate { name: "b".into(), nodes: vec![NodeId(3)], ..base.clone() };
        let error = plan_graph(&graph(), &[base.clone(), overlap], &oracle, &budget).expect_err("overlap");
        assert!(matches!(error, PlanError::OverlappingSubgraph { .. }));

        let unknown = SubgraphCandidate { name: "c".into(), nodes: vec![NodeId(0)], ..base };
        let error = plan_graph(&graph(), &[unknown], &oracle, &budget).expect_err("input node");
        assert!(matches!(error, PlanError::UnknownNode(NodeId(0))));
    }
}
