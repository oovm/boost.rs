//! `boost-collection` 图容器与访问能力验收。
//!
//! 覆盖：内存图的身份、平行边与自环策略、空图与孤点、无向边的双向枚举，
//! 以及只读 CSR 表示交出同样的访问能力。图论算法（拓扑序、可达集合）归
//! `boost-math` 的 `graph_theory`，其用例见该 crate 的测试，不在本文件。

use boost_collection::{CsrGraph, GraphError, GraphRead, IncomingEdges, MemoryGraph, OutgoingEdges};

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
fn empty_graph_and_isolated_vertices_stay_visible() {
    let empty = MemoryGraph::<&'static str>::new(true);
    assert_eq!(empty.vertex_count(), 0);
    assert_eq!(empty.edge_count(), 0);
    assert_eq!(empty.vertices().count(), 0);

    let mut graph = MemoryGraph::<&'static str>::new(true);
    graph.add_vertex("solo").expect("unique vertex");
    assert_eq!(graph.vertices().copied().collect::<Vec<_>>(), vec!["solo"]);
    assert_eq!(graph.edge_count(), 0);
    assert_eq!(graph.outgoing(&"solo").count(), 0);
}

#[test]
fn undirected_edges_are_outgoing_in_both_directions() {
    let mut graph = MemoryGraph::<&'static str>::new(false);
    graph.add_vertex("a").expect("unique vertex");
    graph.add_vertex("b").expect("unique vertex");
    graph.add_edge("a", "b", ()).expect("known endpoints");
    assert_eq!(graph.edge_count(), 1);
    assert!(!graph.is_directed());
    assert_eq!(graph.outgoing(&"a").collect::<Vec<_>>(), vec![0]);
    assert_eq!(graph.outgoing(&"b").collect::<Vec<_>>(), vec![0]);
}

#[test]
fn the_csr_representation_exposes_the_same_access() {
    let memory = chain();
    let csr = CsrGraph::from_graph(&memory);
    assert_eq!(csr.vertex_count(), 3);
    assert_eq!(csr.edge_count(), 2);
    assert!(csr.is_directed());
    // 顶点顺序沿用来源：c=0, a=1, b=2。
    assert_eq!(csr.endpoints(0), Some((&1, &2)));
    assert_eq!(csr.outgoing(&1).collect::<Vec<_>>(), vec![0]);

    let mut parallel = MemoryGraph::<&'static str>::new(true);
    parallel.add_vertex("a").expect("unique vertex");
    parallel.add_vertex("b").expect("unique vertex");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    let csr = CsrGraph::from_graph(&parallel);
    assert_eq!(csr.edge_count(), 2);
    assert_eq!(csr.outgoing(&0).collect::<Vec<_>>(), vec![0, 1]);
}

#[test]
fn incoming_edges_are_an_optional_capability() {
    fn in_degree<G: IncomingEdges>(graph: &G, vertex: &G::Vertex) -> usize {
        graph.incoming(vertex).count()
    }

    let graph = chain();
    assert_eq!(in_degree(&graph, &"b"), 1);
    assert_eq!(in_degree(&graph, &"a"), 0);

    let mut parallel = MemoryGraph::<&'static str>::new(true);
    parallel.add_vertex("a").expect("unique vertex");
    parallel.add_vertex("b").expect("unique vertex");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    assert_eq!(in_degree(&parallel, &"b"), 2);

    let mut undirected = MemoryGraph::<&'static str>::new(false);
    undirected.add_vertex("a").expect("unique vertex");
    undirected.add_vertex("b").expect("unique vertex");
    undirected.add_edge("a", "b", ()).expect("known endpoints");
    assert_eq!(in_degree(&undirected, &"b"), 0);
}

#[test]
fn malformed_edge_descriptors_are_reported() {
    let graph = chain();
    assert_eq!(graph.endpoints(9), None);
    let csr = CsrGraph::from_graph(&graph);
    assert_eq!(csr.endpoints(9), None);
    assert_eq!(csr.outgoing(&9).collect::<Vec<_>>(), Vec::<usize>::new());
}
