//! 按放置计划执行 DL 图，并在每个算子前流式加载它的段。
//!
//! 执行顺序与 `dl-graph` 的节点顺序一致，每个算子执行前把它在计划中的足迹提交给
//! [`SegmentStreamer`]，因此大于设备预算的模型也能逐段算完。分步执行和 `graph.run` 共用
//! 同一内核，测试要求两者数值完全一致。
//!
//! 调用方通常直接用 [`PlannedGraph`]：计划只算一次，之后每次执行都按计划走。

use std::collections::{BTreeMap, BTreeSet};

use dl_graph::{Bindings, Graph, NodeId, TrainingGraph};
use dl_tensor::Tensor;
use hardware_capability::MemoryBudget;
use hardware_runtime::{SegmentId, SegmentStreamer, StreamError, StreamStats};

use crate::{CpuOracle, GraphPlan, NodeAssignment, PlanError, StreamStep, plan_graph};

/// 流式执行错误。
#[derive(Debug)]
pub enum RunError {
    /// DL 内核错误。
    Eval(dl_types::DlError),
    /// 虚拟显存错误。
    Stream(StreamError),
    /// 计划缺少某个算子节点的放置。
    MissingPlacement(NodeId),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Eval(error) => write!(f, "{error}"),
            RunError::Stream(error) => write!(f, "{error}"),
            RunError::MissingPlacement(node) => write!(f, "placement plan has no assignment for node {}", node.0),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunError::Eval(error) => Some(error),
            RunError::Stream(error) => Some(error),
            RunError::MissingPlacement(_) => None,
        }
    }
}

impl From<dl_types::DlError> for RunError {
    fn from(error: dl_types::DlError) -> Self {
        RunError::Eval(error)
    }
}

impl From<StreamError> for RunError {
    fn from(error: StreamError) -> Self {
        RunError::Stream(error)
    }
}

/// 流式执行结果。
#[derive(Debug)]
pub struct ExecutedPlan {
    /// 图输出。
    pub outputs: Vec<Tensor>,
    /// 每个算子执行前的加载步骤。
    pub steps: Vec<StreamStep>,
    /// 流式层累计统计。
    pub stats: StreamStats,
}

impl ExecutedPlan {
    /// 训练图的损失输出。
    pub fn loss(&self, training: &TrainingGraph) -> Option<&Tensor> {
        let index = training.graph.outputs().iter().position(|output| *output == training.loss)?;
        self.outputs.get(index)
    }

    /// 按名字取参数或输入的梯度。
    pub fn gradient(&self, training: &TrainingGraph, name: &str) -> Option<&Tensor> {
        let node = training.gradients.parameter(name).or_else(|| training.gradients.input(name))?;
        let index = training.graph.outputs().iter().position(|output| *output == node)?;
        self.outputs.get(index)
    }
}

/// 按 [`GraphPlan`] 执行整张图。
///
/// 输入/参数节点不占段；子图只在其第一个节点前加载一次。
pub fn execute_plan(graph: &Graph, plan: &GraphPlan, bindings: &Bindings, streamer: &mut SegmentStreamer) -> Result<ExecutedPlan, RunError> {
    let mut interpreter = graph.interpreter(bindings)?;
    let assignments: BTreeMap<usize, &NodeAssignment> = plan.assignments.iter().map(|assignment| (assignment.node.0, assignment)).collect();
    let subgraph_bytes: BTreeMap<&str, u64> =
        plan.subgraphs.iter().map(|placement| (placement.name.as_str(), placement.footprint_bytes)).collect();
    let mut emitted: BTreeSet<&str> = BTreeSet::new();
    let mut next_segment = 1u64;
    let mut steps = Vec::new();

    for index in 0..graph.node_count() {
        let node = NodeId(index);
        let is_binding = graph.is_binding(node);
        if !is_binding {
            let assignment = assignments.get(&index).ok_or(RunError::MissingPlacement(node))?;
            let footprint = match assignment.subgraph.as_deref() {
                Some(name) => {
                    if emitted.insert(name) {
                        *subgraph_bytes.get(name).ok_or(RunError::MissingPlacement(node))?
                    }
                    else {
                        0
                    }
                }
                None => assignment.footprint_bytes,
            };
            if footprint > 0 {
                let id = SegmentId(next_segment);
                next_segment += 1;
                streamer.register(id, footprint)?;
                steps.push(StreamStep { node, plan: streamer.submit(id)? });
            }
        }
        interpreter.step()?;
    }

    Ok(ExecutedPlan { outputs: interpreter.outputs()?, steps, stats: streamer.stats() })
}

