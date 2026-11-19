//! G5 多表示与真实消费者验收（33 号合同）。
//!
//! 同一张逻辑图分别以 `MemoryGraph`、`CsrGraph` 与非连续业务 ID 的 `GraphView`
//! 借用视图消费 `boost.math::graph_theory` 的同一实现，覆盖合同要求的边界矩阵：
//! 空图、孤点、自环、平行边、有向/无向、环见证、优先级顺序、权重边界与快照
//! 失效。上游 `boost-math` 只保留基本冒烟，本文件是详细套件的落点。

use std::collections::BTreeSet;

use boost_collection::{CsrGraph, GraphError, GraphView, MemoryGraph};
use boost_math::graph_theory::{
    connected_components, cycle_path, reachable_from, reverse_postorder, shortest_path_non_negative, strongly_connected_components,
    topological_order, topological_order_by, topological_partition, topological_partition_by,
};

/// 非连续业务 ID：证明算法不假定顶点是紧凑 `usize`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct BizId(u32);

/// 同一逻辑图的三种表示。顶点顺序固定为声明顺序，因此下标可以互查。
struct Triple {
    names: Vec<&'static str>,
    memory: MemoryGraph<&'static str>,
    csr: CsrGraph,
    biz: Vec<BizId>,
    edges: Vec<(usize, usize)>,
}

impl Triple {
    fn new(vertices: &[&'static str], edges: &[(&'static str, &'static str)]) -> Self {
        let mut memory = MemoryGraph::new(true);
        for vertex in vertices {
            memory.add_vertex(*vertex).expect("unique vertex");
        }
        for &(from, to) in edges {
            memory.add_edge(from, to, ()).expect("known endpoints");
        }
        let csr = CsrGraph::from_graph(&memory);
        let biz: Vec<BizId> = (0..vertices.len()).map(|index| BizId(1_000 + index as u32 * 7)).collect();
        let position = |name: &str| vertices.iter().position(|candidate| *candidate == name).expect("declared vertex");
        let edges = edges.iter().map(|&(from, to)| (position(from), position(to))).collect();
        Self { names: vertices.to_vec(), memory, csr, biz, edges }
    }

    fn view(&self) -> GraphView<'_, BizId> {
        let mut view = GraphView::new(true);
        for id in &self.biz {
            view.add_vertex(id);
        }
        for &(from, to) in &self.edges {
            view.add_edge(&self.biz[from], &self.biz[to]);
        }
        view
    }

    fn position(&self, id: &BizId) -> usize {
        self.biz.iter().position(|candidate| candidate == id).expect("view vertices come from `biz`")
    }

    fn name(&self, index: usize) -> &'static str {
        self.names[index]
    }

    /// 拓扑序在三种表示上逐位一致（映射回名字后比较）。
    fn assert_topo_agreement(&self) {
        let memory = topological_order(&self.memory);
        let csr = topological_order(&self.csr).map(|order| order.iter().map(|&index| self.name(index)).collect::<Vec<_>>());
        let view = topological_order(&self.view()).map(|order| order.iter().map(|id| self.name(self.position(id))).collect::<Vec<_>>());
        assert_eq!(memory, csr, "MemoryGraph 与 CSR 拓扑序必须一致");
        assert_eq!(memory, view, "借用视图与拥有图拓扑序必须一致");
    }
}

#[test]
fn empty_graph_and_isolated_vertices_across_representations() {
    let empty = MemoryGraph::<&'static str>::new(true);
    assert_eq!(topological_order(&empty), Ok(Vec::new()));
    assert_eq!(connected_components(&empty).expect("weak components"), Vec::<Vec<&str>>::new());
    assert_eq!(cycle_path(&empty).expect("directed"), None);
    let empty_csr = CsrGraph::from_graph(&empty);
    assert_eq!(topological_order(&empty_csr), Ok(Vec::new()));

    let triple = Triple::new(&["solo", "island"], &[]);
    triple.assert_topo_agreement();
    assert_eq!(topological_order(&triple.memory), Ok(vec!["solo", "island"]));
    assert_eq!(connected_components(&triple.memory).expect("weak"), vec![vec!["solo"], vec!["island"]]);
    let view_scc: Vec<usize> =
        strongly_connected_components(&triple.view()).expect("directed").iter().map(|component| triple.position(component[0])).collect();
    assert_eq!(view_scc, vec![0, 1], "孤点各自成分量，按首成员来源顺序");
}

