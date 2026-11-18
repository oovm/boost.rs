use core::{
    fmt::{self, Display, Formatter},
    hash::Hash,
};
use std::collections::HashMap;

/// 图读取能力：顶点、边、端点与方向。
///
/// 描述符属于产生它的图快照，有效期由借用约束表达；键不是连续整数，也不
/// 假定所有图都维护双向索引。入边、结构修改与属性访问按需要单独增加能力，
/// 不塞进同一个 trait。
pub trait GraphRead {
    /// 顶点标识。
    type Vertex: Clone + Eq + Hash;
    /// 边标识；同一图内有效，平行边各自拥有独立标识。
    type Edge: Copy + Eq;

    /// 是否有向。无向边是一条逻辑边，双向邻接项只是表示。
    fn is_directed(&self) -> bool;
    /// 顶点数。
    fn vertex_count(&self) -> usize;
    /// 边数；平行边逐条计数。
    fn edge_count(&self) -> usize;
    /// 按实现声明的稳定顺序枚举顶点。
    fn vertices(&self) -> impl Iterator<Item = &Self::Vertex>;
    /// 按实现声明的稳定顺序枚举边。
    fn edges(&self) -> impl Iterator<Item = Self::Edge>;
    /// 边两端；标识不属于本图时返回 `None`。
    fn endpoints(&self, edge: Self::Edge) -> Option<(&Self::Vertex, &Self::Vertex)>;
}

/// 出边能力；只需要出边的算法不必要求反向索引。
pub trait OutgoingEdges: GraphRead {
    /// 顶点关联的出边；未知顶点返回空迭代器。无向图中两个方向都是出边。
    fn outgoing(&self, vertex: &Self::Vertex) -> impl Iterator<Item = Self::Edge>;
}

/// 入边能力；只有实际维护反向索引的表示实现它。
///
/// 需要双向邻接的算法显式要求本能力，其余图不必为它付出索引成本。
/// 无向图没有方向意义上的入边，实现返回空迭代器；只读 CSR 不实现本能力，
/// 需要入边时应显式物化反向 CSR。
pub trait IncomingEdges: GraphRead {
    /// 顶点关联的入边；未知顶点返回空迭代器。
    fn incoming(&self, vertex: &Self::Vertex) -> impl Iterator<Item = Self::Edge>;
}

/// 图构造与遍历的失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphError {
    /// 顶点标识重复。
    DuplicateVertex,
    /// 边的端点不在图中。
    UnknownVertex,
    /// 无向图不支持只对方向定义的遍历。
    UndirectedTraversal,
    /// 拓扑排序遇到环，包括自环。
    Cycle,
}

impl Display for GraphError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateVertex => f.write_str("the vertex identifier is already present"),
            Self::UnknownVertex => f.write_str("the vertex identifier is not part of this graph"),
            Self::UndirectedTraversal => f.write_str("the traversal requires a directed graph"),
            Self::Cycle => f.write_str("the graph contains a cycle"),
        }
    }
}

impl std::error::Error for GraphError {}

/// 图操作的统一结果别名。
pub type GraphResult<T> = core::result::Result<T, GraphError>;

/// 一条内存中的边。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryEdge<K, E> {
    from: K,
    to: K,
    payload: E,
}

impl<K, E> MemoryEdge<K, E> {
    /// 起点。
    #[must_use]
    pub const fn from(&self) -> &K {
        &self.from
    }

    /// 终点。
    #[must_use]
    pub const fn to(&self) -> &K {
        &self.to
    }

    /// 边载荷，权重等业务属性由调用方决定。
    #[must_use]
    pub const fn payload(&self) -> &E {
        &self.payload
    }
}

/// 首个内存图实现：不可变结构 + 出边索引。
///
/// 顶点身份是调用方给出的键，不要求连续整数；平行边各自保留，自环允许
/// 但会在拓扑排序中构成环。无向图的每条边只计一次，两个方向都是出边；
/// 无向自环在出边枚举中出现一次。删除、失效标识与可变结构尚未提供。
#[derive(Clone, Debug)]
pub struct MemoryGraph<K, E = ()> {
    directed: bool,
    vertices: Vec<K>,
    edges: Vec<MemoryEdge<K, E>>,
    outgoing: HashMap<K, Vec<usize>>,
    incoming: Option<HashMap<K, Vec<usize>>>,
}

impl<K: Clone + Eq + Hash, E> MemoryGraph<K, E> {
    /// 建一个空图；有向图同时维护入边索引，无向图不建。
    #[must_use]
    pub fn new(directed: bool) -> Self {
        Self { directed, vertices: Vec::new(), edges: Vec::new(), outgoing: HashMap::new(), incoming: directed.then(HashMap::new) }
    }

    /// 添加顶点；重复标识返回 [`GraphError::DuplicateVertex`]。
    pub fn add_vertex(&mut self, vertex: K) -> GraphResult<()> {
        if self.outgoing.contains_key(&vertex) {
            return Err(GraphError::DuplicateVertex);
        }
        self.outgoing.insert(vertex.clone(), Vec::new());
        if let Some(incoming) = &mut self.incoming {
            incoming.insert(vertex.clone(), Vec::new());
        }
        self.vertices.push(vertex);
        Ok(())
    }