/// 计划固定下来的可执行图。
///
/// 计划在构造时算一次，之后每次 [`PlannedGraph::run`] 都按同一计划执行；每次执行新建流式器，
/// 段身份按本次执行分配，重复调用不会冲突。训练与推理都应走这里，而不是直接 `Graph::run`，
/// 否则放置与流式装载永远不会生效。
pub struct PlannedGraph {
    graph: Graph,
    plan: GraphPlan,
}

impl PlannedGraph {
    /// 用默认 CPU 放置（[`CpuOracle`]）为整张图建计划。
    pub fn new(graph: Graph) -> Result<Self, PlanError> {
        let plan = plan_graph(&graph, &[], &CpuOracle::new(), &MemoryBudget::unlimited())?;
        Ok(Self { graph, plan })
    }

    /// 复用调用方已经算好的计划。
    pub fn from_plan(graph: Graph, plan: GraphPlan) -> Self {
        Self { graph, plan }
    }

    /// 底层图。
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// 放置计划。
    pub fn plan(&self) -> &GraphPlan {
        &self.plan
    }

    /// 按绑定执行整张图，返回输出与本次流式统计。
    pub fn run(&self, bindings: &Bindings) -> Result<ExecutedPlan, RunError> {
        let device_capacity = self.plan.total_footprint_bytes.max(1);
        let mut streamer = SegmentStreamer::new(device_capacity, u64::MAX, u64::MAX);
        execute_plan(&self.graph, &self.plan, bindings, &mut streamer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_graph::capture;
    use hardware_capability::{ExecPlan, ExecSignature, MemoryBudget};

    use crate::{Attempt, CapabilityOracle, plan_graph};

    struct StageOracle {
        footprint: u64,
    }

    impl CapabilityOracle for StageOracle {
        fn candidates(&self, _signature: &ExecSignature) -> Vec<Attempt> {
            vec![Attempt::accepted("gpu", "gpu0", ExecPlan::new("gpu", "gpu0").with_temp_bytes(self.footprint).with_estimated_cost(1))]
        }
    }

    fn graph() -> Graph {
        capture(|trace| {
            let x = trace.input("x", [2, 2])?;
            let w = trace.parameter("w", [2, 2])?;
            let sum = trace.add(x, w)?;
            let activated = trace.relu(sum)?;
            let out = trace.mul(activated, w)?;
            Ok(vec![out])
        })
        .expect("graph")
    }

    fn bindings() -> Bindings {
        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![1.0, -2.0, 3.0, -4.0], [2, 2]).unwrap());
        bindings.insert_parameter("w", Tensor::from_vec(vec![0.5, 0.5, 1.0, -1.0], [2, 2]).unwrap());
        bindings
    }

