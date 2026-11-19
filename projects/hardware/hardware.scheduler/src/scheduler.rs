use std::collections::BTreeMap;

use hardware_harness::{EventKind, Trace, Tracer};
use hardware_ir::{Dispatch, Graph, Objective, Priority, TaskId, TaskKind};

use crate::{
    backend::Backend,
    deadline::effective_deadlines,
    error::{Error, Result},
    exec::ExecutionBindings,
    probe::BackendProbeEvent,
};
use hardware_capability::{BackendId, CapabilityReport, DeviceId, DeviceState, ExecDecision, ExecRefusal, ExecSignature, MemoryBudget};

/// One cross-backend placement transition observed during dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSwitch {
    /// Backend flushed before the switch.
    pub from_backend: String,
    /// Backend receiving the next task.
    pub to_backend: String,
    /// Task that triggered the switch.
    pub at_task: TaskId,
}

/// One backend probe result for a signature, including refusal reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCandidate {
    /// Probed backend.
    pub backend: BackendId,
    /// Physical device behind the backend.
    pub device: DeviceId,
    /// Accepted plan or typed refusal.
    pub decision: ExecDecision,
}

/// Places and submits tasks onto registered backends.
pub struct Scheduler {
    backends: Vec<Box<dyn Backend>>,
    last_backend_switches: Vec<BackendSwitch>,
    last_backend_probes: Vec<BackendProbeEvent>,
    tracer: Option<Tracer>,
}

impl Scheduler {
    /// Empty scheduler with no backends.
    pub fn new() -> Self {
        Self { backends: Vec::new(), last_backend_switches: Vec::new(), last_backend_probes: Vec::new(), tracer: None }
    }

    /// Records placement and dispatch events into `tracer`.
    pub fn with_tracer(mut self, tracer: Tracer) -> Self {
        self.tracer = Some(tracer);
        self
    }

    /// Takes the finished trace when tracing was enabled.
    pub fn take_trace(&mut self) -> Option<Trace> {
        self.tracer.take().map(Tracer::finish)
    }

    /// Take backend-switch records from the most recent [`Self::dispatch`] / submit.
    pub fn take_backend_switches(&mut self) -> Vec<BackendSwitch> {
        std::mem::take(&mut self.last_backend_switches)
    }

    /// Take backend probe records from the most recent [`Self::dispatch`] / submit.
    pub fn take_backend_probes(&mut self) -> Vec<BackendProbeEvent> {
        std::mem::take(&mut self.last_backend_probes)
    }

    /// Register a backend for placement.
    pub fn register(&mut self, backend: Box<dyn Backend>) {
        self.backends.push(backend);
    }

    /// Push external buffers / preferred stream onto every registered backend.
    pub fn bind_externals(&mut self, bindings: &ExecutionBindings) -> Result<()> {
        for backend in &mut self.backends {
            backend.bind_externals(bindings)?;
        }
        Ok(())
    }

    /// Capability rows for every registered backend (application probe).
    pub fn capabilities(&self) -> CapabilityReport {
        CapabilityReport::from_backends(self.backends.iter().map(|b| (b.name().to_string(), b.features())))
    }

    /// Validate the graph and allocate/bind resources on every backend.
    ///
    /// Idempotent when resource sizes match an existing allocation. Prefer
    /// [`Self::prepare_selected`] / [`Self::execute`], which only prepare backends that
    /// actually receive work.
    pub fn prepare(&mut self, graph: &Graph) -> Result<()> {
        graph.validate()?;
        for backend in &mut self.backends {
            backend.prepare(graph)?;
        }
        Ok(())
    }

    /// Validate the graph and prepare only the backends selected by placement.
    ///
    /// Placement runs first: pinned backends win, then infeasible backends are filtered
    /// out by [`Backend::supports`], health, state and budget, then the lowest estimated
    /// cost wins. Exactly one prepare call is made per selected backend.
    pub fn prepare_selected(&mut self, graph: &Graph) -> Result<()> {
        graph.validate()?;
        let placement = self.plan_placement(graph)?;
        self.prepare_placed(graph, &placement)
    }

