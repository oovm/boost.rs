//! `boost-math` 图论算法验收。
//!
//! 同一份算法分别消费内存图与只读 CSR 两种表示，证明算法与容器解耦：
//! 拓扑序的确定性、空图与孤点、环（含自环）失败、无向图拒绝方向遍历，
//! 以及可达集合的广度优先顺序、环安全与未知起点失败。

use boost_collection::{CsrGraph, GraphError, GraphView, MemoryGraph};
use boost_math::graph_theory::{
    ShortestPath, connected_components, cycle_path, reachable_from, shortest_path_non_negative,
    strongly_connected_components, topological_order, topological_order_by,
};

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

#[test]
fn priority_selection_keeps_the_smallest_ready_vertex() {
    // a 与 b 都是零入度；按 FIFO 会先出 a，按最小键会先出 b。
    let mut graph = MemoryGraph::<&'static str>::new(true);
    for vertex in ["a", "b", "c"] {
        graph.add_vertex(vertex).expect("unique vertex");
    }
    graph.add_edge("a", "c", ()).expect("known endpoints");
    graph.add_edge("b", "c", ()).expect("known endpoints");

    assert_eq!(topological_order(&graph), Ok(vec!["a", "b", "c"]));
    assert_eq!(topological_order_by(&graph, |vertex| *vertex), Ok(vec!["a", "b", "c"]));

    // 逆序键证明选择策略真的生效，而不是恰好与插入顺序相同。
    assert_eq!(topological_order_by(&graph, |vertex| std::cmp::Reverse(*vertex)), Ok(vec!["b", "a", "c"]));
}

#[test]
fn priority_selection_reports_cycles_and_undirected_graphs() {
    let mut cycle = chain();
    cycle.add_edge("c", "a", ()).expect("known endpoints");
    assert_eq!(topological_order_by(&cycle, |vertex| *vertex), Err(GraphError::Cycle));

    let mut undirected = MemoryGraph::<&'static str>::new(false);
    undirected.add_vertex("a").expect("unique vertex");
    assert_eq!(topological_order_by(&undirected, |vertex| *vertex), Err(GraphError::UndirectedTraversal));
}

#[test]
fn cycle_path_names_the_closed_walk() {
    let mut graph = chain();
    graph.add_edge("c", "a", ()).expect("known endpoints");
    assert_eq!(cycle_path(&graph), Ok(Some(vec!["c", "a", "b", "c"])));

    assert_eq!(cycle_path(&chain()), Ok(None));

    let mut self_loop = MemoryGraph::<&'static str>::new(true);
    self_loop.add_vertex("a").expect("unique vertex");
    self_loop.add_edge("a", "a", ()).expect("known endpoints");
    assert_eq!(cycle_path(&self_loop), Ok(Some(vec!["a", "a"])));

    let undirected = MemoryGraph::<&'static str>::new(false);
    assert_eq!(cycle_path(&undirected), Err(GraphError::UndirectedTraversal));
}

#[test]
fn connected_components_groups_weakly_reachable_vertices() {
    let mut graph = MemoryGraph::<&'static str>::new(true);
    for vertex in ["a", "b", "c", "island"] {
        graph.add_vertex(vertex).expect("unique vertex");
    }
    graph.add_edge("a", "b", ()).expect("known endpoints");
    graph.add_edge("c", "b", ()).expect("known endpoints");

    assert_eq!(connected_components(&graph), Ok(vec![vec!["a", "b", "c"], vec!["island"]]));
}

#[test]
fn strongly_connected_components_split_directed_cycles() {
    let mut graph = MemoryGraph::<&'static str>::new(true);
    for vertex in ["a", "b", "c", "d"] {
        graph.add_vertex(vertex).expect("unique vertex");
    }
    graph.add_edge("a", "b", ()).expect("known endpoints");
    graph.add_edge("b", "a", ()).expect("known endpoints");
    graph.add_edge("b", "c", ()).expect("known endpoints");
    graph.add_edge("d", "c", ()).expect("known endpoints");

    assert_eq!(strongly_connected_components(&graph), Ok(vec![vec!["a", "b"], vec!["c"], vec!["d"]]));
    assert_eq!(strongly_connected_components(&CsrGraph::from_graph(&graph)).map(|mut components| {
        components.sort();
        components
    }), Ok(vec![vec![0, 1], vec![2], vec![3]]));

    let undirected = MemoryGraph::<&'static str>::new(false);
    assert_eq!(strongly_connected_components(&undirected), Err(GraphError::UndirectedTraversal));
}