    #[test]
    fn streamed_execution_matches_eager_execution() {
        let graph = graph();
        let bindings = bindings();
        let oracle = StageOracle { footprint: 48 };
        let plan = plan_graph(&graph, &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");

        let mut streamer = SegmentStreamer::new(64, 1 << 20, 1 << 20);
        let executed = execute_plan(&graph, &plan, &bindings, &mut streamer).expect("execute");
        let eager = graph.run(&bindings).expect("eager");

        assert_eq!(executed.steps.len(), 3);
        assert!(executed.stats.evictions > 0, "64-byte budget forces streaming");
        assert_eq!(executed.outputs[0].to_vec(), eager[0].to_vec());
        assert_eq!(streamer.device_used(), 48);
    }

    fn training_graph() -> dl_graph::TrainingGraph {
        let mut builder = dl_graph::GraphBuilder::new();
        let x = builder.input("x", [2, 2]).unwrap();
        let w = builder.parameter("w", [2, 2]).unwrap();
        let product = builder.matmul(x, w).unwrap();
        let activated = builder.relu(product).unwrap();
        let loss = builder.mean_all(activated).unwrap();
        let forward = builder.finish(vec![loss]).unwrap();
        forward.training(loss).unwrap()
    }

    #[test]
    fn streaming_execution_covers_the_backward_pass() {
        let training = training_graph();
        let graph = &training.graph;
        let bindings = bindings();
        let oracle = StageOracle { footprint: 48 };
        let plan = plan_graph(graph, &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");
        let placeable = (0..graph.node_count()).filter(|index| !graph.is_binding(NodeId(*index))).count();
        assert_eq!(plan.assignments.len(), placeable, "backward nodes must be placed too");

        let mut streamer = SegmentStreamer::new(64, 1 << 20, 1 << 20);
        let executed = execute_plan(graph, &plan, &bindings, &mut streamer).expect("execute");
        let eager = graph.run(&bindings).expect("eager");

        assert_eq!(executed.outputs.len(), eager.len());
        for (streamed, reference) in executed.outputs.iter().zip(&eager) {
            assert_eq!(streamed.to_vec(), reference.to_vec());
        }
        assert_eq!(executed.loss(&training).map(Tensor::to_vec), Some(eager[0].to_vec()));
        assert_eq!(executed.gradient(&training, "w").map(Tensor::to_vec), Some(eager[1].to_vec()));
        assert!(executed.stats.evictions > 0, "64-byte budget forces the training graph to stream");
    }

    #[test]
    fn disk_backed_streaming_runs_the_training_graph() {
        let root = std::env::temp_dir().join(format!("boost-dl-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let training = training_graph();
        let graph = &training.graph;
        let bindings = bindings();
        let oracle = StageOracle { footprint: 48 };
        let plan = plan_graph(graph, &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");

        let mut streamer = SegmentStreamer::with_disk_backing(&root, 64, 64, 1 << 20).expect("disk backing").with_host_backing();
        let executed = execute_plan(graph, &plan, &bindings, &mut streamer).expect("execute");
        let eager = graph.run(&bindings).expect("eager");

        assert!(executed.stats.disk_read_bytes > 0, "segments must come back through real files");
        assert_eq!(streamer.host_allocated(), streamer.host_used(), "host tier must hold real buffers");
        assert_eq!(executed.outputs.len(), eager.len());
        for (streamed, reference) in executed.outputs.iter().zip(&eager) {
            assert_eq!(streamed.to_vec(), reference.to_vec());
        }
        assert_eq!(executed.gradient(&training, "x").map(Tensor::to_vec), Some(eager[2].to_vec()));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn planned_graph_matches_direct_execution_through_the_default_cpu_plan() {
        let mut builder = dl_graph::GraphBuilder::new();
        let x = builder.input("x", [2, 2]).unwrap();
        let target = builder.input("y", [2, 3]).unwrap();
        let weight = builder.parameter("w", [2, 3]).unwrap();
        let logits = builder.matmul(x, weight).unwrap();
        let loss = builder.softmax_cross_entropy(logits, target).unwrap();
        let forward = builder.finish(vec![logits]).unwrap();
        let training = forward.training(loss).unwrap();
        let graph = &training.graph;

        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![1.0, -2.0, 0.5, 3.0], [2, 2]).unwrap());
        bindings.insert_input("y", Tensor::from_vec(vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0], [2, 3]).unwrap());
        bindings.insert_parameter("w", Tensor::from_vec(vec![0.2, -0.1, 0.4, 0.3, 0.5, -0.2], [2, 3]).unwrap());

        let planned = PlannedGraph::new(graph.clone()).expect("default cpu plan");
        let placeable = (0..graph.node_count()).filter(|index| !graph.is_binding(NodeId(*index))).count();
        assert_eq!(planned.plan().assignments.len(), placeable);

        let executed = planned.run(&bindings).expect("execute");
        let eager = graph.run(&bindings).expect("eager");
        for (streamed, reference) in executed.outputs.iter().zip(&eager) {
            assert_eq!(streamed.to_vec(), reference.to_vec(), "planned execution must match graph.run");
        }
        // 计划可重复执行：每次执行都新建流式器，段身份不会冲突。
        let again = planned.run(&bindings).expect("execute twice");
        assert_eq!(again.outputs[0].to_vec(), eager[0].to_vec());
    }

    #[test]
    fn missing_placement_is_reported_not_silently_skipped() {
        let graph = graph();
        let bindings = bindings();
        let oracle = StageOracle { footprint: 48 };
        let mut plan = plan_graph(&graph, &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");
        plan.assignments.retain(|assignment| assignment.node != NodeId(2));
        let mut streamer = SegmentStreamer::new(1 << 20, 1 << 20, 1 << 20);
        let error = execute_plan(&graph, &plan, &bindings, &mut streamer).expect_err("missing");
        assert!(matches!(error, RunError::MissingPlacement(NodeId(2))));
    }
}
