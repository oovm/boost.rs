//! 表格跨度与占位插入的图级变换。
//!
//! 边界条件对齐下游产品模型的 `table.*` 操作：
//! - 行按网格行下标定位（head → body → foot 连续编号），列按网格列下标
//!   定位（`colspan` 累加，落在跨度内的列指向锚点单元格）；
//! - 合并（[`DocumentGraph::set_cell_span`]）由锚点保留内容，消耗锚点行内
//!   被覆盖的单元格并物理删除其子树；设回 1x1 即拆分；
//! - 行/列插入为每个网格位置生成 1x1 占位单元格（内含一个占位段落），
//!   行插入落在源带内、表尾追加遵循 head/body/foot 分带规则；
//! - 行复制（[`DocumentGraph::duplicate_table_row`]）留在源分带内，保留每
//!   个单元格的行列跨度，单元格内容经深复制获得全新节点。

use std::collections::{BTreeMap, VecDeque};

use crate::{Block, DocumentGraph, Inline, NodeId, TableCell, TableRow, TransformError};

/// 跨度上限，与下游一致：`colspan` 与 `rowspan` 均须在 1..=65535。
const MAX_CELL_SPAN: u32 = 65_535;

impl DocumentGraph {
    /// 设置一个单元格覆盖的网格列数与行数（合并/拆分）。
    ///
    /// 单元格以左上角网格位置 `(row, column)` 定位。锚点保留其内容；锚点
    /// 行内起点落在 `column + colspan` 之前的后续单元格被移除，其子树经
    /// [`DocumentGraph::delete_subtree`] 物理删除。`rowspan` 只记录跨度，
    /// 跨行覆盖的下方单元格不做消耗（与下游一致，由后续显式操作维护）。
    /// 设回 `1x1` 即取消合并。
    ///
    /// 失败：跨度越界 [`TransformError::InvalidSpan`]、行越界
    /// [`TransformError::RowOutOfRange`]、列越界
    /// [`TransformError::ColumnOutOfRange`]、目标非表格
    /// [`TransformError::NotATable`]。
    pub fn set_cell_span(&mut self, table: NodeId, row: usize, column: usize, colspan: u32, rowspan: u32) -> Result<(), TransformError> {
        if colspan == 0 || rowspan == 0 || colspan > MAX_CELL_SPAN || rowspan > MAX_CELL_SPAN {
            return Err(TransformError::InvalidSpan { colspan, rowspan });
        }
        let (total_rows, columns) = self.table_shape(table)?;
        if row >= total_rows {
            return Err(TransformError::RowOutOfRange { table, row, rows: total_rows });
        }
        if column >= columns {
            return Err(TransformError::ColumnOutOfRange { table, column, columns });
        }
        let orphans = {
            let node = self.blocks.iter_mut().find(|node| node.id == table).expect("table_shape validated the table");
            let Block::Table { head, body, foot, .. } = &mut node.block else { unreachable!("table_shape validated a table") };
            let target = row_at(head, body, foot, row).expect("the row was validated");
            let index = grid_column_index(target, column);
            if index >= target.cells.len() {
                return Err(TransformError::ColumnOutOfRange { table, column, columns: row_width(target) });
            }
            // 合并消耗其覆盖的单元格：按合并前的网格，锚点之后起点落在
            // `column + colspan` 内的单元格离开该行。
            let span_end = column.saturating_add(colspan as usize);
            let mut covered: Vec<usize> = Vec::new();
            let mut grid = 0usize;
            for (i, cell) in target.cells.iter().enumerate() {
                let start = grid;
                grid = grid.saturating_add(cell.colspan.max(1) as usize);
                if i > index && start < span_end {
                    covered.push(i);
                }
            }
            target.cells[index].colspan = colspan;
            target.cells[index].rowspan = rowspan;
            let orphans: Vec<NodeId> = covered.iter().flat_map(|i| target.cells[*i].children.clone()).collect();
            for i in covered.into_iter().rev() {
                target.cells.remove(i);
            }
            orphans
        };
        for child in orphans {
            self.delete_subtree(child)?;
        }
        self.bump_revision();
        Ok(())
    }

