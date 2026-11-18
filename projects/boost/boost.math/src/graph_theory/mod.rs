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

use alloc::{
    vec,
    vec::Vec,
};
use boost_collection::{GraphError, GraphRead, GraphResult, OutgoingEdges};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
};

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

/// 按优先级选择就绪顶点的拓扑序。
///
/// 与 [`topological_order`] 的区别只在就绪集合的选择策略：每次从入度为零的
/// 顶点中取 `key` 最小的一个，而不是按入队顺序。`key` 相同则按来源的顶点
/// 顺序决定先后，因此结果仍然是确定的。调度器用它保留“每步选最小就绪任务”
/// 这类既有行为，不需要在领域里另写一份排序核。
///
/// 无向图返回 [`GraphError::UndirectedTraversal`]，存在环（含自环）返回
/// [`GraphError::Cycle`]。`key` 只在选择时调用，不读边载荷。
pub fn topological_order_by<G, K, F>(graph: &G, key: F) -> GraphResult<Vec<G::Vertex>>
where
    G: OutgoingEdges,
    F: Fn(&G::Vertex) -> K,
    K: Ord,
{
    if !graph.is_directed() {
        return Err(GraphError::UndirectedTraversal);
    }

    let vertices: Vec<&G::Vertex> = graph.vertices().collect();
    let mut index: HashMap<&G::Vertex, usize> = HashMap::with_capacity(vertices.len());
    for (position, vertex) in vertices.iter().enumerate() {
        index.insert(*vertex, position);
    }

    let mut indegree = vec![0usize; vertices.len()];
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); vertices.len()];
    for edge in graph.edges() {
        let (from, to) = graph.endpoints(edge).ok_or(GraphError::UnknownVertex)?;
        let from = *index.get(from).ok_or(GraphError::UnknownVertex)?;
        let to = *index.get(to).ok_or(GraphError::UnknownVertex)?;
        adjacency[from].push(to);
        indegree[to] += 1;
    }

    let mut ready: BinaryHeap<Reverse<(K, usize)>> = (0..vertices.len())
        .filter(|position| indegree[*position] == 0)
        .map(|position| Reverse((key(vertices[position]), position)))
        .collect();

    let mut order = Vec::with_capacity(vertices.len());
    while let Some(Reverse((_, position))) = ready.pop() {
        order.push(vertices[position].clone());
        for &next in &adjacency[position] {
            indegree[next] -= 1;
            if indegree[next] == 0 {
                ready.push(Reverse((key(vertices[next]), next)));
            }
        }
    }

    if order.len() == vertices.len() { Ok(order) } else { Err(GraphError::Cycle) }
}

/// 返回一条有向环的具体路径；无环返回 `None`。
///
/// 路径按箭头方向给出顶点序列，首尾是同一个顶点（闭合），例如
/// `a -> b -> c -> a`。它用于把“发现环”升级成可定位的诊断：调用方可以映射
/// 成领域名称或键，而不是只得到 [`GraphError::Cycle`]。使用的第一处灰色回边
/// 由来源的顶点顺序与出边顺序决定，因此同一图的结果是确定的。
///
/// 无向图返回 [`GraphError::UndirectedTraversal`]：无向环的判定需要区分父边，
/// 语义与有向依赖环不同，不能混用。
pub fn cycle_path<G>(graph: &G) -> GraphResult<Option<Vec<G::Vertex>>>
where
    G: OutgoingEdges,
{
    if !graph.is_directed() {
        return Err(GraphError::UndirectedTraversal);
    }
    let graph = IndexedGraph::new(graph)?;

    // 0 未访问，1 在当前路径上，2 已完成。
    let mut color = vec![0u8; graph.vertices.len()];
    let mut path: Vec<usize> = Vec::new();
    for start in 0..graph.vertices.len() {
        if color[start] != 0 {
            continue;
        }
        color[start] = 1;
        path.push(start);
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(node, position)) = stack.last() {
            if position < graph.forward[node].len() {
                let next = graph.forward[node][position];
                stack.last_mut().expect("a frame is being visited").1 += 1;
                match color[next] {
                    0 => {
                        color[next] = 1;
                        path.push(next);
                        stack.push((next, 0));
                    }
                    1 => {
                        let begin = path.iter().position(|vertex| *vertex == next).expect("a gray vertex is on the current path");
                        let mut cycle: Vec<G::Vertex> = path[begin..].iter().map(|vertex| graph.vertices[*vertex].clone()).collect();
                        cycle.push(graph.vertices[next].clone());
                        return Ok(Some(cycle));
                    }
                    _ => {}
                }
            }
            else {
                color[node] = 2;
                path.pop();
                stack.pop();
            }
        }
    }
    Ok(None)
}