#[test]
fn self_loops_and_parallel_edges_are_independent_edges() {
    let mut graph = MemoryGraph::<&'static str>::new(true);
    graph.add_vertex("a").expect("vertex");
    graph.add_edge("a", "a", ()).expect("loop");
    assert_eq!(topological_order(&graph), Err(GraphError::Cycle));
    assert_eq!(cycle_path(&graph).expect("directed"), Some(vec!["a", "a"]), "自环的见证是闭合两步");
    assert_eq!(connected_components(&graph).expect("weak"), vec![vec!["a"]]);

    // 平行边按独立边计数：拓扑与可达不重复，SCC 不受影响。
    let mut parallel = MemoryGraph::<&'static str>::new(true);
    parallel.add_vertex("a").expect("vertex");
    parallel.add_vertex("b").expect("vertex");
    parallel.add_edge("a", "b", ()).expect("edge");
    parallel.add_edge("a", "b", ()).expect("edge");
    assert_eq!(topological_order(&parallel), Ok(vec!["a", "b"]));
    assert_eq!(reachable_from(&parallel, &"a").expect("start"), vec!["a", "b"]);
    assert_eq!(strongly_connected_components(&parallel).expect("directed"), vec![vec!["a"], vec!["b"]]);
    let csr = CsrGraph::from_graph(&parallel);
    assert_eq!(topological_order(&csr), Ok(vec![0, 1]), "CSR 顶点顺序沿用来源 a=0, b=1");
}

#[test]
fn undirected_graphs_reject_directional_algorithms_only() {
    let mut graph = MemoryGraph::<&'static str>::new(false);
    graph.add_vertex("a").expect("vertex");
    graph.add_vertex("b").expect("vertex");
    graph.add_edge("a", "b", ()).expect("edge");
    assert_eq!(topological_order(&graph), Err(GraphError::UndirectedTraversal));
    assert_eq!(topological_partition(&graph), Err(GraphError::UndirectedTraversal));
    assert_eq!(topological_order_by(&graph, |vertex| *vertex), Err(GraphError::UndirectedTraversal));
    assert_eq!(topological_partition_by(&graph, |vertex| *vertex), Err(GraphError::UndirectedTraversal));
    assert_eq!(cycle_path(&graph), Err(GraphError::UndirectedTraversal));
    assert_eq!(strongly_connected_components(&graph), Err(GraphError::UndirectedTraversal));
    assert_eq!(reachable_from(&graph, &"b").expect("start"), vec!["b", "a"]);
    assert_eq!(connected_components(&graph).expect("weak"), vec![vec!["a", "b"]]);
    assert_eq!(reverse_postorder(&graph, &"a").expect("start"), vec!["a", "b"], "RPO 只依赖出边能力");
}

#[test]
fn cycle_witness_is_a_closed_directed_path_in_all_representations() {
    let triple = Triple::new(&["a", "b", "c", "d"], &[("a", "b"), ("b", "c"), ("c", "a")]);
    let witness = cycle_path(&triple.memory).expect("directed").expect("cycle exists");
    assert_eq!(&witness[0], &witness[witness.len() - 1], "见证必须闭合");
    assert_eq!(witness, vec!["a", "b", "c", "a"]);

    let csr_witness = cycle_path(&triple.csr).expect("directed").expect("cycle exists");
    assert_eq!(csr_witness.iter().map(|&index| triple.name(index)).collect::<Vec<_>>(), witness, "CSR 见证与拥有图同一条环");
    let view_witness = cycle_path(&triple.view()).expect("directed").expect("cycle exists");
    assert_eq!(view_witness.iter().map(|id| triple.name(triple.position(id))).collect::<Vec<_>>(), witness, "借用视图见证同一条环");

    let (ordered, remaining) = topological_partition(&triple.memory).expect("directed");
    assert_eq!(ordered, vec!["d"], "零入度的无关点仍可出队");
    assert_eq!(remaining, vec!["a", "b", "c"], "残差按来源顺序");
    triple.assert_topo_agreement();
}

#[test]
fn priority_selection_orders_ready_vertices_by_key_then_source() {
    let triple = Triple::new(&["hi", "lo", "mid"], &[]);
    let order = topological_order_by(&triple.memory, |vertex| match *vertex {
        "lo" => 0,
        "mid" => 1,
        _ => 2,
    })
    .expect("acyclic");
    assert_eq!(order, vec!["lo", "mid", "hi"]);

    // 借用视图 + 优先级分区：与拥有图同序；键相同则按来源顺序。
    let key = |id: &&BizId| match triple.position(id) {
        1 => 0u8,
        2 => 1,
        _ => 2,
    };
    let (view_order, remaining) = topological_partition_by(&triple.view(), key).expect("acyclic");
    assert_eq!(view_order.iter().map(|id| triple.name(triple.position(id))).collect::<Vec<_>>(), vec!["lo", "mid", "hi"]);
    assert!(remaining.is_empty());
}

