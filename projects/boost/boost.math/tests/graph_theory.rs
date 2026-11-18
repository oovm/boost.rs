//! `boost-math` 图论算法验收。
//!
//! 同一份算法分别消费内存图与只读 CSR 两种表示，证明算法与容器解耦：
//! 拓扑序的确定性、空图与孤点、环（含自环）失败、无向图拒绝方向遍历，
//! 以及可达集合的广度优先顺序、环安全与未知起点失败。

use boost_collection::{CsrGraph, GraphError, MemoryGraph};
use boost_math::graph_theory::{reachable_from, topological_order};

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
fn topological_order_is_deterministic_and_reports_cycles() {
    let graph = chain();
    assert_eq!(topological_order(&graph), Ok(vec!["a", "b", "c"]));

    // 零入度顶点按来源的顶点顺序入队，不依赖哈希迭代。
    let mut independent = MemoryGraph::<&'static str>::new(true);
    for vertex in ["second", "first"] {
        independent.add_vertex(vertex).expect("unique vertex");
    }
    assert_eq!(topological_order(&independent), Ok(vec!["second", "first"]));

    let mut cycle = chain();
    cycle.add_edge("c", "a", ()).expect("known endpoints");
    assert_eq!(topological_order(&cycle), Err(GraphError::Cycle));

    let mut self_loop = MemoryGraph::<&'static str>::new(true);
    self_loop.add_vertex("a").expect("unique vertex");
    self_loop.add_edge("a", "a", ()).expect("known endpoints");
    assert_eq!(topological_order(&self_loop), Err(GraphError::Cycle));
}

#[test]
fn topological_order_separates_empty_from_cycle() {
    let empty = MemoryGraph::<&'static str>::new(true);
    assert_eq!(topological_order(&empty), Ok(Vec::new()));

    let mut graph = MemoryGraph::<&'static str>::new(true);
    graph.add_vertex("solo").expect("unique vertex");
    assert_eq!(topological_order(&graph), Ok(vec!["solo"]));
}

#[test]
fn topological_order_rejects_undirected_graphs() {
    let mut graph = MemoryGraph::<&'static str>::new(false);
    graph.add_vertex("a").expect("unique vertex");
    graph.add_vertex("b").expect("unique vertex");
    graph.add_edge("a", "b", ()).expect("known endpoints");
    assert_eq!(topological_order(&graph), Err(GraphError::UndirectedTraversal));
}

#[test]
fn the_same_algorithm_consumes_the_csr_representation() {
    let memory = chain();
    let csr = CsrGraph::from_graph(&memory);
    // 顶点顺序沿用来源：c=0, a=1, b=2。
    assert_eq!(topological_order(&csr), Ok(vec![1, 2, 0]));

    let mut parallel = MemoryGraph::<&'static str>::new(true);
    parallel.add_vertex("a").expect("unique vertex");
    parallel.add_vertex("b").expect("unique vertex");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    parallel.add_edge("a", "b", ()).expect("known endpoints");
    let csr = CsrGraph::from_graph(&parallel);
    assert_eq!(topological_order(&csr), Ok(vec![0, 1]));
}

#[test]
fn reachable_from_follows_edges_in_breadth_first_order() {
    let graph = chain();
    assert_eq!(reachable_from(&graph, &"a"), Ok(vec!["a", "b", "c"]));
    assert_eq!(reachable_from(&graph, &"c"), Ok(vec!["c"]));
}

#[test]
fn reachable_from_terminates_on_cycles_and_self_loops() {
    let mut cycle = chain();
    cycle.add_edge("c", "a", ()).expect("known endpoints");
    assert_eq!(reachable_from(&cycle, &"a"), Ok(vec!["a", "b", "c"]));

    let mut self_loop = MemoryGraph::<&'static str>::new(true);
    self_loop.add_vertex("a").expect("unique vertex");
    self_loop.add_edge("a", "a", ()).expect("known endpoints");
    assert_eq!(reachable_from(&self_loop, &"a"), Ok(vec!["a"]));
}

#[test]
fn reachable_from_reports_unknown_start() {
    let graph = chain();
    assert_eq!(reachable_from(&graph, &"missing"), Err(GraphError::UnknownVertex));
}

#[test]
fn reachable_from_an_undirected_graph_is_its_component() {
    let mut graph = MemoryGraph::<&'static str>::new(false);
    for vertex in ["a", "b", "island"] {
        graph.add_vertex(vertex).expect("unique vertex");
    }
    graph.add_edge("a", "b", ()).expect("known endpoints");
    assert_eq!(reachable_from(&graph, &"a"), Ok(vec!["a", "b"]));
    assert_eq!(reachable_from(&graph, &"island"), Ok(vec!["island"]));
}

#[test]
fn reachable_from_consumes_the_csr_representation() {
    let csr = CsrGraph::from_graph(&chain());
    assert_eq!(reachable_from(&csr, &1), Ok(vec![1, 2, 0]));
    assert_eq!(reachable_from(&csr, &0), Ok(vec![0]));
}
