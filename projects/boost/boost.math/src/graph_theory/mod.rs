//! 消费图访问能力的图论算法。
//!
//! 顶点/边身份、方向、平行边与存储布局归 `boost-collection`；本模块只有
//! 图论定义与算法，只要求 `GraphRead` / `OutgoingEdges` 这类访问能力，因此
//! 内存图与只读 CSR 共用同一份实现，不绑定具体容器。
//!
//! 本模块当前只提供**无权**算法：不读取边载荷、不做权重加法或路径累加，
//! 因而不存在负权、权重溢出或权重不可比较的问题，也不存在加权最短路意义下
//! 的“不可达”结果。平行边按独立边逐条遍历，自环构成环。算法各自持有临时
//! 工作区（入度表、已访问集合），不写回图容器。

use alloc::vec::Vec;
use boost_collection::{GraphError, GraphResult, OutgoingEdges};
use std::collections::{HashMap, HashSet, VecDeque};

/// 有向图的拓扑序。
///
/// 只要求出边与端点能力，因此内存图与 CSR 表示共用同一实现。零入度顶点
/// 按来源的顶点顺序入队，结果是确定性的；空图返回空序，遇到环（含自环）
/// 返回 [`GraphError::Cycle`]，两者不能混淆。无向图返回
/// [`GraphError::UndirectedTraversal`]，因为拓扑序只对方向有意义。
///
/// 该函数不读取边载荷，因此不涉及权重、负权或权重溢出；拓扑序成立的前提是
/// 图确实有向且无环，调用方不应把 `Cycle` 当作可忽略的告警。
pub fn topological_order<G>(graph: &G) -> GraphResult<Vec<G::Vertex>>
where
    G: OutgoingEdges,
{
    if !graph.is_directed() {
        return Err(GraphError::UndirectedTraversal);
    }
    let mut indegree: HashMap<&G::Vertex, usize> = HashMap::with_capacity(graph.vertex_count());
    for vertex in graph.vertices() {
        indegree.insert(vertex, 0);
    }
    for edge in graph.edges() {
        let (_, target) = graph.endpoints(edge).ok_or(GraphError::UnknownVertex)?;
        *indegree.get_mut(target).ok_or(GraphError::UnknownVertex)? += 1;
    }

    let mut ready: VecDeque<&G::Vertex> = graph.vertices().filter(|vertex| indegree.get(*vertex) == Some(&0)).collect();
    let mut order = Vec::with_capacity(graph.vertex_count());
    while let Some(vertex) = ready.pop_front() {
        order.push(vertex.clone());
        for edge in graph.outgoing(vertex) {
            let (_, target) = graph.endpoints(edge).ok_or(GraphError::UnknownVertex)?;
            let remaining = indegree.get_mut(target).ok_or(GraphError::UnknownVertex)?;
            *remaining -= 1;
            if *remaining == 0 {
                ready.push_back(target);
            }
        }
    }

    if order.len() == graph.vertex_count() { Ok(order) } else { Err(GraphError::Cycle) }
}

/// 从 `start` 出发沿出边可达的顶点。
///
/// 只要求出边与端点能力，因此内存图与 CSR 表示共用同一实现。返回序列是
/// 广度优先的访问顺序：`start` 在首位，其余顶点按来源的顶点顺序与各顶点的
/// 出边顺序展开，结果是确定性的。每个顶点最多出现一次，因此环与自环不会
/// 导致重复或死循环；不可达顶点不出现在结果里，而不是报错。`start` 不是
/// 本图的顶点时返回 [`GraphError::UnknownVertex`]。
///
/// 有向图只沿箭头方向展开；无向图的出边能力在两个方向都可枚举，所以结果
/// 恰好是 `start` 所在的连通分量。该函数不读取边载荷，因此不含权重、
/// 负权与权重溢出语义；需要加权路径时另用带 `Weight` 条件的算法。
pub fn reachable_from<G>(graph: &G, start: &G::Vertex) -> GraphResult<Vec<G::Vertex>>
where
    G: OutgoingEdges,
{
    if !graph.vertices().any(|vertex| vertex == start) {
        return Err(GraphError::UnknownVertex);
    }

    let mut visited: HashSet<&G::Vertex> = HashSet::with_capacity(graph.vertex_count());
    let mut queue: VecDeque<&G::Vertex> = VecDeque::new();
    let mut reached = Vec::with_capacity(graph.vertex_count());

    visited.insert(start);
    queue.push_back(start);
    reached.push(start.clone());

    while let Some(vertex) = queue.pop_front() {
        for edge in graph.outgoing(vertex) {
            let (_, target) = graph.endpoints(edge).ok_or(GraphError::UnknownVertex)?;
            if visited.insert(target) {
                queue.push_back(target);
                reached.push(target.clone());
            }
        }
    }

    Ok(reached)
}
