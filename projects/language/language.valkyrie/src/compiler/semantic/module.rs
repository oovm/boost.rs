//! 模块系统核心：依赖图与模块解析错误类型。
//!
//! 本模块提供 `DependencyGraph` 用于管理模块间的依赖关系，
//! 支持循环依赖检测和拓扑排序，为构建系统提供编译顺序决策。

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

use boost_collection::GraphView;
use boost_math::graph_theory::{cycle_path, topological_order_by};
use compiler_types::QualifiedName;

/// 模块解析错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleError {
    /// 找不到指定模块。
    NotFound {
        /// 缺失模块的限定名。
        name: QualifiedName,
        /// 已搜索的路径列表。
        searched: Vec<PathBuf>,
    },
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleError::NotFound { name, searched } => {
                write!(f, "模块 {} 未找到，已搜索路径：", name)?;
                for path in searched {
                    write!(f, "\n  - {}", path.display())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ModuleError {}

/// 拓扑排序错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologicalSortError {
    /// 检测到的循环路径。
    pub cycle: Vec<QualifiedName>,
}

impl fmt::Display for TopologicalSortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "检测到循环依赖：")?;
        for (i, name) in self.cycle.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "{}", name)?;
        }
        Ok(())
    }
}

impl std::error::Error for TopologicalSortError {}

/// 模块依赖图。
///
/// 管理模块间的依赖关系，支持：
/// - 添加模块节点和依赖边
/// - 检测循环依赖
/// - 拓扑排序（依赖在前，被依赖在后）
/// - 查询直接依赖和反向依赖
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    /// 邻接表：模块 -> 其依赖的模块集合。
    /// `edges[a]` 包含 `b` 表示 `a` 依赖 `b`。
    edges: BTreeMap<QualifiedName, BTreeSet<QualifiedName>>,
    /// 反向邻接表：模块 -> 依赖它的模块集合。
    /// `reverse_edges[a]` 包含 `b` 表示 `b` 依赖 `a`。
    reverse_edges: BTreeMap<QualifiedName, BTreeSet<QualifiedName>>,
}

impl DependencyGraph {
    /// 创建空的依赖图。
    pub fn new() -> Self {
        Self::default()
    }

    /// 判断图是否为空。
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// 返回图中的模块数量。
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// 判断模块是否存在于图中。
    pub fn contains(&self, name: &QualifiedName) -> bool {
        self.edges.contains_key(name)
    }

    /// 添加一个模块节点。
    ///
    /// 如果模块已存在，不做任何操作。
    pub fn add_module(&mut self, name: QualifiedName) {
        self.edges.entry(name.clone()).or_default();
        self.reverse_edges.entry(name).or_default();
    }

    /// 添加一条依赖边：`from` 依赖 `to`。
    ///
    /// 如果模块不存在，会自动添加。
    pub fn add_dependency(&mut self, from: QualifiedName, to: QualifiedName) {
        self.add_module(from.clone());
        self.add_module(to.clone());
        self.edges.get_mut(&from).unwrap().insert(to.clone());
        self.reverse_edges.get_mut(&to).unwrap().insert(from);
    }

    /// 返回模块的直接依赖列表。
    ///
    /// 如果模块不存在，返回 `None`。
    pub fn dependencies(&self, name: &QualifiedName) -> Option<Vec<QualifiedName>> {
        self.edges.get(name).map(|deps| deps.iter().cloned().collect())
    }

    /// 返回依赖该模块的模块列表。
    ///
    /// 如果模块不存在，返回空列表。
    pub fn dependents(&self, name: &QualifiedName) -> Vec<QualifiedName> {
        self.reverse_edges.get(name).map(|deps| deps.iter().cloned().collect()).unwrap_or_default()
    }

    /// 移除一个模块及其所有关联边。
    ///
    /// 返回模块是否曾存在。
    pub fn remove_module(&mut self, name: &QualifiedName) -> bool {
        let existed = self.edges.remove(name).is_some();

        // 从所有依赖该模块的节点中移除这条边
        if let Some(rev_deps) = self.reverse_edges.remove(name) {
            for rev_dep in rev_deps {
                if let Some(deps) = self.edges.get_mut(&rev_dep) {
                    deps.remove(name);
                }
            }
        }

        // 从该模块的依赖中移除反向边
        if let Some(deps) = self.edges.get(name) {
            for dep in deps {
                if let Some(rev) = self.reverse_edges.get_mut(dep) {
                    rev.remove(name);
                }
            }
        }

        existed
    }

    /// 清空图中的所有模块和边。
    pub fn clear(&mut self) {
        self.edges.clear();
        self.reverse_edges.clear();
    }

    /// 检测图中是否存在循环依赖。
    ///
    /// 如果存在循环，返回循环路径；否则返回 `None`。路径首尾是同一个模块，
    /// 由共享图论核给出，本方法只把键映射回限定名。
    pub fn detect_cycle(&self) -> Option<Vec<QualifiedName>> {
        let view = self.view();
        match cycle_path(&view) {
            Ok(Some(cycle)) => Some(cycle.into_iter().cloned().collect()),
            // 视图固定为有向图且顶点来自同一张邻接表，失败只可能是实现错误。
            Ok(None) | Err(_) => None,
        }
    }

    /// 对图进行拓扑排序。
    ///
    /// 返回依赖在前、被依赖在后的顺序。如果存在循环依赖，返回错误。
    /// 就绪集合按限定名有序选择，因此同一张图的结果是确定的。
    pub fn topological_sort(&self) -> Result<Vec<QualifiedName>, TopologicalSortError> {
        let view = self.view();
        match topological_order_by(&view, |name| (*name).clone()) {
            Ok(order) => Ok(order.into_iter().cloned().collect()),
            Err(_) => Err(TopologicalSortError { cycle: self.detect_cycle().unwrap_or_default() }),
        }
    }

    /// 把邻接表投影成借用视图；只在调用期间有效，不复制模块名。
    fn view(&self) -> GraphView<'_, QualifiedName> {
        let mut view = GraphView::new(true);
        for (name, dependencies) in &self.edges {
            view.add_vertex(name);
            for dependency in dependencies {
                view.add_edge(name, dependency);
            }
        }
        view
    }
}
