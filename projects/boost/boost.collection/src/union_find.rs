//! 并查集：整数索引上的不相交集合合并与代表元查询。
//!
//! 只拥有分区本身（parent/rank 向量），不承载图的边、领域身份、持久化或
//! 失效策略。连通性判定属于 `boost.math::graph_theory` 的分量算法；这里
//! 提供的是通用结构原语，供增量合并等真实需要者使用。`find` 用迭代两趟
//! 做路径压缩，不依赖调用线程的递归栈深度。

/// 按秩合并 + 路径压缩的并查集，元素是 `0..len` 的稠密索引。
///
/// 索引越界沿用 `Vec` 的 panic 语义：分区大小在 [`UnionFind::new`] 固定，
/// 越界是调用方缺陷，不是可恢复的数据错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    /// 创建 `size` 个独立集合 `{0}, {1}, …`。
    pub fn new(size: usize) -> Self {
        Self { parent: (0..size).collect(), rank: vec![0; size] }
    }

    /// 元素个数（不是集合个数）。
    pub const fn len(&self) -> usize {
        self.parent.len()
    }

    /// 没有任何元素。
    pub const fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// 查找 `x` 所在集合的代表元，并把路径上的节点压缩到代表元。
    pub fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cursor = x;
        while self.parent[cursor] != cursor {
            let next = self.parent[cursor];
            self.parent[cursor] = root;
            cursor = next;
        }
        root
    }

    /// 合并两个集合；已在同一集合返回 `false`。
    pub fn union(&mut self, a: usize, b: usize) -> bool {
        let mut ra = self.find(a);
        let mut rb = self.find(b);
        if ra == rb {
            return false;
        }
        if self.rank[ra] < self.rank[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb] = ra;
        if self.rank[ra] == self.rank[rb] {
            self.rank[ra] = self.rank[ra].saturating_add(1);
        }
        true
    }

    /// 当前互异集合的个数。
    pub fn set_count(&mut self) -> usize {
        let count = self.parent.len();
        let mut roots = vec![false; count];
        for index in 0..count {
            roots[self.find(index)] = true;
        }
        roots.iter().filter(|&&is_root| is_root).count()
    }
}