#[test]
fn a_borrowed_view_feeds_the_same_algorithms() {
    use boost_collection::GraphRead;

    let names = ["a", "b", "c"];
    let mut view = GraphView::new(true);
    view.add_edge(&names[0], &names[1]);
    view.add_edge(&names[1], &names[2]);
    view.add_edge(&names[2], &names[0]);

    assert_eq!(view.vertex_count(), 3);
    assert_eq!(view.edge_count(), 3);
    assert_eq!(topological_order(&view), Err(GraphError::Cycle));
    assert_eq!(cycle_path(&view), Ok(Some(vec![&names[0], &names[1], &names[2], &names[0]])));
    assert_eq!(strongly_connected_components(&view), Ok(vec![vec![&names[0], &names[1], &names[2]]]));
}

fn weighted() -> MemoryGraph<&'static str, u64> {
    let mut graph = MemoryGraph::new(true);
    for vertex in ["a", "b", "c", "d"] {
        graph.add_vertex(vertex).expect("unique vertex");
    }
    // 直连 a->d 昂贵，绕行 a->b->c->d 更便宜。
    graph.add_edge("a", "d", 10).expect("known endpoints");
    graph.add_edge("a", "b", 1).expect("known endpoints");
    graph.add_edge("b", "c", 2).expect("known endpoints");
    graph.add_edge("c", "d", 3).expect("known endpoints");
    graph
}

#[test]
fn shortest_path_prefers_the_cheaper_route_not_the_shorter_one() {
    let graph = weighted();
    let found = shortest_path_non_negative(&graph, &"a", &"d", |edge| *graph.edge_payload(edge).expect("edge payload"))
        .expect("weights are representable");
    assert_eq!(found.distance, Some(6));
    assert_eq!(found.path, vec!["a", "b", "c", "d"]);
    assert!(found.relaxations >= 4);
    assert_eq!(found.distances[3], Some(6));
}

#[test]
fn shortest_path_reports_unreachable_and_unknown_endpoints() {
    let mut graph = weighted();
    graph.add_vertex("island").expect("unique vertex");
    let unreachable =
        shortest_path_non_negative(&graph, &"a", &"island", |edge| *graph.edge_payload(edge).expect("edge payload")).expect("weights are representable");
    assert_eq!(unreachable.distance, None);
    assert!(unreachable.path.is_empty());
    assert_eq!(
        shortest_path_non_negative(&graph, &"a", &"missing", |edge| *graph.edge_payload(edge).expect("edge payload")),
        Err(GraphError::UnknownVertex)
    );
}

#[test]
fn shortest_path_checks_accumulated_weight() {
    let mut graph = MemoryGraph::new(true);
    graph.add_vertex("a").expect("unique vertex");
    graph.add_vertex("b").expect("unique vertex");
    graph.add_vertex("c").expect("unique vertex");
    graph.add_edge("a", "b", u64::MAX).expect("known endpoints");
    graph.add_edge("b", "c", 1).expect("known endpoints");
    assert_eq!(
        shortest_path_non_negative(&graph, &"a", &"c", |edge| *graph.edge_payload(edge).expect("edge payload")),
        Err(GraphError::DistanceOverflow)
    );
}

#[test]
fn shortest_path_returns_a_typed_result() {
    let graph = weighted();
    let found: ShortestPath<&'static str> =
        shortest_path_non_negative(&graph, &"b", &"c", |edge| *graph.edge_payload(edge).expect("edge payload"))
            .expect("weights are representable");
    assert_eq!(found.distance, Some(2));
    assert_eq!(found.path, vec!["b", "c"]);
}
