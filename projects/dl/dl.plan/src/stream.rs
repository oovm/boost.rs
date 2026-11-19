//! 把 [`GraphPlan`] 接到虚拟显存分段流式加载。
//!
//! 放置计划决定“在哪算”，本模块决定“怎么装”：按计划顺序把每个节点方案的足迹注册为
//! 一个段并提交给 [`SegmentStreamer`]。设备装不下时由流式层按 LRU 驱逐，因此大于设备
//! 预算的模型也能逐段执行，并返回每步的搬运/驱逐明细。

use std::collections::BTreeSet;

use dl_graph::NodeId;
use hardware_runtime::{LoadPlan, SegmentId, SegmentStreamer, StreamError, StreamStats};

use crate::GraphPlan;

/// 一步流式加载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStep {
    /// 该步对应的节点；子图整体放置时取子图覆盖的第一个节点。
    pub node: NodeId,
    /// 虚拟显存返回的加载计划。
    pub plan: LoadPlan,
}

/// 整个放置计划的流式执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReport {
    /// 按计划顺序的加载步骤。
    pub steps: Vec<StreamStep>,
    /// 流式层累计统计。
    pub stats: StreamStats,
}

impl StreamReport {
    /// 本计划触发的驱逐次数。
    pub fn evictions(&self) -> u64 {
        self.stats.evictions
    }
}

/// 按 [`GraphPlan`] 顺序把每个放置单元作为段提交给虚拟显存。
///
/// 子图只注册一次，并在其第一个节点位置提交；逐节点放置按节点下标顺序提交。
pub fn stream_plan(plan: &GraphPlan, streamer: &mut SegmentStreamer) -> Result<StreamReport, StreamError> {
    let mut next_id = 1u64;
    let mut emitted: BTreeSet<&str> = BTreeSet::new();
    let mut steps = Vec::new();
    for assignment in &plan.assignments {
        let (id, bytes) = match assignment.subgraph.as_deref() {
            Some(name) => {
                if !emitted.insert(name) {
                    continue;
                }
                let placement =
                    plan.subgraphs.iter().find(|placement| placement.name == name).expect("assignment subgraph must have a placement record");
                (SegmentId(next_id), placement.footprint_bytes)
            }
            None => (SegmentId(next_id), assignment.footprint_bytes),
        };
        next_id += 1;
        streamer.register(id, bytes)?;
        let load = streamer.submit(id)?;
        steps.push(StreamStep { node: assignment.node, plan: load });
    }
    Ok(StreamReport { steps, stats: streamer.stats() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_graph::capture;
    use hardware_capability::{ExecPlan, ExecSignature, MemoryBudget};

    use crate::{Attempt, CapabilityOracle, SubgraphCandidate, plan_graph};

    struct ChainOracle {
        footprint: u64,
    }

    impl CapabilityOracle for ChainOracle {
        fn candidates(&self, _signature: &ExecSignature) -> Vec<Attempt> {
            vec![Attempt::accepted("gpu", "gpu0", ExecPlan::new("gpu", "gpu0").with_temp_bytes(self.footprint).with_estimated_cost(1))]
        }
    }

    fn chain_graph(layers: usize) -> dl_graph::Graph {
        capture(|trace| {
            let mut current = trace.input("x", [2, 2])?;
            for _ in 0..layers {
                current = trace.relu(current)?;
            }
            Ok(vec![current])
        })
        .expect("graph")
    }

    #[test]
    fn streams_a_plan_larger_than_the_device_budget() {
        let graph = chain_graph(6);
        let oracle = ChainOracle { footprint: 4000 };
        let budget = MemoryBudget::new(5000, 0, 0);
        let plan = plan_graph(&graph, &[], &oracle, &budget).expect("plan");
        assert!(plan.total_footprint_bytes > budget.available_bytes());

        let mut streamer = SegmentStreamer::new(5000, 1 << 20, 1 << 20);
        let report = stream_plan(&plan, &mut streamer).expect("stream");
        assert_eq!(report.steps.len(), 6);
        assert_eq!(report.evictions(), 5, "only one segment fits at a time");
        assert_eq!(streamer.device_used(), 4000);
        assert!(streamer.host_used() > 0);
        assert_eq!(report.steps[0].plan.transfers.len(), 1, "cold segment comes straight from disk");
    }

    #[test]
    fn subgraph_is_streamed_once_at_its_first_node() {
        let graph = chain_graph(2);
        let oracle = ChainOracle { footprint: 4000 };
        let budget = MemoryBudget::new(1 << 20, 0, 0);
        let subgraph = SubgraphCandidate {
            name: "fused".into(),
            nodes: vec![NodeId(1), NodeId(2)],
            signature: ExecSignature::subgraph("fused", vec!["f32".into()], vec![]),
        };
        let plan = plan_graph(&graph, &[subgraph], &oracle, &budget).expect("plan");
        assert_eq!(plan.subgraphs.len(), 1);

        let mut streamer = SegmentStreamer::new(1 << 20, 1 << 20, 1 << 20);
        let report = stream_plan(&plan, &mut streamer).expect("stream");
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].node, NodeId(1));
        assert_eq!(streamer.device_used(), 4000);
    }
}
