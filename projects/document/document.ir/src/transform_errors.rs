//! 语义变换的错误类型与返回值。
//!
//! 本模块为 DocumentTransform infrastructure 的基础枚举：
//! - [`TransformError`]：mutation API 失败时的结构化错误
//! - [`RewriteResult`]：visitor 闭包的有限返回值
//! - [`Traversal`]：图遍历顺序

use crate::id::NodeId;

// ---------------------------------------------------------------------------
// TransformError
// ---------------------------------------------------------------------------

/// mutation API 操作失败的结构化原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformError {
    /// 请求操作的节点在图中不存在。
    NotFound(NodeId),
    /// 目标节点不是能承载子节点的 parent（不是 Section / List 等）。
    InvalidParent(NodeId),
    /// 子节点 id 不在 parent 的 children 列表中。
    ChildNotFound {
        /// 父节点 id。
        parent: NodeId,
        /// 缺失的子节点 id。
        child: NodeId,
    },
    /// 请求的表格节点不存在，或不是 [`crate::Block::Table`]。
    NotATable(NodeId),
    /// 请求的 colspan / rowspan 超出支持的 1..=65535 范围。
    InvalidSpan {
        /// 请求的列跨度。
        colspan: u32,
        /// 请求的行跨度。
        rowspan: u32,
    },
    /// 网格行下标超出表格的行数。
    RowOutOfRange {
        /// 表格节点 id。
        table: NodeId,
        /// 请求的行下标。
        row: usize,
        /// 表格当前行数（head + body + foot）。
        rows: usize,
    },
    /// 网格列下标超出表格的列宽。
    ColumnOutOfRange {
        /// 表格节点 id。
        table: NodeId,
        /// 请求的列下标。
        column: usize,
        /// 可用列宽（插入时含末尾插入点）。
        columns: usize,
    },
    /// 占位单元格数量与表格网格不匹配。
    CellCountMismatch {
        /// 表格节点 id。
        table: NodeId,
        /// 网格要求的单元格数。
        expected: usize,
        /// 实际提供的单元格数。
        actual: usize,
    },
    /// 表格没有可插入的网格列。
    EmptyTable(NodeId),
}

impl std::fmt::Display for TransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "node {id:?} not found in graph"),
            Self::InvalidParent(id) => write!(f, "node {id:?} cannot be a parent (does not carry children)"),
            Self::ChildNotFound { parent, child } => {
                write!(f, "child {child:?} not found in parent {parent:?} children")
            }
            Self::NotATable(id) => write!(f, "node {id:?} is not a table block"),
            Self::InvalidSpan { colspan, rowspan } => {
                write!(f, "colspan {colspan} and rowspan {rowspan} must each be between 1 and 65535")
            }
            Self::RowOutOfRange { table, row, rows } => {
                write!(f, "row {row} is outside the table {table:?}'s {rows} rows")
            }
            Self::ColumnOutOfRange { table, column, columns } => {
                write!(f, "column {column} is outside the table {table:?}'s {columns} grid columns")
            }
            Self::CellCountMismatch { table, expected, actual } => {
                write!(f, "cell list needs {expected} entries for the table {table:?} grid but has {actual}")
            }
            Self::EmptyTable(table) => write!(f, "table {table:?} has no grid columns"),
        }
    }
}

impl std::error::Error for TransformError {}

// ---------------------------------------------------------------------------
// Diagnostic (transform-level, distinct from ValidationIssue)
// ---------------------------------------------------------------------------

/// transform pass 在 visitor 中产生的诊断信息。
///
/// 这和 [`crate::ValidationIssue`] 是两层概念：
/// - ValidationIssue 是图结构校验器发现的不变量违反
/// - Diagnostic 是变换逻辑本身产生的错误/警告
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// 相关节点（若有）。
    pub node: Option<NodeId>,
    /// 人类可读的描述。
    pub message: String,
}

impl Diagnostic {
    /// 构造一个不带节点的诊断。
    pub fn msg(message: impl Into<String>) -> Self {
        Self { node: None, message: message.into() }
    }

    /// 构造一个与某个节点相关的诊断。
    pub fn for_node(node: NodeId, message: impl Into<String>) -> Self {
        Self { node: Some(node), message: message.into() }
    }
}

// ---------------------------------------------------------------------------
// RewriteResult
// ---------------------------------------------------------------------------

/// [`crate::walk::DocumentGraph::walk_mut`] visitor 的有限返回值。
///
/// 不允许任意闭包返回 Markdown 字符串 —— 变换只能通过下面
/// 这几种受控语义来改变图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteResult {
    /// 节点保持不变，继续遍历。
    Keep,
    /// 把当前节点替换为一组新节点。
    ///
    /// **调用方必须先把新节点 push 进 graph，然后把 NodeId 填到这里。**
    /// `walk_mut` 会把当前节点从 graph 中移除，并把新节点接入到
    /// 原来的 parent children 列表中。
    Replace(Vec<NodeId>),
    /// 删除当前节点（以及所有在它 children 里的 block 节点）。
    Remove,
    /// 删除当前 Block，保留其 children 挂到 parent 下。
    ///
    /// 语义上等价于：把 parent 的 children 里 `id` 替换为 `id` 的 children。
    Unwrap,
    /// 保留节点本身，但不再递归它的子节点。
    SkipChildren,
    /// 变换失败，附带诊断；节点保持原样，诊断会被收集。
    Error(Diagnostic),
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

/// 图遍历顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Traversal {
    /// 先父后子（pre-order）。
    TopDown,
    /// 先子后父（post-order），默认推荐。
    BottomUp,
}