    /// 添加边并返回边标识；端点必须已存在。
    ///
    /// 平行边逐条保留，不去重；无向图的边只存一条逻辑边。
    pub fn add_edge(&mut self, from: K, to: K, payload: E) -> GraphResult<usize> {
        if !self.outgoing.contains_key(&from) || !self.outgoing.contains_key(&to) {
            return Err(GraphError::UnknownVertex);
        }
        let edge = self.edges.len();
        self.outgoing.get_mut(&from).expect("the source was checked").push(edge);
        if let Some(incoming) = &mut self.incoming {
            incoming.get_mut(&to).expect("the target was checked").push(edge);
        }
        if !self.directed && from != to {
            self.outgoing.get_mut(&to).expect("the target was checked").push(edge);
        }
        self.edges.push(MemoryEdge { from, to, payload });
        Ok(edge)
    }

    /// 边载荷；标识越界返回 `None`。
    #[must_use]
    pub fn edge_payload(&self, edge: usize) -> Option<&E> {
        self.edges.get(edge).map(MemoryEdge::payload)
    }
}

impl<K: Clone + Eq + Hash, E> GraphRead for MemoryGraph<K, E> {
    type Vertex = K;
    type Edge = usize;

    fn is_directed(&self) -> bool {
        self.directed
    }

    fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    fn edge_count(&self) -> usize {
        self.edges.len()
    }

    fn vertices(&self) -> impl Iterator<Item = &Self::Vertex> {
        self.vertices.iter()
    }

    fn edges(&self) -> impl Iterator<Item = Self::Edge> {
        0..self.edges.len()
    }

    fn endpoints(&self, edge: Self::Edge) -> Option<(&Self::Vertex, &Self::Vertex)> {
        self.edges.get(edge).map(|edge| (&edge.from, &edge.to))
    }
}

impl<K: Clone + Eq + Hash, E> OutgoingEdges for MemoryGraph<K, E> {
    fn outgoing(&self, vertex: &Self::Vertex) -> impl Iterator<Item = Self::Edge> {
        self.outgoing.get(vertex).into_iter().flatten().copied()
    }
}

impl<K: Clone + Eq + Hash, E> IncomingEdges for MemoryGraph<K, E> {
    fn incoming(&self, vertex: &Self::Vertex) -> impl Iterator<Item = Self::Edge> {
        self.incoming.as_ref().and_then(|incoming| incoming.get(vertex)).into_iter().flatten().copied()
    }
}

/// 只读 CSR 表示：顶点编号为稠密索引，出边按压缩行存储。
///
/// 它在构建时完成一次映射与重排；之后不支持插边、删除或载荷修改。
/// 平行边与自环按原图保留，无向边在两个方向都可枚举。
#[derive(Clone, Debug)]
pub struct CsrGraph {
    directed: bool,
    vertices: Vec<usize>,
    offsets: Vec<usize>,
    targets: Vec<usize>,
    edge_ids: Vec<usize>,
    from: Vec<usize>,
    to: Vec<usize>,
}

impl CsrGraph {
    /// 从任意声明了顶点、边与端点的图物化为 CSR。
    ///
    /// 顶点顺序沿用来源的迭代顺序；平行边保留为独立边标识。
    #[must_use]
    pub fn from_graph<G: GraphRead>(graph: &G) -> Self {
        let mut index: HashMap<&G::Vertex, usize> = HashMap::with_capacity(graph.vertex_count());
        for (position, vertex) in graph.vertices().enumerate() {
            index.insert(vertex, position);
        }
        let mut from = Vec::with_capacity(graph.edge_count());
        let mut to = Vec::with_capacity(graph.edge_count());
        for edge in graph.edges() {
            let (source, target) = graph.endpoints(edge).expect("edge descriptors come from the same graph");
            from.push(index[source]);
            to.push(index[target]);
        }

        let mut lists: Vec<Vec<(usize, usize)>> = vec![Vec::new(); graph.vertex_count()];
        for (edge, (&source, &target)) in from.iter().zip(&to).enumerate() {
            lists[source].push((edge, target));
            if !graph.is_directed() && source != target {
                lists[target].push((edge, source));
            }
        }

        let mut offsets = Vec::with_capacity(lists.len() + 1);
        let mut targets = Vec::new();
        let mut edge_ids = Vec::new();
        offsets.push(0);
        for list in lists {
            for (edge, target) in list {
                targets.push(target);
                edge_ids.push(edge);
            }
            offsets.push(targets.len());
        }

        Self { directed: graph.is_directed(), vertices: (0..graph.vertex_count()).collect(), offsets, targets, edge_ids, from, to }
    }
}

impl GraphRead for CsrGraph {
    type Vertex = usize;
    type Edge = usize;

    fn is_directed(&self) -> bool {
        self.directed
    }

    fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    fn edge_count(&self) -> usize {
        self.from.len()
    }

    fn vertices(&self) -> impl Iterator<Item = &Self::Vertex> {
        self.vertices.iter()
    }

    fn edges(&self) -> impl Iterator<Item = Self::Edge> {
        0..self.from.len()
    }

    fn endpoints(&self, edge: Self::Edge) -> Option<(&Self::Vertex, &Self::Vertex)> {
        Some((self.from.get(edge)?, self.to.get(edge)?))
    }
}

impl OutgoingEdges for CsrGraph {
    fn outgoing(&self, vertex: &Self::Vertex) -> impl Iterator<Item = Self::Edge> {
        let start = self.offsets.get(*vertex).copied().unwrap_or(0);
        let end = vertex.checked_add(1).and_then(|next| self.offsets.get(next)).copied().unwrap_or(start);
        let end = end.min(self.edge_ids.len());
        self.edge_ids[start.min(end)..end].iter().copied()
    }
}
