//! 分量扫描原语（供 `graph_theory` 包装证书；不是已证领域 API）。
//!
//! 算法本身来自 `boost-math` 的共享图论核：本模块只把 `MutableGraph` 投影为
//! 借用视图，并把结果还原成科学侧按 `NodeId` 稠密编号的分量标签。

use boost_collection::GraphView;
use boost_math::graph_theory::{
    connected_components as shared_connected_components, strongly_connected_components as shared_strongly_connected_components,
    topological_order_by,
};

use crate::{GraphDirection, GraphError, MutableGraph, NodeId};

/// DAG 拓扑排序原语；存在环时返回 [`GraphError::CycleDetected`]。
///
/// 领域完成态须经 `science-cas::graph_theory` 的 `GraphPropertyState` 包装。
pub fn topological_sort<N, E>(graph: &MutableGraph<N, E>) -> Result<Vec<NodeId>, GraphError> {
    if graph.direction() != GraphDirection::Directed {
        return Err(GraphError::UndirectedTopo);
    }
    let ids = node_ids(graph);
    let view = view_of(graph, &ids);
    topological_order_by(&view, |node| **node).map(|order| order.into_iter().map(|node| *node).collect()).map_err(|_| GraphError::CycleDetected)
}

/// 弱连通分量标签扫描（有向图按底层无向邻接解释）。
///
/// 返回每个节点的分量标签，标签为该分量中最小 `NodeId`。领域结论须经 engine 包装。
pub fn connected_components<N, E>(graph: &MutableGraph<N, E>) -> Vec<NodeId> {
    let ids = node_ids(graph);
    let view = view_of(graph, &ids);
    labels_from_components(graph.node_count(), shared_connected_components(&view).unwrap_or_default())
}

/// 强连通分量标签扫描（Kosaraju；仅有向图）。
///
/// 领域结论须经 engine 包装证书。
pub fn strongly_connected_components<N, E>(graph: &MutableGraph<N, E>) -> Result<Vec<NodeId>, GraphError> {
    if graph.direction() != GraphDirection::Directed {
        return Err(GraphError::UndirectedTopo);
    }
    let ids = node_ids(graph);
    let view = view_of(graph, &ids);
    let components = shared_strongly_connected_components(&view).map_err(|_| GraphError::UndirectedTopo)?;
    Ok(labels_from_components(graph.node_count(), components))
}

fn node_ids<N, E>(graph: &MutableGraph<N, E>) -> Vec<NodeId> {
    (0..graph.node_count()).map(NodeId).collect()
}

fn view_of<'a, N, E>(graph: &MutableGraph<N, E>, ids: &'a [NodeId]) -> GraphView<'a, NodeId> {
    let mut view = GraphView::new(graph.direction() == GraphDirection::Directed);
    for id in ids {
        view.add_vertex(id);
    }
    for (from, to, _) in graph.edges() {
        view.add_edge(&ids[from.0 as usize], &ids[to.0 as usize]);
    }
    view
}

/// 共享核按分量给出顶点列表；标签取分量内最小 `NodeId`，与既有语义一致。
fn labels_from_components(node_count: u64, components: Vec<Vec<&NodeId>>) -> Vec<NodeId> {
    let mut labels = (0..node_count).map(NodeId).collect::<Vec<_>>();
    for component in components {
        let Some(root) = component.iter().map(|node| **node).min()
        else {
            continue;
        };
        for node in component {
            labels[node.0 as usize] = root;
        }
    }
    labels
}
