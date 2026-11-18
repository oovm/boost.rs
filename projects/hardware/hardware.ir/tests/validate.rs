use hardware_ir::{
    Access, DepEdge, DepKind, Domain, Error, Graph, Objective, Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, ShaderId,
    TaskGraph, TaskId, TaskKind, TaskNode,
};

fn buffer(id: u64) -> ResourceNode {
    ResourceNode { id: ResourceId(id), kind: ResourceKind::Buffer, domain: Domain::Vram, access: Access::ReadWrite, byte_size: Some(64) }
}

fn task(id: u64) -> TaskNode {
    TaskNode {
        id: TaskId(id),
        kind: TaskKind::Copy,
        shader: ShaderId(id),
        params: Default::default(),
        dispatch: Default::default(),
        objective: Objective::MaxThroughput,
        priority: Priority::Batch,
    }
}

#[test]
fn rejects_duplicate_resource_id() {
    let graph = Graph { resources: ResourceGraph { nodes: vec![buffer(1), buffer(1)] }, tasks: TaskGraph::default() };
    assert_eq!(graph.validate(), Err(Error::DuplicateResource(1)));
}

#[test]
fn rejects_duplicate_task_id() {
    let graph = Graph { resources: ResourceGraph::default(), tasks: TaskGraph { nodes: vec![task(10), task(10)], edges: vec![] } };
    assert_eq!(graph.validate(), Err(Error::DuplicateTask(10)));
}

#[test]
fn rejects_unknown_to_task() {
    let graph = Graph {
        resources: ResourceGraph::default(),
        tasks: TaskGraph {
            nodes: vec![task(1)],
            edges: vec![DepEdge { from_task: Some(TaskId(1)), from_resource: None, to_task: TaskId(99), kind: DepKind::Execution }],
        },
    };
    assert_eq!(graph.validate(), Err(Error::UnknownTask(99)));
}

#[test]
fn rejects_unknown_from_task() {
    let graph = Graph {
        resources: ResourceGraph::default(),
        tasks: TaskGraph {
            nodes: vec![task(2)],
            edges: vec![DepEdge { from_task: Some(TaskId(1)), from_resource: None, to_task: TaskId(2), kind: DepKind::Execution }],
        },
    };
    assert_eq!(graph.validate(), Err(Error::UnknownTask(1)));
}

#[test]
fn rejects_unknown_from_resource() {
    let graph = Graph {
        resources: ResourceGraph::default(),
        tasks: TaskGraph {
            nodes: vec![task(10)],
            edges: vec![DepEdge { from_task: None, from_resource: Some(ResourceId(7)), to_task: TaskId(10), kind: DepKind::Data }],
        },
    };
    assert_eq!(graph.validate(), Err(Error::UnknownResource(7)));
}

#[test]
fn rejects_empty_edge_source() {
    let graph = Graph {
        resources: ResourceGraph::default(),
        tasks: TaskGraph {
            nodes: vec![task(10)],
            edges: vec![DepEdge { from_task: None, from_resource: None, to_task: TaskId(10), kind: DepKind::Execution }],
        },
    };
    assert_eq!(graph.validate(), Err(Error::EmptyEdgeSource(10)));
}

#[test]
fn rejects_dependency_cycle() {
    let graph = Graph {
        resources: ResourceGraph::default(),
        tasks: TaskGraph {
            nodes: vec![task(1), task(2)],
            edges: vec![
                DepEdge { from_task: Some(TaskId(1)), from_resource: None, to_task: TaskId(2), kind: DepKind::Execution },
                DepEdge { from_task: Some(TaskId(2)), from_resource: None, to_task: TaskId(1), kind: DepKind::Execution },
            ],
        },
    };
    assert_eq!(graph.validate(), Err(Error::Cycle));
}

#[test]
fn accepts_data_edge_and_execution_chain() {
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1)] },
        tasks: TaskGraph {
            nodes: vec![task(10), task(11)],
            edges: vec![
                DepEdge { from_task: None, from_resource: Some(ResourceId(1)), to_task: TaskId(10), kind: DepKind::Data },
                DepEdge { from_task: Some(TaskId(10)), from_resource: None, to_task: TaskId(11), kind: DepKind::Execution },
            ],
        },
    };
    graph.validate().expect("valid");
}

#[test]
fn topological_order_always_takes_the_smallest_ready_task() {
    // 任务按 5、3、4 声明；3 与 4 就绪，必须先出 3，再出 4，最后才轮到依赖它们的 5。
    let tasks = TaskGraph {
        nodes: vec![task(5), task(3), task(4)],
        edges: vec![
            DepEdge { from_task: Some(TaskId(3)), from_resource: None, to_task: TaskId(5), kind: DepKind::Execution },
            DepEdge { from_task: Some(TaskId(4)), from_resource: None, to_task: TaskId(5), kind: DepKind::Execution },
            // 仅资源边不参与排序。
            DepEdge { from_task: None, from_resource: Some(ResourceId(1)), to_task: TaskId(4), kind: DepKind::Data },
        ],
    };
    assert_eq!(tasks.topological_order(), Ok(vec![TaskId(3), TaskId(4), TaskId(5)]));
}