/// 弱连通分量：忽略方向后互相可达的顶点集合。
///
/// 每个分量内的顶点按来源的顶点顺序排列，分量按其中第一个顶点在来源中的
/// 位置排序。孤点各自构成一个分量，自环与平行边不影响结果。该函数不读取
/// 边载荷。
pub fn connected_components<G>(graph: &G) -> GraphResult<Vec<Vec<G::Vertex>>>
where
    G: GraphRead,
{
    let graph = IndexedGraph::new(graph)?;
    let count = graph.vertices.len();
    let mut seen = vec![false; count];
    let mut components = Vec::new();

    for start in 0..count {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut stack = vec![start];
        let mut members = Vec::new();
        while let Some(node) = stack.pop() {
            members.push(node);
            for &next in graph.forward[node].iter().chain(graph.reverse[node].iter()) {
                if !seen[next] {
                    seen[next] = true;
                    stack.push(next);
                }
            }
        }
        members.sort_unstable();
        components.push(graph.collect(&members));
    }

    Ok(components)
}

/// 强连通分量（Kosaraju），仅有向图。
///
/// 每个分量内的顶点按来源的顶点顺序排列，分量按其中第一个顶点的位置排序。
/// 无向图返回 [`GraphError::UndirectedTraversal`]：无向图的强连通等价于连通，
/// 调用方应使用 [`connected_components`]。
pub fn strongly_connected_components<G>(graph: &G) -> GraphResult<Vec<Vec<G::Vertex>>>
where
    G: GraphRead,
{
    if !graph.is_directed() {
        return Err(GraphError::UndirectedTraversal);
    }
    let graph = IndexedGraph::new(graph)?;
    let count = graph.vertices.len();

    // 第一趟：沿出边做迭代 DFS，记录完成顺序。
    let mut order = Vec::with_capacity(count);
    let mut seen = vec![false; count];
    for start in 0..count {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut stack = vec![(start, 0usize)];
        while let Some(&(node, position)) = stack.last() {
            if position < graph.forward[node].len() {
                let next = graph.forward[node][position];
                stack.last_mut().expect("a frame is being visited").1 += 1;
                if !seen[next] {
                    seen[next] = true;
                    stack.push((next, 0));
                }
            }
            else {
                order.push(node);
                stack.pop();
            }
        }
    }

    // 第二趟：按完成顺序的逆序沿入边收集分量。
    let mut seen = vec![false; count];
    let mut components = Vec::new();
    for &finish in order.iter().rev() {
        if seen[finish] {
            continue;
        }
        seen[finish] = true;
        let mut stack = vec![finish];
        let mut members = Vec::new();
        while let Some(node) = stack.pop() {
            members.push(node);
            for &previous in &graph.reverse[node] {
                if !seen[previous] {
                    seen[previous] = true;
                    stack.push(previous);
                }
            }
        }
        members.sort_unstable();
        components.push(members);
    }

    components.sort_by_key(|members| members.first().copied().unwrap_or(usize::MAX));
    Ok(components.into_iter().map(|members| graph.collect(&members)).collect())
}

/// 从 `start` 到 `goal` 的一条非负权最短路。
///
/// 权重由调用方按边给出，类型是 `u64`：负权与 `NaN` 在该表示下不可表达，
/// 因而不需要额外的“拒绝负权”分支。累计距离用检查加法，溢出返回
/// [`GraphError::DistanceOverflow`]，不静默饱和。平行边各自参与松弛；自环
/// 不会制造更短路径。无向图的两个方向都参与松弛。
///
/// 返回 `distance` 为 `None` 表示不可达（`distances` 仍保留其余顶点的真实距离）；
/// 起止点不在图中返回 [`GraphError::UnknownVertex`]。
/// 同权路径的选择由顶点顺序与边顺序决定，结果是确定的。
pub fn shortest_path_non_negative<G, F>(
    graph: &G,
    start: &G::Vertex,
    goal: &G::Vertex,
    weight: F,
) -> GraphResult<ShortestPath<G::Vertex>>
where
    G: GraphRead,
    F: Fn(G::Edge) -> u64,
{
    let graph = IndexedGraph::weighted(graph, weight)?;
    let start = *graph.position(start).ok_or(GraphError::UnknownVertex)?;
    let goal = *graph.position(goal).ok_or(GraphError::UnknownVertex)?;

    let mut adjacency: Vec<Vec<(usize, u64)>> = vec![Vec::new(); graph.vertices.len()];
    for (edge, &(from, to)) in graph.weighted_edges.iter().enumerate() {
        adjacency[from].push((to, graph.edge_weights[edge]));
        if !graph.directed && from != to {
            adjacency[to].push((from, graph.edge_weights[edge]));
        }
    }

    let mut distance: Vec<Option<u64>> = vec![None; graph.vertices.len()];
    let mut previous = vec![None; graph.vertices.len()];
    let mut relaxations = 0_u64;
    let mut ready: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    distance[start] = Some(0);
    ready.push(Reverse((0, start)));

    while let Some(Reverse((cost, node))) = ready.pop() {
        if cost > distance[node].unwrap_or(u64::MAX) {
            continue;
        }
        if node == goal {
            break;
        }
        for &(next, edge_weight) in &adjacency[node] {
            let candidate = cost.checked_add(edge_weight).ok_or(GraphError::DistanceOverflow)?;
            relaxations += 1;
            let improves = match distance[next] {
                Some(existing) => candidate < existing,
                None => true,
            };
            if improves {
                distance[next] = Some(candidate);
                previous[next] = Some(node);
                ready.push(Reverse((candidate, next)));
            }
        }
    }

    let Some(total) = distance[goal]
    else {
        return Ok(ShortestPath {
            distance: None,
            path: Vec::new(),
            relaxations,
            distances: distance.clone(),
            predecessors: previous.iter().map(|previous| previous.map(|node| graph.vertices[node].clone())).collect(),
        });
    };

    let mut path = vec![goal];
    let mut cursor = goal;
    while cursor != start {
        let Some(parent) = previous[cursor] else {
            return Err(GraphError::UnknownVertex);
        };
        path.push(parent);
        cursor = parent;
    }
    path.reverse();

    Ok(ShortestPath {
        distance: Some(total),
        path: graph.collect(&path),
        relaxations,
        distances: distance.clone(),
        predecessors: previous.iter().map(|previous| previous.map(|node| graph.vertices[node].clone())).collect(),
    })
}

