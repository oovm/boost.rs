//! `boost-collection` 图能力与跨表示验收。
//!
//! 覆盖：内存图的身份、平行边与自环策略、空图与孤点、拓扑序的确定性与
//! 环失败、无向图拒绝，以及同一算法直接消费只读 CSR 表示。

use boost_collection::{CsrGraph, GraphError, GraphRead, MemoryGraph, OutgoingEdges, topological_order};

fn chain() -> MemoryGraph<&'static str> {
    let mut graph = MemoryGraph::<&'static str>::new(true);
    for vertex in ["c", "a", "b"] {
        graph.add_vertex(vertex).expect("unique vertex");
    }
    graph.add_edge("a", "b", ()).expect("known endpoints");
    graph.add_edge("b", "c", ()).expect("known endpoints");
    graph
}

#[test]
fn memory_graph_keeps_identity_and_parallel_edges() {
    let graph = chain();
    assert!(graph.is_directed());
    assert_eq!(graph.vertex_count(), 3);
    assert_eq!(graph.edge_count(), 2);
    assert_eq!(graph.vertices().copied().collect::<Vec<_>>(), vec!["c", "a", "b"]);

    let mut parallel = MemoryGraph::<&'static str, u32>::new(true);
    parallel.add_vertex("a").expect("unique vertex");
    parallel.add_vertex("b").expect("unique vertex");
    let first = parallel.add_edge("a", "b", 1).expect("known endpoints");
    let second = parallel.add_edge("a", "b", 2).expect("known endpoints");
    assert_ne!(first, second);
    assert_eq!(parallel.edge_count(), 2);
    assert_eq!(parallel.edge_payload(second), Some(&2));
    assert_eq!(parallel.outgoing(&"a").collect::<Vec<_>>(), vec![first, second]);
}

#[test]
fn memory_graph_rejects_duplicate_vertices_and_unknown_endpoints() {
    let mut graph = MemoryGraph::<&'static str>::new(true);
    graph.add_vertex("a").expect("first insert");
    assert_eq!(graph.add_vertex("a"), Err(GraphError::DuplicateVertex));
    assert_eq!(graph.add_edge("a", "b", ()), Err(GraphError::UnknownVertex));
}

#[test]
fn topological_order_is_deterministic_and_reports_cycles() {
    let graph = chain();
    assert_eq!(topological_order(&graph), Ok(vec!["a", "b", "c"]));

    let mut cycle = chain();
    cycle.add_edge("c", "a", ()).expect("known endpoints");
    assert_eq!(topological_order(&cycle), Err(GraphError::Cycle));

    let mut self_loop = MemoryGraph::<&'static str>::new(true);
    self_loop.add_vertex("a").expect("unique vertex");
    self_loop.add_edge("a", "a", ()).expect("known endpoints");
    assert_eq!(topological_order(&self_loop), Err(GraphError::Cycle));
}

#[test]
fn empty_graph_and_isolated_vertices_stay_visible() {
    let empty = MemoryGraph::<&'static str>::new(true);
    assert_eq!(topological_order(&empty), Ok(Vec::new()));

    let mut graph = MemoryGraph::<&'static str>::new(true);
    graph.add_vertex("solo").expect("unique vertex");
    assert_eq!(topological_order(&graph), Ok(vec!["solo"]));
}

#[test]
fn undirected_graphs_reject_directional_traversal() {
    let mut graph = MemoryGraph::<&'static str>::new(false);
    graph.add_vertex("a").expect("unique vertex");
    graph.add_vertex("b").expect("unique vertex");
    graph.add_edge("a", "b", ()).expect("known endpoints");
    assert_eq!(graph.edge_count(), 1);
    assert_eq!(graph.outgoing(&"b").collect::<Vec<_>>(), vec![0]);
    assert_eq!(topological_order(&graph), Err(GraphError::UndirectedTraversal));
}

#[test]
fn the_same_algorithm_consumes_the_csr_representation() {
    let memory = chain();
    let csr = CsrGraph::from_graph(&memory);
    assert_eq!(csr.vertex_count(), 3);
    assert_eq!(csr.edge_count(), 2);
    // 顶点顺序沿用来源：c=0, a=1, b=2。
    assert_eq!(csr.endpoints(0), Some((&1, &2)));
    assert_eq!(csr.outgoing(&1).collect::<Vec<_>>(), vec![0]);
    assert_eq!(topological_order(&csr), Ok(vec![1, 2, 0]));

    let mut parallel = MemoryGraph::<&'static str>::new(true);
    parallel.add_vertex("a").expect("unique vertex");
    parallel.add_vertex("b").expect("unique vertex");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    let csr = CsrGraph::from_graph(&parallel);
    assert_eq!(csr.edge_count(), 2);
    assert_eq!(csr.outgoing(&0).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(topological_order(&csr), Ok(vec![0, 1]));
}

#[test]
fn malformed_edge_descriptors_are_reported() {
    let graph = chain();
    assert_eq!(graph.endpoints(9), None);
    let csr = CsrGraph::from_graph(&graph);
    assert_eq!(csr.endpoints(9), None);
    assert_eq!(csr.outgoing(&9).collect::<Vec<_>>(), Vec::<usize>::new());
}