    fn prepare_placed(&mut self, graph: &Graph, placement: &Placement) -> Result<()> {
        let mut prepared = vec![false; self.backends.len()];
        for task_id in &placement.order {
            let index = placement.backends[task_id];
            if !prepared[index] {
                self.backends[index].prepare(graph)?;
                prepared[index] = true;
            }
        }
        Ok(())
    }

    /// Flush every registered backend (host-visible after prior submits).
    pub fn flush(&mut self) -> Result<()> {
        for backend in &mut self.backends {
            backend.flush()?;
        }
        Ok(())
    }

    /// Submit a previously [`prepare`]d graph (does not allocate again).
    pub fn submit_prepared(&mut self, graph: &Graph) -> Result<()> {
        graph.validate()?;
        self.dispatch(graph)
    }

    /// Validate, prepare, then submit tasks in dependency order.
    ///
    /// Ready tasks prefer [`Priority::Interactive`] and tighter effective deadlines
    /// (see [`crate::effective_deadlines`]) before batch work.
    ///
    /// Placement: optional string param `backend` pins a task to a registered backend
    /// name (exact or substring). Otherwise every backend is asked via
    /// [`Backend::supports`]; infeasible ones (refusal, unhealthy, not submittable, over
    /// budget) are filtered out and the lowest estimated cost wins. Only selected
    /// backends are prepared. Crossing backends calls [`Backend::flush`] on the previous
    /// backend before the next submit.
    pub fn execute(&mut self, graph: &Graph) -> Result<()> {
        graph.validate()?;
        self.last_backend_switches.clear();
        self.last_backend_probes.clear();
        let mut tracer = self.tracer.take();
        let result = match self.plan_placement(graph) {
            Ok(placement) => self.prepare_placed(graph, &placement).and_then(|()| self.dispatch_inner(graph, &placement, &mut tracer)),
            Err(error) => {
                record_placement_failure(&mut tracer, &error);
                Err(error)
            }
        };
        self.tracer = tracer;
        result
    }

    fn dispatch(&mut self, graph: &Graph) -> Result<()> {
        self.last_backend_switches.clear();
        self.last_backend_probes.clear();
        let mut tracer = self.tracer.take();
        let result = match self.plan_placement(graph) {
            Ok(placement) => self.dispatch_inner(graph, &placement, &mut tracer),
            Err(error) => {
                record_placement_failure(&mut tracer, &error);
                Err(error)
            }
        };
        self.tracer = tracer;
        result
    }

    fn dispatch_inner(&mut self, graph: &Graph, placement: &Placement, tracer: &mut Option<Tracer>) -> Result<()> {
        let mut last_backend: Option<usize> = None;
        for task_id in &placement.order {
            let task_id = *task_id;
            let task = graph.tasks.nodes.iter().find(|t| t.id == task_id).expect("task in order must exist");
            let label = task_label(task);
            let backend_idx = placement.backends[&task_id];
            let backend_name = self.backends[backend_idx].name().to_string();
            if let Some(tracer) = tracer.as_mut() {
                let pinned = matches!(task.params.get("backend"), Some(hardware_ir::ParamValue::Str(_)));
                tracer.record(EventKind::BackendSelected {
                    backend: backend_name.clone(),
                    reason: if pinned { "pinned" } else { "placement" }.into(),
                });
            }
            if let Some(prev) = last_backend {
                if prev != backend_idx {
                    self.backends[prev].flush()?;
                    self.last_backend_switches.push(BackendSwitch {
                        from_backend: self.backends[prev].name().to_string(),
                        to_backend: self.backends[backend_idx].name().to_string(),
                        at_task: task_id,
                    });
                }
            }
            let submitted = if let Some(tracer) = tracer.as_mut() {
                tracer.span(format!("dispatch {label} on {backend_name}"), |_| self.backends[backend_idx].submit_task(graph, task))
            }
            else {
                self.backends[backend_idx].submit_task(graph, task)
            };
            match submitted {
                Ok(()) => {
                    if let Some(tracer) = tracer.as_mut() {
                        tracer.record(EventKind::Dispatched { task: label, parallel: parallel_count(task) });
                    }
                }
                Err(error) => {
                    if let Some(tracer) = tracer.as_mut() {
                        tracer.record(EventKind::DispatchFailed { task: label, message: error.to_string() });
                    }
                    return Err(error);
                }
            }
            let probes = self.backends[backend_idx].take_probe_events();
            if let Some(tracer) = tracer.as_mut() {
                for probe in &probes {
                    tracer.record(EventKind::BackendProbe {
                        backend: probe.backend.clone(),
                        kind: probe.kind.to_string(),
                        shader_key: probe.shader_key,
                    });
                }
            }
            self.last_backend_probes.extend(probes);
            last_backend = Some(backend_idx);
        }
        Ok(())
    }