/// 非负权最短路的返回值。
///
/// `distances` 与 `predecessors` 按来源的顶点顺序排列，与本模块其它算法的
/// 顺序约定一致，供领域证书记录完整结果；不可达顶点为 `None`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortestPath<V> {
    /// 起点到终点的累计权重；不可达时为 `None`。
    pub distance: Option<u64>,
    /// 顶点序列，首元素是起点，末元素是终点；不可达时为空。
    pub path: Vec<V>,
    /// 成功松弛的边次数，供领域证书记录算法工作量。
    pub relaxations: u64,
    /// 每个顶点的最短距离，按来源顶点顺序排列。
    pub distances: Vec<Option<u64>>,
    /// 每个顶点在最短路树上的前驱，按来源顶点顺序排列。
    pub predecessors: Vec<Option<V>>,
}

/// 顶点引用加正向/反向邻接索引；算法共用的一次性投影。
struct IndexedGraph<'a, V> {
    vertices: Vec<&'a V>,
    index: HashMap<&'a V, usize>,
    forward: Vec<Vec<usize>>,
    reverse: Vec<Vec<usize>>,
    weighted_edges: Vec<(usize, usize)>,
    edge_weights: Vec<u64>,
    directed: bool,
}

impl<'a, V: Eq + core::hash::Hash> IndexedGraph<'a, V> {
    fn new<G>(graph: &'a G) -> GraphResult<Self>
    where
        G: GraphRead<Vertex = V>,
    {
        Self::build(graph, |_| 0)
    }

    fn weighted<G, F>(graph: &'a G, weight: F) -> GraphResult<Self>
    where
        G: GraphRead<Vertex = V>,
        F: Fn(G::Edge) -> u64,
    {
        Self::build(graph, weight)
    }

    fn build<G, F>(graph: &'a G, weight: F) -> GraphResult<Self>
    where
        G: GraphRead<Vertex = V>,
        F: Fn(G::Edge) -> u64,
    {
        let vertices: Vec<&'a V> = graph.vertices().collect();
        let mut index: HashMap<&'a V, usize> = HashMap::with_capacity(vertices.len());
        for (position, vertex) in vertices.iter().enumerate() {
            index.insert(vertex, position);
        }

        let mut forward = vec![Vec::new(); vertices.len()];
        let mut reverse = vec![Vec::new(); vertices.len()];
        let mut weighted_edges = Vec::with_capacity(graph.edge_count());
        let mut edge_weights = Vec::with_capacity(graph.edge_count());
        for edge in graph.edges() {
            let (from, to) = graph.endpoints(edge).ok_or(GraphError::UnknownVertex)?;
            let from = *index.get(from).ok_or(GraphError::UnknownVertex)?;
            let to = *index.get(to).ok_or(GraphError::UnknownVertex)?;
            forward[from].push(to);
            reverse[to].push(from);
            if !graph.is_directed() && from != to {
                forward[to].push(from);
                reverse[from].push(to);
            }
            weighted_edges.push((from, to));
            edge_weights.push(weight(edge));
        }

        Ok(Self { vertices, index, forward, reverse, weighted_edges, edge_weights, directed: graph.is_directed() })
    }

    fn position(&self, vertex: &V) -> Option<&usize> {
        self.index.get(vertex)
    }

    fn collect(&self, positions: &[usize]) -> Vec<V>
    where
        V: Clone,
    {
        positions.iter().map(|position| self.vertices[*position].clone()).collect()
    }
}
