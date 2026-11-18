//! 非负权最短路（共享 Dijkstra）。

use boost_collection::GraphView;
use boost_math::graph_theory::shortest_path_non_negative as shared_shortest_path;
use science_types::{Diagnostic, DiagnosticCode};

use super::{
    object::{GraphNodeId, GraphObject, MemoryGraph, WeightDomain},
    property::{GraphCertificate, GraphPropertyKind, GraphPropertyResult, GraphPropertyState},
    result::ShortestPathResult,
};

/// 非负权 Dijkstra；无权域边权按 1 计。
///
/// 算法由 `boost-math` 的共享图论核提供，本函数只负责把 `GraphObject` 投影成
/// 借用视图、映射权重域与节点身份，并按领域合同生成完整证书（距离表、
/// 前驱表、边权表与松弛次数均来自共享核的真实输出）。
pub fn shortest_path_non_negative(
    graph: &GraphObject,
    source: GraphNodeId,
    target: GraphNodeId,
) -> Result<ShortestPathResult, Diagnostic> {
    validate_weight_domain(graph)?;
    let n = graph.node_count();
    if source.0 >= n || target.0 >= n {
        return Err(Diagnostic::new(DiagnosticCode::DomainError).detail("domain", "graph_theory").detail("field", "node"));
    }

    let unit_weight = matches!(graph.semantics.weight_domain, WeightDomain::Unweighted);
    let ids: Vec<GraphNodeId> = (0..n).map(science_graph::NodeId).collect();
    let mut view = GraphView::new(true);
    for id in &ids {
        view.add_vertex(id);
    }
    for (from, to, _) in &graph.memory.edges {
        view.add_edge(&ids[from.0 as usize], &ids[to.0 as usize]);
    }

    let found = shared_shortest_path(&view, &&ids[source.0 as usize], &&ids[target.0 as usize], |edge| {
        if unit_weight { 1 } else { graph.memory.edges[edge].2 }
    })
    .map_err(|error| match error {
        boost_collection::GraphError::DistanceOverflow => {
            Diagnostic::new(DiagnosticCode::NumericResourceLimit).detail("domain", "graph_theory").detail("field", "distance")
        }
        _ => Diagnostic::new(DiagnosticCode::DomainError).detail("domain", "graph_theory").detail("field", "node"),
    })?;

    let edge_weights = collect_edge_weights(&graph.memory, unit_weight);
    let predecessors: Vec<Option<GraphNodeId>> =
        found.predecessors.iter().map(|previous| previous.as_ref().map(|id| (*id).clone())).collect();

    let Some(distance) = found.distance
    else {
        return Ok(ShortestPathResult::Unreachable {
            property: GraphPropertyResult {
                kind: GraphPropertyKind::Reachability,
                state: GraphPropertyState::ProvenFalse,
                value: (),
                certificate: Some(GraphCertificate::ShortestPathDual {
                    algorithm: "dijkstra",
                    source,
                    target,
                    dist: found.distances,
                    pred: predecessors,
                    edge_weights,
                    nonnegative_assumed: true,
                    relaxations: found.relaxations,
                }),
                strength: super::property::CertificateStrength::Summary,
                algorithm: "dijkstra",
                snapshot: graph.snapshot.clone(),
            },
        });
    };

    let path: Vec<GraphNodeId> = found.path.iter().map(|id| (*id).clone()).collect();
    Ok(ShortestPathResult::Found {
        distance,
        path,
        property: GraphPropertyResult {
            kind: GraphPropertyKind::Reachability,
            state: GraphPropertyState::ProvenTrue,
            value: distance,
            certificate: Some(GraphCertificate::ShortestPathDual {
                algorithm: "dijkstra",
                source,
                target,
                dist: found.distances,
                pred: predecessors,
                edge_weights,
                nonnegative_assumed: true,
                relaxations: found.relaxations,
            }),
            strength: super::property::CertificateStrength::Summary,
            algorithm: "dijkstra",
            snapshot: graph.snapshot.clone(),
        },
    })
}

fn collect_edge_weights(memory: &MemoryGraph, unit_weight: bool) -> Vec<(GraphNodeId, GraphNodeId, u64)> {
    memory.edges.iter().map(|(s, t, w)| (*s, *t, if unit_weight { 1 } else { *w })).collect()
}

fn validate_weight_domain(graph: &GraphObject) -> Result<(), Diagnostic> {
    match graph.semantics.weight_domain {
        WeightDomain::Unweighted | WeightDomain::NonNegativeInteger => Ok(()),
    }
}