    /// Compute placement for every task: pins first, then feasibility filtering,
    /// then lowest estimated cost (ties keep registration order).
    fn plan_placement(&self, graph: &Graph) -> Result<Placement> {
        let deadlines = effective_deadlines(graph);
        let order = schedule_order(graph, &deadlines)?;
        let mut backends = BTreeMap::new();
        for task_id in &order {
            let task = graph.tasks.nodes.iter().find(|t| t.id == *task_id).expect("task in order must exist");
            backends.insert(*task_id, self.pick_backend_index(task)?);
        }
        Ok(Placement { order, backends })
    }

    fn pick_backend_index(&self, task: &hardware_ir::TaskNode) -> Result<usize> {
        let signature = signature_of(task);
        if let Some(hardware_ir::ParamValue::Str(name)) = task.params.get("backend") {
            let idx = self
                .backends
                .iter()
                .position(|b| b.name() == name || b.name().contains(name.as_str()))
                .ok_or_else(|| Error::NoBackend(format!("pinned backend `{name}` not registered")))?;
            return self.confirm_candidate(idx, task, &signature);
        }

        let mut refusals = Vec::new();
        let mut best: Option<(u64, usize)> = None;
        for index in 0..self.backends.len() {
            match self.feasibility(index, &signature) {
                ExecDecision::Accept(plan) => {
                    // 后端自己的端到端代价加上旧的种类偏好权重：默认后端代价为 0，
                    // 因此默认放置与旧的按种类/注册顺序完全一致。
                    let cost = plan.estimated_cost.saturating_add(legacy_weight(task, self.backends[index].name()));
                    let candidate = (cost, index);
                    if best.map_or(true, |current| candidate < current) {
                        best = Some(candidate);
                    }
                }
                ExecDecision::Refuse(reason) => refusals.push(format!("{}: {reason}", self.backends[index].name())),
            }
        }

        match best {
            Some((_, index)) => Ok(index),
            None if self.backends.is_empty() => Err(Error::NoBackend(format!("{task_kind:?}", task_kind = task.kind))),
            None => Err(Error::Infeasible { task: task_label(task), detail: refusals.join("; ") }),
        }
    }

    fn confirm_candidate(&self, index: usize, task: &hardware_ir::TaskNode, signature: &ExecSignature) -> Result<usize> {
        match self.feasibility(index, signature) {
            ExecDecision::Accept(_) => Ok(index),
            ExecDecision::Refuse(reason) => {
                Err(Error::Infeasible { task: task_label(task), detail: format!("{}: {reason}", self.backends[index].name()) })
            }
        }
    }

    /// 可行性硬条件：健康、可提交、签名被接受、实时预算装得下。
    fn feasibility(&self, index: usize, signature: &ExecSignature) -> ExecDecision {
        let backend = &self.backends[index];
        let health = backend.health();
        if !health.is_usable() {
            return ExecDecision::Refuse(ExecRefusal::Unhealthy(health.reason().unwrap_or("unhealthy").to_owned()));
        }
        let state = backend.state();
        if state != DeviceState::Submittable {
            return ExecDecision::Refuse(ExecRefusal::Unhealthy(format!("device state is {state:?}")));
        }
        match backend.supports(signature) {
            ExecDecision::Accept(plan) => {
                let budget: MemoryBudget = backend.budget();
                if !plan.fits_budget(&budget) {
                    return ExecDecision::Refuse(ExecRefusal::BudgetExceeded {
                        required_bytes: plan.footprint_bytes(),
                        available_bytes: budget.available_bytes(),
                    });
                }
                ExecDecision::Accept(plan)
            }
            ExecDecision::Refuse(reason) => ExecDecision::Refuse(reason),
        }
    }