#[test]
fn weight_boundaries_unreachable_and_overflow() {
    // 零权、平行边逐边取权、不可达与溢出。
    let mut graph = MemoryGraph::<&'static str, u64>::new(true);
    for vertex in ["a", "b", "c", "island"] {
        graph.add_vertex(vertex).expect("vertex");
    }
    graph.add_edge("a", "b", 0u64).expect("zero weight");
    graph.add_edge("b", "c", 5u64).expect("edge");
    graph.add_edge("b", "c", 2u64).expect("parallel cheaper edge");
    let weight = |edge: usize| *graph.edge_payload(edge).expect("payload");
    let path = shortest_path_non_negative(&graph, &"a", &"c", weight).expect("representable");
    assert_eq!(path.distance, Some(2), "平行边必须逐边取权，走更小的 0+2");
    assert_eq!(path.path, vec!["a", "b", "c"]);
    let unreachable = shortest_path_non_negative(&graph, &"a", &"island", weight).expect("representable");
    assert_eq!(unreachable.distance, None);
    assert!(unreachable.path.is_empty());
    assert_eq!(unreachable.distances.iter().filter(|distance| distance.is_none()).count(), 1, "只有 island 不可达");

    let mut overflow = MemoryGraph::<&'static str, u64>::new(true);
    overflow.add_vertex("a").expect("vertex");
    overflow.add_vertex("b").expect("vertex");
    overflow.add_vertex("c").expect("vertex");
    overflow.add_edge("a", "b", u64::MAX).expect("edge");
    overflow.add_edge("b", "c", 1u64).expect("edge");
    assert_eq!(
        shortest_path_non_negative(&overflow, &"a", &"c", |edge| *overflow.edge_payload(edge).expect("payload")),
        Err(GraphError::DistanceOverflow),
        "累计距离用检查加法，不静默饱和"
    );

    let mut undirected = MemoryGraph::<&'static str, u64>::new(false);
    undirected.add_vertex("a").expect("vertex");
    undirected.add_vertex("b").expect("vertex");
    undirected.add_edge("a", "b", 3u64).expect("edge");
    let found = shortest_path_non_negative(&undirected, &"b", &"a", |edge| *undirected.edge_payload(edge).expect("payload")).expect("ok");
    assert_eq!(found.distance, Some(3), "无向图两个方向都参与松弛");
}

#[test]
fn csr_snapshot_does_not_observe_later_mutation_of_the_source_graph() {
    let mut graph = MemoryGraph::<&'static str>::new(true);
    graph.add_vertex("a").expect("vertex");
    graph.add_vertex("b").expect("vertex");
    graph.add_edge("a", "b", ()).expect("edge");
    let snapshot = CsrGraph::from_graph(&graph);
    assert_eq!(topological_order(&snapshot), Ok(vec![0, 1]));

    graph.add_vertex("c").expect("vertex");
    graph.add_edge("b", "c", ()).expect("edge");
    graph.add_edge("c", "a", ()).expect("edge");
    assert_eq!(topological_order(&graph), Err(GraphError::Cycle), "拥有图已成环");
    assert_eq!(topological_order(&snapshot), Ok(vec![0, 1]), "快照保持冻结时刻的 DAG");
    assert_eq!(cycle_path(&snapshot).expect("directed"), None);
}

#[test]
fn rpo_and_reachability_agree_between_owned_and_borrowed_representations() {
    // 真实消费者形态：菱形 + 回边（CFG 形状）。
    let triple = Triple::new(
        &["entry", "left", "right", "join"],
        &[("entry", "left"), ("entry", "right"), ("left", "join"), ("right", "join"), ("join", "left")],
    );
    let memory_rpo = reverse_postorder(&triple.memory, &"entry").expect("entry exists");
    let csr_rpo = reverse_postorder(&triple.csr, &0).expect("entry index");
    let view_rpo = reverse_postorder(&triple.view(), &&triple.biz[0]).expect("entry id");
    assert_eq!(memory_rpo[0], "entry", "RPO 起点在首位");
    assert_eq!(csr_rpo.iter().map(|&index| triple.name(index)).collect::<Vec<_>>(), memory_rpo, "CSR 与拥有图 RPO 逐位一致");
    assert_eq!(view_rpo.iter().map(|id| triple.name(triple.position(id))).collect::<Vec<_>>(), memory_rpo, "借用视图与拥有图 RPO 逐位一致");

    let reached: BTreeSet<&str> = reachable_from(&triple.memory, &"entry").expect("ok").into_iter().collect();
    assert_eq!(reached, ["entry", "left", "right", "join"].into_iter().collect());

    // 回边只困住 `left` 与 `join`：`entry` 零入度先出，`right` 随即可出。
    let (ordered, remaining) = topological_partition(&triple.memory).expect("directed");
    assert_eq!(ordered, vec!["entry", "right"]);
    assert_eq!(remaining, vec!["left", "join"], "残差按来源顺序，含环上点与其下游");
}
