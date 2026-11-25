//! 语义变换的错误类型与返回值。
//!
//! 本模块为 DocumentTransform infrastructure 的基础枚举：
//! - [`TransformError`]：mutation API 失败时的结构化错误
//! - [`RewriteResult`]：visitor 闭包的有限返回值
//! - [`Traversal`]：图遍历顺序

use crate::{convert::BlockKind, id::NodeId};

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
    /// 请求的列表节点不存在，或不是 [`crate::Block::List`]。
    NotAList(NodeId),
    /// 条目下标超出列表的条目数。
    ItemOutOfRange {
        /// 列表节点 id。
        list: NodeId,
        /// 请求的条目下标。
        item: usize,
        /// 列表当前条目数。
        items: usize,
    },
    /// 移动/插入下标超出目标列表的合法位次（含末尾插入点）。
    IndexOutOfRange {
        /// 目标列表节点 id。
        list: NodeId,
        /// 请求的下标。
        index: usize,
        /// 合法下标数量。
        positions: usize,
    },
    /// 首条目没有可依附的前驱兄弟。
    NoPreviousSibling(NodeId),
    /// 条目末尾没有可提升的同样式嵌套列表。
    NothingToLift {
        /// 列表节点 id。
        list: NodeId,
        /// 条目下标。
        item: usize,
    },
    /// 移动目标嵌套在被移动条目内部，移动会形成环。
    MoveIntoDescendant {
        /// 源列表节点 id。
        list: NodeId,
        /// 被移动的条目下标。
        item: usize,
        /// 目标列表节点 id。
        target: NodeId,
    },
    /// 删除唯一条目会留下空的列表块，应改为删除整个块。
    CannotEmptyList {
        /// 列表节点 id。
        list: NodeId,
    },
    /// 重排编号会把列表编号推过 `u32` 上限。
    NumberingOverflow,
    /// 该块的类型不参与互转。
    NotConvertible(NodeId),
    /// 该块已经是请求的类型。
    AlreadyKind {
        /// 块节点 id。
        node: NodeId,
        /// 请求的类型。
        kind: BlockKind,
    },
    /// 请求的 (源类型, 目标类型) 不是受支持的转换对。
    InvalidConversion {
        /// 源类型。
        from: BlockKind,
        /// 目标类型。
        to: BlockKind,
    },
    /// 转标题需要一个 1..=6 的层级。
    MissingHeadingLevel(NodeId),
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
            Self::NotAList(id) => write!(f, "node {id:?} is not a list block"),
            Self::ItemOutOfRange { list, item, items } => write!(f, "item {item} is outside the list {list:?}'s {items} items"),
            Self::IndexOutOfRange { list, index, positions } => {
                write!(f, "index {index} is outside the list {list:?}'s {positions} positions")
            }
            Self::NoPreviousSibling(list) => write!(f, "the first item of list {list:?} has no previous sibling to indent under"),
            Self::NothingToLift { list, item } => {
                write!(f, "item {item} of list {list:?} has no nested list of the same marker style to lift")
            }
            Self::MoveIntoDescendant { list, item, target } => {
                write!(f, "item {item} of list {list:?} cannot move into list {target:?} nested inside it")
            }
            Self::CannotEmptyList { list } => {
                write!(f, "removing the only item of list {list:?} would leave an empty list block")
            }
            Self::NumberingOverflow => write!(f, "renumbering the list would push a number beyond the u32 maximum"),
            Self::NotConvertible(id) => write!(f, "node {id:?} is not a convertible block kind"),
            Self::AlreadyKind { node, kind } => write!(f, "node {node:?} is already a `{}`", kind.label()),
            Self::InvalidConversion { from, to } => write!(f, "cannot convert a `{}` into a `{}`", from.label(), to.label()),
            Self::MissingHeadingLevel(id) => write!(f, "converting node {id:?} to a heading needs a level between 1 and 6"),
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
