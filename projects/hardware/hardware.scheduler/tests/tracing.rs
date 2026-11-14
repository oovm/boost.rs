use std::collections::BTreeMap;

use hardware_harness::{AnalysisOptions, EventKind, Tracer, analyze};
use hardware_ir::{Graph, Objective, Priority, ResourceGraph, ShaderId, TaskGraph, TaskId, TaskKind, TaskNode};
use hardware_scheduler::{NoopBackend, Scheduler};

fn fill_task() -> TaskNode {
    TaskNode {
        id: TaskId(1),
        kind: TaskKind::Fill,
        shader: ShaderId(1),
        params: BTreeMap::new(),
        dispatch: Default::default(),
        objective: Objective::MaxThroughput,
        priority: Priority::Batch,
    }
}

#[test]
fn scheduler_records_selection_and_dispatch() {
    let graph = Graph { resources: ResourceGraph { nodes: vec![] }, tasks: TaskGraph { nodes: vec![fill_task()], edges: vec![] } };
    let mut scheduler = Scheduler::new().with_tracer(Tracer::new());
    scheduler.register(Box::new(NoopBackend));
    scheduler.execute(&graph).expect("execute");

    let trace = scheduler.take_trace().expect("trace");
    assert!(trace.events.iter().any(|event| matches!(&event.kind, EventKind::BackendSelected { reason, .. } if reason == "placement")));
    assert!(trace.events.iter().any(|event| matches!(event.kind, EventKind::Dispatched { .. })));
    assert!(trace.spans.iter().any(|span| span.name.starts_with("dispatch ")));
    assert!(analyze(&trace, &AnalysisOptions::default()).is_empty());
}

#[test]
fn scheduler_records_failed_dispatch() {
    let graph = Graph { resources: ResourceGraph { nodes: vec![] }, tasks: TaskGraph { nodes: vec![fill_task()], edges: vec![] } };
    let mut scheduler = Scheduler::new().with_tracer(Tracer::new());
    scheduler.execute(&graph).expect_err("no backend registered");

    let trace = scheduler.take_trace().expect("trace");
    assert!(trace.events.iter().any(|event| matches!(event.kind, EventKind::DispatchFailed { .. })));
}