    /// 在网格行 `row` 处插入一行，每个网格列得到一个 1x1 占位单元格。
    ///
    /// `cells` 为 `Some` 时必须恰好提供表格网格列数（最宽行的
    /// `colspan` 总和）个单元格内容，每个内容成为一个占位段落的 inline
    /// 正文；`None` 时全部为空段落占位。插入位置按 head/body/foot 分带落位：
    /// 行尾插入（`row == 总行数`）在有表脚时追加到表脚，否则追加到表体。
    ///
    /// 失败：无列网格 [`TransformError::EmptyTable`]、行越界
    /// [`TransformError::RowOutOfRange`]、内容数量不符
    /// [`TransformError::CellCountMismatch`]。
    pub fn insert_table_row(&mut self, table: NodeId, row: usize, cells: Option<Vec<Vec<Inline>>>) -> Result<(), TransformError> {
        let (total_rows, columns) = self.table_shape(table)?;
        if columns == 0 {
            return Err(TransformError::EmptyTable(table));
        }
        if row > total_rows {
            return Err(TransformError::RowOutOfRange { table, row, rows: total_rows });
        }
        let contents = match cells {
            Some(values) if values.len() != columns => {
                return Err(TransformError::CellCountMismatch { table, expected: columns, actual: values.len() });
            }
            Some(values) => values,
            None => vec![Vec::new(); columns],
        };
        let mut new_cells = Vec::with_capacity(contents.len());
        for content in contents {
            let child = self.push_block(Block::Paragraph { content });
            new_cells.push(TableCell { children: vec![child], colspan: 1, rowspan: 1 });
        }
        let new_row = TableRow { cells: new_cells };
        {
            let node = self.blocks.iter_mut().find(|node| node.id == table).expect("table_shape validated the table");
            let Block::Table { head, body, foot, .. } = &mut node.block else { unreachable!("table_shape validated a table") };
            let head_len = head.len();
            let body_len = body.len();
            if row < head_len {
                head.insert(row, new_row);
            }
            else if row < head_len + body_len {
                body.insert(row - head_len, new_row);
            }
            else if row < head_len + body_len + foot.len() {
                foot.insert(row - head_len - body_len, new_row);
            }
            else if !foot.is_empty() {
                foot.push(new_row);
            }
            else {
                body.push(new_row);
            }
        }
        self.bump_revision();
        Ok(())
    }

    /// 在网格列 `column` 处插入一列，每行得到一个 1x1 占位单元格。
    ///
    /// `cells` 为 `Some` 时必须恰好提供总行数（head + body + foot）个内容，
    /// 按网格行序（head → body → foot）逐行消费；`None` 时全部为空段落占位。
    /// 每行的插入位置由网格列下标换算（[`grid_column_index`]），因此带跨度的
    /// 行会把新单元格插到覆盖该列的单元格之前。
    ///
    /// 失败：无列网格 [`TransformError::EmptyTable`]、列越界
    /// [`TransformError::ColumnOutOfRange`]、内容数量不符
    /// [`TransformError::CellCountMismatch`]。
    pub fn insert_table_column(&mut self, table: NodeId, column: usize, cells: Option<Vec<Vec<Inline>>>) -> Result<(), TransformError> {
        let (total_rows, columns) = self.table_shape(table)?;
        if columns == 0 {
            return Err(TransformError::EmptyTable(table));
        }
        if column > columns {
            return Err(TransformError::ColumnOutOfRange { table, column, columns });
        }
        let contents = match cells {
            Some(values) if values.len() != total_rows => {
                return Err(TransformError::CellCountMismatch { table, expected: total_rows, actual: values.len() });
            }
            Some(values) => values,
            None => vec![Vec::new(); total_rows],
        };
        let mut queue = VecDeque::with_capacity(contents.len());
        for content in contents {
            let child = self.push_block(Block::Paragraph { content });
            queue.push_back(TableCell { children: vec![child], colspan: 1, rowspan: 1 });
        }
        {
            let node = self.blocks.iter_mut().find(|node| node.id == table).expect("table_shape validated the table");
            let Block::Table { head, body, foot, .. } = &mut node.block else { unreachable!("table_shape validated a table") };
            for target in head.iter_mut().chain(body.iter_mut()).chain(foot.iter_mut()) {
                let index = grid_column_index(target, column);
                let cell = queue.pop_front().expect("one new cell was built per row");
                target.cells.insert(index, cell);
            }
        }
        self.bump_revision();
        Ok(())
    }

