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
use hardware_capability::CapabilityReport;

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
    /// Idempotent when resource sizes match an existing allocation.
    pub fn prepare(&mut self, graph: &Graph) -> Result<()> {
        graph.validate()?;
        for backend in &mut self.backends {
            backend.prepare(graph)?;
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
    /// name (exact or substring). Crossing backends calls [`Backend::flush`] on the
    /// previous backend before the next submit.
    pub fn execute(&mut self, graph: &Graph) -> Result<()> {
        self.prepare(graph)?;
        self.dispatch(graph)
    }

    fn dispatch(&mut self, graph: &Graph) -> Result<()> {
        self.last_backend_switches.clear();
        self.last_backend_probes.clear();
        let mut tracer = self.tracer.take();
        let result = self.dispatch_inner(graph, &mut tracer);
        self.tracer = tracer;
        result
    }

    fn dispatch_inner(&mut self, graph: &Graph, tracer: &mut Option<Tracer>) -> Result<()> {
        let deadlines = effective_deadlines(graph);
        let order = schedule_order(graph, &deadlines)?;
        let mut last_backend: Option<usize> = None;
        for task_id in order {
            let task = graph.tasks.nodes.iter().find(|t| t.id == task_id).expect("task in order must exist");
            let label = task_label(task);
            let backend_idx = match self.pick_backend_index(task) {
                Ok(index) => index,
                Err(error) => {
                    if let Some(tracer) = tracer.as_mut() {
                        tracer.record(EventKind::DispatchFailed { task: label, message: error.to_string() });
                    }
                    return Err(error);
                }
            };
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
            } else {
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

    fn pick_backend_index(&self, task: &hardware_ir::TaskNode) -> Result<usize> {
        if let Some(hardware_ir::ParamValue::Str(name)) = task.params.get("backend") {
            let pinned = self.backends.iter().position(|b| b.name() == name || b.name().contains(name.as_str()));
            if let Some(idx) = pinned {
                return Ok(idx);
            }
            return Err(Error::NoBackend(format!("pinned backend `{name}` not registered")));
        }

        let prefer_cuda = matches!(task.kind, hardware_ir::TaskKind::MatMul | hardware_ir::TaskKind::Dispatch);
        let prefer_cpu = matches!(task.kind, hardware_ir::TaskKind::Copy | hardware_ir::TaskKind::Fill);
        let prefer_graphics = matches!(task.kind, hardware_ir::TaskKind::Raster | hardware_ir::TaskKind::RtTrace);

        let idx = self
            .backends
            .iter()
            .position(|b| {
                if prefer_cuda {
                    b.name().contains("cuda") || b.name() == "cpu"
                } else if prefer_cpu {
                    b.name() == "cpu"
                } else if prefer_graphics {
                    b.name().contains("dx") || b.name().contains("vulkan")
                } else {
                    true
                }
            })
            .or_else(|| {
                if task.priority == Priority::Interactive || matches!(task.objective, Objective::MinLatency { .. }) {
                    self.backends.iter().position(|b| b.name().contains("dx"))
                } else {
                    None
                }
            })
            .or_else(|| if self.backends.is_empty() { None } else { Some(0) });

        idx.ok_or_else(|| Error::NoBackend(format!("{task_kind:?}", task_kind = task.kind)))
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
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
}