    /// 按放置规则探测每个后端，返回包含拒绝原因的可判别结果。
    ///
    /// 供上层规划器（如 `dl-plan`）把 DL 签名接到真实后端状态，不需要重复实现健康、
    /// 提交状态和预算判断。
    pub fn candidates(&self, signature: &ExecSignature) -> Vec<BackendCandidate> {
        (0..self.backends.len())
            .map(|index| {
                let backend = &self.backends[index];
                BackendCandidate { backend: backend.id(), device: backend.device(), decision: self.feasibility(index, signature) }
            })
            .collect()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// 一次放置结果：执行顺序与每个任务选中的后端下标。
struct Placement {
    order: Vec<TaskId>,
    backends: BTreeMap<TaskId, usize>,
}

fn record_placement_failure(tracer: &mut Option<Tracer>, error: &Error) {
    if let Some(tracer) = tracer.as_mut() {
        tracer.record(EventKind::DispatchFailed { task: "placement".into(), message: error.to_string() });
    }
}

/// 把任务描述翻译成可执行性签名；未知元数据留空，由后端按自身支持范围判断。
fn signature_of(task: &hardware_ir::TaskNode) -> ExecSignature {
    fn text(params: &BTreeMap<String, hardware_ir::ParamValue>, key: &str) -> Option<String> {
        match params.get(key) {
            Some(hardware_ir::ParamValue::Str(value)) => Some(value.clone()),
            _ => None,
        }
    }

    let op = match &task.kind {
        TaskKind::Custom(name) => name.clone(),
        other => format!("{other:?}"),
    };
    let dtypes = text(&task.params, "dtype").into_iter().collect();
    let mut signature = ExecSignature::operator(op, dtypes, Vec::new());
    if let Some(layout) = text(&task.params, "layout") {
        signature = signature.with_layout(layout);
    }
    if let Some(quantization) = text(&task.params, "quantization") {
        signature = signature.with_quantization(quantization);
    }
    if let Some(precision) = text(&task.params, "precision") {
        signature = signature.with_precision(precision);
    }
    signature
}

/// 旧的种类/交互偏好，折算成代价权重；偏好命中为 0，其余按注册顺序兜底。
fn legacy_weight(task: &hardware_ir::TaskNode, backend: &str) -> u64 {
    const PREFERRED: u64 = 0;
    const INTERACTIVE_FALLBACK: u64 = 50;
    const OTHER: u64 = 100;

    let prefer_cuda = matches!(task.kind, TaskKind::MatMul | TaskKind::Dispatch);
    let prefer_cpu = matches!(task.kind, TaskKind::Copy | TaskKind::Fill);
    let prefer_graphics = matches!(task.kind, TaskKind::Raster | TaskKind::RtTrace);
    if !(prefer_cuda || prefer_cpu || prefer_graphics) {
        return PREFERRED;
    }

    let preferred = if prefer_cuda {
        backend.contains("cuda") || backend == "cpu"
    }
    else if prefer_cpu {
        backend == "cpu"
    }
    else {
        backend.contains("dx") || backend.contains("vulkan")
    };
    if preferred {
        return PREFERRED;
    }

    let interactive = task.priority == Priority::Interactive || matches!(task.objective, Objective::MinLatency { .. });
    if interactive && backend.contains("dx") { INTERACTIVE_FALLBACK } else { OTHER }
}

fn schedule_order(graph: &Graph, deadlines: &BTreeMap<TaskId, Option<u64>>) -> Result<Vec<TaskId>> {
    // 就绪顺序是领域语义（优先级档、deadline、TaskId 三级键），排序核是共享的：
    // `topological_order_by` 每步取键最小的就绪任务，与旧的本地 Kahn 逐位一致。
    let keys: BTreeMap<TaskId, (i32, u64, TaskId)> = graph.tasks.nodes.iter().map(|task| (task.id, ready_key(task, deadlines))).collect();
    boost_math::graph_theory::topological_order_by(&graph.tasks, |id| keys[id]).map_err(|_| Error::Ir(hardware_ir::Error::Cycle))
}

fn task_label(task: &hardware_ir::TaskNode) -> String {
    let kind = match &task.kind {
        TaskKind::Custom(name) => name.clone(),
        other => format!("{other:?}"),
    };
    format!("{kind}#{}", task.id.0)
}

fn parallel_count(task: &hardware_ir::TaskNode) -> u32 {
    match task.dispatch {
        Dispatch::Explicit { x, y, z } => x.saturating_mul(y).saturating_mul(z),
        Dispatch::Auto => 0,
    }
}

fn ready_key(task: &hardware_ir::TaskNode, deadlines: &BTreeMap<TaskId, Option<u64>>) -> (i32, u64, TaskId) {
    let prio_rank = match task.priority {
        Priority::Interactive => 0,
        Priority::Streaming => 1,
        Priority::Background => 2,
        Priority::Batch => 3,
    };
    let deadline = deadlines.get(&task.id).copied().flatten().unwrap_or(u64::MAX);
    (prio_rank, deadline, task.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap as Map,
        sync::{Arc, Mutex},
    };

    use hardware_ir::{DepEdge, DepKind, Graph, Objective, Priority, ResourceGraph, ShaderId, TaskGraph, TaskKind, TaskNode};

    use crate::noop::NoopBackend;

    struct OrderBackend {
        name: String,
        order: Arc<Mutex<Vec<TaskId>>>,
    }

    impl Backend for OrderBackend {
        fn name(&self) -> &str {
            &self.name
        }

        fn features(&self) -> hardware_capability::FeatureSet {
            hardware_capability::FeatureSet::new()
        }

        fn submit_task(&mut self, _graph: &Graph, task: &hardware_ir::TaskNode) -> Result<()> {
            self.order.lock().unwrap().push(task.id);
            Ok(())
        }
    }

    fn task(id: u64, priority: Priority, objective: Objective) -> TaskNode {
        TaskNode {
            id: TaskId(id),
            kind: TaskKind::Custom("x".into()),
            shader: ShaderId(id),
            params: Map::new(),
            dispatch: Default::default(),
            objective,
            priority,
        }
    }

    struct FlushBackend {
        name: String,
        order: Arc<Mutex<Vec<String>>>,
    }

    impl Backend for FlushBackend {
        fn name(&self) -> &str {
            &self.name
        }

        fn features(&self) -> hardware_capability::FeatureSet {
            hardware_capability::FeatureSet::new()
        }

        fn submit_task(&mut self, _graph: &Graph, task: &hardware_ir::TaskNode) -> Result<()> {
            self.order.lock().unwrap().push(format!("submit:{}:{}", self.name, task.id.0));
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            self.order.lock().unwrap().push(format!("flush:{}", self.name));
            Ok(())
        }
    }

    fn pinned(id: u64, backend: &str) -> TaskNode {
        let mut params = Map::new();
        params.insert("backend".into(), hardware_ir::ParamValue::Str(backend.into()));
        TaskNode {
            id: TaskId(id),
            kind: TaskKind::Custom("x".into()),
            shader: ShaderId(id),
            params,
            dispatch: Default::default(),
            objective: Objective::MaxThroughput,
            priority: Priority::Batch,
        }
    }

    #[test]
    fn pin_and_flush_when_crossing_backends() {
        let graph = Graph {
            resources: ResourceGraph { nodes: vec![] },
            tasks: TaskGraph {
                nodes: vec![pinned(1, "dx12"), pinned(2, "cuda")],
                edges: vec![DepEdge { from_task: Some(TaskId(1)), from_resource: None, to_task: TaskId(2), kind: DepKind::Execution }],
            },
        };
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(FlushBackend { name: "dx12".into(), order: log.clone() }));
        sched.register(Box::new(FlushBackend { name: "cuda".into(), order: log.clone() }));
        sched.execute(&graph).expect("execute");
        assert_eq!(*log.lock().unwrap(), vec!["submit:dx12:1".to_string(), "flush:dx12".to_string(), "submit:cuda:2".to_string(),]);
        let switches = sched.take_backend_switches();
        assert_eq!(switches, vec![BackendSwitch { from_backend: "dx12".into(), to_backend: "cuda".into(), at_task: TaskId(2) }]);
    }

    #[test]
    fn interactive_before_batch_when_independent() {
        let graph = Graph {
            resources: ResourceGraph { nodes: vec![] },
            tasks: TaskGraph {
                nodes: vec![
                    task(1, Priority::Batch, Objective::MaxThroughput),
                    task(2, Priority::Interactive, Objective::MinLatency { deadline_micros: 10 }),
                ],
                edges: vec![],
            },
        };
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(OrderBackend { name: "cpu".into(), order: order.clone() }));
        sched.execute(&graph).expect("execute");
        assert_eq!(*order.lock().unwrap(), vec![TaskId(2), TaskId(1)]);
    }