    /// 深复制网格行 `row` 并紧随其后插入，返回副本的网格行下标。
    ///
    /// 副本留在源分带（head/body/foot）内；每个单元格保留其行列跨度；
    /// 单元格内容经 [`DocumentGraph::copy_node_detached`] 深复制为全新节点
    /// （保留来源跨度），指向缺失子节点的引用被丢弃。
    ///
    /// 失败：行越界 [`TransformError::RowOutOfRange`]、目标非表格
    /// [`TransformError::NotATable`]。
    pub fn duplicate_table_row(&mut self, table: NodeId, row: usize) -> Result<usize, TransformError> {
        let (total_rows, _) = self.table_shape(table)?;
        if row >= total_rows {
            return Err(TransformError::RowOutOfRange { table, row, rows: total_rows });
        }
        let (cells, band, offset) = {
            let node = self.block(table).expect("table_shape validated the table");
            let Block::Table { head, body, foot, .. } = &node.block else { unreachable!("table_shape validated a table") };
            if row < head.len() {
                (head[row].cells.clone(), 0usize, row)
            }
            else if row < head.len() + body.len() {
                (body[row - head.len()].cells.clone(), 1, row - head.len())
            }
            else {
                (foot[row - head.len() - body.len()].cells.clone(), 2, row - head.len() - body.len())
            }
        };
        let mut mapping = BTreeMap::new();
        let mut new_cells = Vec::with_capacity(cells.len());
        for cell in &cells {
            let mut children = Vec::with_capacity(cell.children.len());
            for child in &cell.children {
                if self.block(*child).is_some() {
                    children.push(self.copy_node_detached(*child, &mut mapping));
                }
            }
            new_cells.push(TableCell { children, colspan: cell.colspan, rowspan: cell.rowspan });
        }
        let copy = TableRow { cells: new_cells };
        {
            let node = self.blocks.iter_mut().find(|node| node.id == table).expect("table_shape validated the table");
            let Block::Table { head, body, foot, .. } = &mut node.block else { unreachable!("table_shape validated a table") };
            match band {
                0 => head.insert(offset + 1, copy),
                1 => body.insert(offset + 1, copy),
                _ => foot.insert(offset + 1, copy),
            }
        }
        self.bump_revision();
        Ok(row + 1)
    }

    /// 表格网格形状：(总行数 head+body+foot, 最宽行的网格列数)。
    ///
    /// 目标不存在返回 [`TransformError::NotFound`]，存在但非表格返回
    /// [`TransformError::NotATable`]。
    pub(crate) fn table_shape(&self, table: NodeId) -> Result<(usize, usize), TransformError> {
        let node = self.block(table).ok_or(TransformError::NotFound(table))?;
        let Block::Table { head, body, foot, .. } = &node.block else { return Err(TransformError::NotATable(table)) };
        let rows = head.len() + body.len() + foot.len();
        let columns = head.iter().chain(body.iter()).chain(foot.iter()).map(row_width).max().unwrap_or(0);
        Ok((rows, columns))
    }
}

/// 一行的网格宽度：各单元格 `colspan`（至少 1）之和。
fn row_width(row: &TableRow) -> usize {
    row.cells.iter().fold(0usize, |total, cell| total.saturating_add(cell.colspan.max(1) as usize))
}

/// 网格列下标换算为行内单元格下标：落在某单元格跨度内即指向该单元格；
/// 超出行宽时返回 `cells.len()`（行尾插入点）。
fn grid_column_index(row: &TableRow, column: usize) -> usize {
    let mut start = 0usize;
    for (index, cell) in row.cells.iter().enumerate() {
        let span = cell.colspan.max(1) as usize;
        if column < start + span {
            return index;
        }
        start += span;
    }
    row.cells.len()
}

/// 按网格行下标（head → body → foot 连续编号）取可变行引用。
fn row_at<'a>(head: &'a mut [TableRow], body: &'a mut [TableRow], foot: &'a mut [TableRow], row: usize) -> Option<&'a mut TableRow> {
    let head_len = head.len();
    if row < head_len {
        return head.get_mut(row);
    }
    let body_len = body.len();
    if row < head_len + body_len {
        return body.get_mut(row - head_len);
    }
    foot.get_mut(row - head_len - body_len)
}
