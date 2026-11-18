//! 内存图 L0 原语合同测试（经 `primitives`，非领域 claim）。

use science_graph::{
    GraphBuilder, GraphDirection, GraphSemantics, ImmutableGraph, bfs_order,
    primitives::{connected_components, topological_sort},
};

fn build(direction: GraphDirection, f: impl FnOnce(&mut science_graph::MutableGraph<(), ()>)) -> ImmutableGraph<(), ()> {
    let mut b = GraphBuilder::<(), ()>::new(GraphSemantics::from_direction(direction));
    f(b.graph_mut());
    b.finish()
}

#[test]
fn bfs_visits_reachable_nodes() {
    let g = build(GraphDirection::Directed, |g| {
        let a = g.add_node(());
        let b = g.add_node(());
        let c = g.add_node(());
        g.add_edge(a, b, ());
        g.add_edge(b, c, ());
    });
    let a = science_graph::NodeId(0);
    let order = bfs_order(&g, a).unwrap();
    assert_eq!(order, vec![science_graph::NodeId(0), science_graph::NodeId(1), science_graph::NodeId(2)]);
}

#[test]
fn topo_sort_detects_cycle() {
    let g = build(GraphDirection::Directed, |g| {
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, ());
        g.add_edge(b, a, ());
    });
    assert!(topological_sort(&g).is_err());
}

#[test]
fn topo_sort_orders_dag() {
    let g = build(GraphDirection::Directed, |g| {
        let a = g.add_node(());
        let b = g.add_node(());
        let c = g.add_node(());
        g.add_edge(a, b, ());
        g.add_edge(a, c, ());
        g.add_edge(b, c, ());
    });
    let order = topological_sort(&g).unwrap();
    assert_eq!(order[0], science_graph::NodeId(0));
    assert_eq!(*order.last().unwrap(), science_graph::NodeId(2));
}

#[test]
fn undirected_topo_rejected() {
    let g = GraphBuilder::<(), ()>::from_direction(GraphDirection::Undirected).finish();
    assert!(matches!(topological_sort(&g), Err(science_graph::GraphError::UndirectedTopo)));
}

#[test]
fn connected_components_labels_undirected() {
    let g = build(GraphDirection::Undirected, |g| {
        let a = g.add_node(());
        let b = g.add_node(());
        let _c = g.add_node(());
        g.add_edge(a, b, ());
    });
    let labels = connected_components(&g);
    assert_eq!(labels[0], science_graph::NodeId(0));
    assert_eq!(labels[1], science_graph::NodeId(0));
    assert_eq!(labels[2], science_graph::NodeId(2));
}