    #[test]
    fn interactive_120hz_not_starved_by_many_batch() {
        use crate::{FRAME_BUDGET_120HZ_MICROS, fits_120hz_frame};

        assert!(fits_120hz_frame(FRAME_BUDGET_120HZ_MICROS));
        assert!(!fits_120hz_frame(FRAME_BUDGET_120HZ_MICROS + 1));

        let mut nodes = vec![task(100, Priority::Interactive, Objective::MinLatency { deadline_micros: FRAME_BUDGET_120HZ_MICROS })];
        for id in 1..=16u64 {
            nodes.push(task(id, Priority::Batch, Objective::MaxThroughput));
        }
        let graph = Graph { resources: ResourceGraph { nodes: vec![] }, tasks: TaskGraph { nodes, edges: vec![] } };
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(OrderBackend { name: "cpu".into(), order: order.clone() }));
        sched.execute(&graph).expect("execute");
        let submitted = order.lock().unwrap().clone();
        assert_eq!(submitted[0], TaskId(100), "120 Hz Interactive must run before ready Batch work");
    }

    #[test]
    fn tighter_deadline_first_when_same_priority() {
        let graph = Graph {
            resources: ResourceGraph { nodes: vec![] },
            tasks: TaskGraph {
                nodes: vec![
                    task(1, Priority::Batch, Objective::MinLatency { deadline_micros: 200 }),
                    task(2, Priority::Batch, Objective::MinLatency { deadline_micros: 50 }),
                ],
                edges: vec![],
            },
        };
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(OrderBackend { name: "cpu".into(), order: order.clone() }));
        // Also register noop so Custom can land on first backend (cpu).
        let _ = NoopBackend;
        sched.execute(&graph).expect("execute");
        assert_eq!(*order.lock().unwrap(), vec![TaskId(2), TaskId(1)]);
    }

    #[test]
    fn respects_dependency_before_priority() {
        let graph = Graph {
            resources: ResourceGraph { nodes: vec![] },
            tasks: TaskGraph {
                nodes: vec![task(1, Priority::Batch, Objective::MaxThroughput), task(2, Priority::Interactive, Objective::MaxThroughput)],
                edges: vec![DepEdge { from_task: Some(TaskId(1)), from_resource: None, to_task: TaskId(2), kind: DepKind::Execution }],
            },
        };
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(OrderBackend { name: "cpu".into(), order: order.clone() }));
        sched.execute(&graph).expect("execute");
        assert_eq!(*order.lock().unwrap(), vec![TaskId(1), TaskId(2)]);
    }

    struct ProbeBackend {
        name: String,
    }

    impl Backend for ProbeBackend {
        fn name(&self) -> &str {
            &self.name
        }

        fn features(&self) -> hardware_capability::FeatureSet {
            hardware_capability::FeatureSet::new()
        }

        fn submit_task(&mut self, _graph: &Graph, _task: &hardware_ir::TaskNode) -> Result<()> {
            Ok(())
        }

        fn take_probe_events(&mut self) -> Vec<BackendProbeEvent> {
            vec![BackendProbeEvent::new(self.name.clone(), crate::probe_kinds::MODULE_COMPILE, Some(0x1234))]
        }
    }

    #[test]
    fn records_backend_probes_in_trace() {
        let nodes = vec![task(1, Priority::Batch, Objective::MaxThroughput)];
        let graph = Graph { resources: ResourceGraph { nodes: vec![] }, tasks: TaskGraph { nodes, edges: vec![] } };
        let mut sched = Scheduler::new().with_tracer(Tracer::new());
        sched.register(Box::new(ProbeBackend { name: "cuda".into() }));
        sched.execute(&graph).expect("execute");

        let probes = sched.take_backend_probes();
        assert_eq!(probes, vec![BackendProbeEvent::new("cuda", crate::probe_kinds::MODULE_COMPILE, Some(0x1234))]);

        let trace = sched.take_trace().expect("trace");
        assert!(trace.events.iter().any(|event| matches!(
            &event.kind,
            EventKind::BackendProbe { backend, kind, shader_key }
                if backend == "cuda" && kind.as_str() == crate::probe_kinds::MODULE_COMPILE && *shader_key == Some(0x1234)
        )));
    }

    struct PlacementBackend {
        name: String,
        cost: u64,
        temp_bytes: u64,
        segment_bytes: Option<u64>,
        refusal: Option<hardware_capability::ExecRefusal>,
        budget: hardware_capability::MemoryBudget,
        prepared: Arc<Mutex<Vec<String>>>,
        submitted: Arc<Mutex<Vec<String>>>,
    }

    impl PlacementBackend {
        fn new(name: &str, cost: u64, prepared: Arc<Mutex<Vec<String>>>, submitted: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                name: name.into(),
                cost,
                temp_bytes: 64,
                segment_bytes: None,
                refusal: None,
                budget: hardware_capability::MemoryBudget::unlimited(),
                prepared,
                submitted,
            }
        }

        fn refusing(mut self, refusal: hardware_capability::ExecRefusal) -> Self {
            self.refusal = Some(refusal);
            self
        }

        fn with_budget(mut self, budget: hardware_capability::MemoryBudget) -> Self {
            self.budget = budget;
            self
        }

        fn with_temp_bytes(mut self, temp_bytes: u64) -> Self {
            self.temp_bytes = temp_bytes;
            self
        }

        fn with_segment_bytes(mut self, segment_bytes: u64) -> Self {
            self.segment_bytes = Some(segment_bytes);
            self
        }
    }

    impl Backend for PlacementBackend {
        fn name(&self) -> &str {
            &self.name
        }

        fn features(&self) -> hardware_capability::FeatureSet {
            hardware_capability::FeatureSet::new()
        }

        fn budget(&self) -> hardware_capability::MemoryBudget {
            self.budget
        }

        fn supports(&self, _signature: &hardware_capability::ExecSignature) -> hardware_capability::ExecDecision {
            match &self.refusal {
                Some(reason) => hardware_capability::ExecDecision::Refuse(reason.clone()),
                None => {
                    let mut plan = hardware_capability::ExecPlan::new(self.name.clone(), self.name.clone())
                        .with_temp_bytes(self.temp_bytes)
                        .with_estimated_cost(self.cost);
                    if let Some(segment) = self.segment_bytes {
                        plan = plan.with_max_segment_bytes(segment);
                    }
                    hardware_capability::ExecDecision::Accept(plan)
                }
            }
        }

        fn prepare(&mut self, _graph: &Graph) -> Result<()> {
            self.prepared.lock().unwrap().push(self.name.clone());
            Ok(())
        }

        fn submit_task(&mut self, _graph: &Graph, task: &hardware_ir::TaskNode) -> Result<()> {
            self.submitted.lock().unwrap().push(format!("{}:{}", self.name, task.id.0));
            Ok(())
        }
    }

    fn single_task_graph() -> Graph {
        Graph {
            resources: ResourceGraph { nodes: vec![] },
            tasks: TaskGraph { nodes: vec![task(1, Priority::Batch, Objective::MaxThroughput)], edges: vec![] },
        }
    }

    #[test]
    fn placement_picks_lowest_cost_feasible_backend() {
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(PlacementBackend::new("cpu", 20, prepared.clone(), submitted.clone())));
        sched.register(Box::new(PlacementBackend::new("dx12", 5, prepared.clone(), submitted.clone())));
        sched.execute(&single_task_graph()).expect("execute");

        assert_eq!(*prepared.lock().unwrap(), vec!["dx12".to_string()], "only the selected backend is prepared");
        assert_eq!(*submitted.lock().unwrap(), vec!["dx12:1".to_string()]);
    }

    #[test]
    fn placement_filters_refused_backend_and_reports_reason() {
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(
            PlacementBackend::new("cpu", 0, prepared.clone(), submitted.clone())
                .refusing(hardware_capability::ExecRefusal::UnsupportedOp("matmul".into())),
        ));
        sched.register(Box::new(PlacementBackend::new("dx12", 90, prepared.clone(), submitted.clone())));
        sched.execute(&single_task_graph()).expect("fallback backend accepted");
        assert_eq!(*submitted.lock().unwrap(), vec!["dx12:1".to_string()]);

        let mut sched = Scheduler::new();
        sched.register(Box::new(
            PlacementBackend::new("cpu", 0, prepared.clone(), submitted.clone())
                .refusing(hardware_capability::ExecRefusal::UnsupportedOp("matmul".into())),
        ));
        let error = sched.execute(&single_task_graph()).expect_err("no feasible backend");
        assert_eq!(error.code(), crate::ErrorCode::Infeasible);
        assert!(error.to_string().contains("unsupported op `matmul`"), "{error}");
    }

    #[test]
    fn placement_rejects_over_budget_plan() {
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(
            PlacementBackend::new("cpu", 0, prepared.clone(), submitted.clone()).with_budget(hardware_capability::MemoryBudget::new(32, 0, 0)),
        ));
        let error = sched.execute(&single_task_graph()).expect_err("over budget");
        assert_eq!(error.code(), crate::ErrorCode::Infeasible);
        assert!(error.to_string().contains("budget exceeded"), "{error}");
    }

    #[test]
    fn segmented_plan_is_accepted_when_one_segment_fits() {
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(
            PlacementBackend::new("cpu", 0, prepared.clone(), submitted.clone())
                .with_temp_bytes(1024)
                .with_budget(hardware_capability::MemoryBudget::new(512, 0, 0)),
        ));
        let error = sched.execute(&single_task_graph()).expect_err("flat plan over budget");
        assert_eq!(error.code(), crate::ErrorCode::Infeasible);

        let mut sched = Scheduler::new();
        sched.register(Box::new(
            PlacementBackend::new("gpu", 0, prepared.clone(), submitted.clone())
                .with_temp_bytes(1024)
                .with_budget(hardware_capability::MemoryBudget::new(512, 0, 0)),
        ));
        sched.register(Box::new(
            PlacementBackend::new("npu", 10, prepared.clone(), submitted.clone()).with_temp_bytes(1024).with_segment_bytes(256),
        ));
        sched.execute(&single_task_graph()).expect("segmented backend accepted");
        assert_eq!(*submitted.lock().unwrap(), vec!["npu:1".to_string()]);
    }

    #[test]
    fn pinned_backend_is_confirmed_against_feasibility() {
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(
            PlacementBackend::new("cpu", 0, prepared.clone(), submitted.clone())
                .refusing(hardware_capability::ExecRefusal::UnsupportedPrecision("f16".into())),
        ));
        sched.register(Box::new(PlacementBackend::new("dx12", 0, prepared.clone(), submitted.clone())));
        let graph = Graph { resources: ResourceGraph { nodes: vec![] }, tasks: TaskGraph { nodes: vec![pinned(1, "cpu")], edges: vec![] } };
        let error = sched.execute(&graph).expect_err("pin cannot bypass feasibility");
        assert_eq!(error.code(), crate::ErrorCode::Infeasible);
        assert!(error.to_string().contains("unsupported precision: f16"), "{error}");
    }

    #[test]
    fn candidates_report_plans_and_refusals_with_device_identity() {
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let mut sched = Scheduler::new();
        sched.register(Box::new(
            PlacementBackend::new("gpu", 0, prepared.clone(), submitted.clone())
                .with_temp_bytes(1024)
                .with_budget(hardware_capability::MemoryBudget::new(512, 0, 0)),
        ));
        sched.register(Box::new(PlacementBackend::new("cpu", 0, prepared, submitted)));

        let signature = hardware_capability::ExecSignature::operator("matmul", vec!["f32".into()], Vec::new());
        let candidates = sched.candidates(&signature);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].backend.as_str(), "gpu");
        assert_eq!(candidates[0].device.as_str(), "gpu");
        assert!(matches!(
            candidates[0].decision,
            hardware_capability::ExecDecision::Refuse(hardware_capability::ExecRefusal::BudgetExceeded { .. })
        ));
        assert!(candidates[1].decision.is_accept());
    }
}
