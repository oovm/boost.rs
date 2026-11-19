//! 把 [`hardware_scheduler::Scheduler`] 接成 [`CapabilityOracle`]。
//!
//! 规划器因此可以直接查询真实注册后端的健康、提交状态、签名支持与预算，不需要在
//! `dl.*` 里复制一套放置判断。

use hardware_capability::ExecSignature;
use hardware_scheduler::Scheduler;

use crate::{Attempt, CapabilityOracle};

/// 以已注册后端为数据源的规划查询器。
pub struct SchedulerOracle<'a> {
    scheduler: &'a Scheduler,
}

impl<'a> SchedulerOracle<'a> {
    /// 包装一个调度器。
    pub fn new(scheduler: &'a Scheduler) -> Self {
        Self { scheduler }
    }
}

impl CapabilityOracle for SchedulerOracle<'_> {
    fn candidates(&self, signature: &ExecSignature) -> Vec<Attempt> {
        self.scheduler
            .candidates(signature)
            .into_iter()
            .map(|candidate| Attempt::new(candidate.backend, candidate.device, candidate.decision))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_graph::capture;
    use hardware_backend_npu::{NpuBackend, NpuConfig};
    use hardware_capability::MemoryBudget;

    use crate::plan_graph;

    fn graph() -> dl_graph::Graph {
        capture(|trace| {
            let x = trace.input("x", [2, 3])?;
            let w = trace.parameter("w", [3, 4])?;
            let product = trace.matmul(x, w)?;
            let activated = trace.relu(product)?;
            Ok(vec![activated])
        })
        .expect("graph")
    }

    #[test]
    fn plans_onto_an_attached_npu_through_the_scheduler() {
        let mut scheduler = Scheduler::new();
        scheduler.register(Box::new(NpuBackend::new(NpuConfig::attached("onnxruntime-xdna"))));
        let oracle = SchedulerOracle::new(&scheduler);
        let plan = plan_graph(&graph(), &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect("plan");
        assert_eq!(plan.assignments.len(), 2);
        assert!(plan.assignments.iter().all(|assignment| assignment.backend.as_str() == "npu"));
        assert!(plan.assignments.iter().all(|assignment| assignment.device.as_str() == "npu0"));
        assert!(plan.total_estimated_cost > 0);
    }

    #[test]
    fn discovered_npu_is_refused_with_its_state_reason() {
        let mut scheduler = Scheduler::new();
        scheduler.register(Box::new(NpuBackend::discovered()));
        let oracle = SchedulerOracle::new(&scheduler);
        let error = plan_graph(&graph(), &[], &oracle, &MemoryBudget::new(1 << 20, 0, 0)).expect_err("discovered");
        assert!(error.to_string().contains("npu"), "{error}");
        assert!(error.to_string().contains("Discovered"), "{error}");
    }
}
