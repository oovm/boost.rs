//! Typed table object operations on a [`DeckEdit`] draft.
//!
//! These methods carry the table editing semantics the OpenPPT product exposed
//! through JSON script operations (`table.setCell`, `table.insertRow`,
//! `table.deleteRow`, `table.insertColumn`, `table.deleteColumn`,
//! `table.transpose`, `element.duplicate`, `element.reorder`), but with typed
//! Rust inputs and outputs. Table insertion itself stays on
//! [`DeckEdit::add_table`](super::edit::DeckEdit::add_table).
//!
//! Every operation mutates the draft in place; atomicity and rollback come from
//! [`Deck::transaction`](super::model::Deck::transaction), so a failing
//! operation never leaves a half-written table and never consumes a revision.
//! Object identities are preserved by all in-place edits; only
//! [`DeckEdit::duplicate_table`] allocates a fresh identity.

use super::{
    edit::DeckEdit,
    error::{DeckError, codes},
    id::{ElementId, SlideId},
    model::{Element, ElementContent, ElementOrigin, Fill, OriginKind, TableCell, TableContent, TableRow, TextContent},
};

/// One row insertion accepted by [`DeckEdit::insert_table_row`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRowInsertInput {
    /// Grid position of the new row; `0..=rows` are valid insertion points.
    pub index: usize,
    /// Plain text for each column, in column order; empty cells are used when absent.
    pub cells: Option<Vec<String>>,
    /// Row height in EMU; the neighbouring row's height is inherited when absent.
    pub height_emu: Option<i64>,
}

/// One column insertion accepted by [`DeckEdit::insert_table_column`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumnInsertInput {
    /// Grid position of the new column; `0..=columns` are valid insertion points.
    pub index: usize,
    /// Plain text for each row, in row order; empty cells are used when absent.
    pub cells: Option<Vec<String>>,
    /// Column width in EMU; the neighbouring column's width is inherited when absent.
    pub width_emu: Option<i64>,
}

/// Destination accepted by [`DeckEdit::move_table`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMoveTarget {
    /// Slide receiving the table; may be the current slide for a reorder.
    pub slide: SlideId,
    /// Paint-order position inside the target slide after the table is detached;
    /// the table is appended when absent.
    pub index: Option<usize>,
}

/// Options accepted by [`DeckEdit::duplicate_table`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDuplicateOptions {
    /// Slide receiving the copy; the source slide is used when absent.
    pub target_slide: Option<SlideId>,
    /// Extra left offset in EMU applied to the copy.
    pub offset_x_emu: Option<i64>,
    /// Extra top offset in EMU applied to the copy.
    pub offset_y_emu: Option<i64>,
}

impl DeckEdit {
    /// Removes a table element, rejecting non-table targets before anything changes.
    pub fn remove_table(&mut self, slide: &SlideId, element: &ElementId) -> Result<(), DeckError> {
        assert_table(self.document().slide(slide), slide, element, "only tables can be removed as tables")?;
        self.remove_element(slide, element)
    }

    /// Writes the plain text of one cell, replacing its runs.
    pub fn set_table_cell_text(&mut self, slide: &SlideId, element: &ElementId, row: usize, column: usize, text: impl Into<String>) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry cells")?;
        assert_cell_bounds(table, row, column)?;
        table.rows[row].cells[column].text = TextContent::plain(text);
        Ok(())
    }

    /// Sets or clears the fill override of one cell.
    pub fn set_table_cell_fill(&mut self, slide: &SlideId, element: &ElementId, row: usize, column: usize, fill: Option<Fill>) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry cells")?;
        assert_cell_bounds(table, row, column)?;
        table.rows[row].cells[column].fill = fill;
        Ok(())
    }

    /// Replaces every column width, rejecting a count mismatch and non-positive widths.
    pub fn set_table_column_widths(&mut self, slide: &SlideId, element: &ElementId, widths: Vec<i64>) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry columns")?;
        if widths.len() != table.column_widths_emu.len() {
            return Err(invalid(format!("`widths` lists {} entries but the table has {} columns", widths.len(), table.column_widths_emu.len())));
        }
        if widths.iter().any(|width| *width <= 0) {
            return Err(invalid("table column widths must be positive"));
        }
        table.column_widths_emu = widths;
        Ok(())
    }

    /// Replaces every row height, rejecting a count mismatch and non-positive heights.
    pub fn set_table_row_heights(&mut self, slide: &SlideId, element: &ElementId, heights: Vec<i64>) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry rows")?;
        if heights.len() != table.rows.len() {
            return Err(invalid(format!("`heights` lists {} entries but the table has {} rows", heights.len(), table.rows.len())));
        }
        if heights.iter().any(|height| *height <= 0) {
            return Err(invalid("table row heights must be positive"));
        }
        for (row, height) in table.rows.iter_mut().zip(heights) {
            row.height_emu = height;
        }
        Ok(())
    }

    /// Inserts one row, rejecting insertion points crossed by a vertical merge.
    pub fn insert_table_row(&mut self, slide: &SlideId, element: &ElementId, input: TableRowInsertInput) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry rows")?;
        let columns = table.column_widths_emu.len();
        if input.index > table.rows.len() {
            return Err(invalid(format!("`index` {} is outside the table's insertion points 0..={}", input.index, table.rows.len())));
        }
        if table.rows.iter().enumerate().any(|(position, row)| position < input.index && row.cells.iter().any(|cell| position + cell.row_span as usize > input.index)) {
            return Err(invalid("a merged cell spans across the insertion point; insert after it instead"));
        }
        let cells = match input.cells {
            Some(texts) => {
                if texts.len() != columns {
                    return Err(invalid(format!("`cells` lists {} entries but the table has {columns} columns", texts.len())));
                }
                texts.into_iter().map(TableCell::plain).collect::<Vec<_>>()
            }
            None => (0..columns).map(|_| TableCell::plain("")).collect::<Vec<_>>(),
        };
        let height_emu = match input.height_emu {
            Some(height) if height > 0 => height,
            Some(_) => return Err(invalid("`height_emu` must be positive")),
            None => table.rows.get(input.index).or(table.rows.last()).map(|row| row.height_emu).unwrap_or_default(),
        };
        table.rows.insert(input.index, TableRow { height_emu, cells });
        Ok(())
    }

    /// Removes one row; the table must keep at least one row and no merged cell may cover it.
    pub fn delete_table_row(&mut self, slide: &SlideId, element: &ElementId, index: usize) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry rows")?;
        if index >= table.rows.len() {
            return Err(invalid(format!("`index` {index} is outside the table's {} rows", table.rows.len())));
        }
        if table.rows.len() == 1 {
            return Err(invalid("a table must keep at least one row"));
        }
        if table.rows.iter().enumerate().any(|(position, row)| {
            row.cells.iter().any(|cell| (position < index && position + cell.row_span as usize > index) || (position == index && cell.row_span > 1))
        }) {
            return Err(invalid("row is covered by a merged cell; remove the merge before deleting it"));
        }
        table.rows.remove(index);
        Ok(())
    }

    /// Inserts one column, rejecting insertion points crossed by a horizontal merge.
    pub fn insert_table_column(&mut self, slide: &SlideId, element: &ElementId, input: TableColumnInsertInput) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry columns")?;
        if input.index > table.column_widths_emu.len() {
            return Err(invalid(format!("`index` {} is outside the table's insertion points 0..={}", input.index, table.column_widths_emu.len())));
        }
        for row in &table.rows {
            let mut position = 0usize;
            for cell in &row.cells {
                if position < input.index && position + cell.column_span as usize > input.index {
                    return Err(invalid("a merged cell spans across the insertion point; insert after it instead"));
                }
                position += cell.column_span as usize;
            }
        }
        let rows = table.rows.len();
        let texts = match input.cells {
            Some(texts) => {
                if texts.len() != rows {
                    return Err(invalid(format!("`cells` lists {} entries but the table has {rows} rows", texts.len())));
                }
                texts
            }
            None => vec![String::new(); rows],
        };
        let width_emu = match input.width_emu {
            Some(width) if width > 0 => width,
            Some(_) => return Err(invalid("`width_emu` must be positive")),
            None => table.column_widths_emu.get(input.index).or(table.column_widths_emu.last()).copied().unwrap_or_default(),
        };
        for (position_in_rows, row) in table.rows.iter_mut().enumerate() {
            let mut position = 0usize;
            let mut cell_index = 0usize;
            while position < input.index && cell_index < row.cells.len() {
                position += row.cells[cell_index].column_span as usize;
                cell_index += 1;
            }
            row.cells.insert(cell_index, TableCell::plain(texts[position_in_rows].clone()));
        }
        table.column_widths_emu.insert(input.index, width_emu);
        Ok(())
    }

    /// Removes one column; the table must keep at least one column and no merged cell may cover it.
    pub fn delete_table_column(&mut self, slide: &SlideId, element: &ElementId, index: usize) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "only tables carry columns")?;
        if index >= table.column_widths_emu.len() {
            return Err(invalid(format!("`index` {index} is outside the table's {} columns", table.column_widths_emu.len())));
        }
        if table.column_widths_emu.len() == 1 {
            return Err(invalid("a table must keep at least one column"));
        }
        for row in &table.rows {
            let mut position = 0usize;
            for cell in &row.cells {
                let span = cell.column_span as usize;
                if span > 1 && position <= index && position + span > index {
                    return Err(invalid("column is covered by a merged cell; remove the merge before deleting it"));
                }
                position += span;
            }
        }
        for row in &mut table.rows {
            let mut position = 0usize;
            for cell_index in 0..row.cells.len() {
                if position == index {
                    row.cells.remove(cell_index);
                    break;
                }
                position += row.cells[cell_index].column_span as usize;
            }
        }
        table.column_widths_emu.remove(index);
        Ok(())
    }

    /// Re-seats one row, keeping its cells and its height.
    ///
    /// `to` counts the rows that remain after the row is detached, matching
    /// [`DeckEdit::move_slide`](super::edit::DeckEdit::move_slide). Vertical
    /// merges refuse the move, because re-seating would break the merge.
    /// Returns the final position.
    pub fn move_table_row(&mut self, slide: &SlideId, element: &ElementId, from: usize, to: usize) -> Result<usize, DeckError> {
        let table = self.table_mut(slide, element, "only tables carry rows")?;
        if from >= table.rows.len() {
            return Err(invalid(format!("`row` {from} is outside the table's {} rows", table.rows.len())));
        }
        if to >= table.rows.len() {
            return Err(invalid(format!("`index` {to} is outside the table's {} rows", table.rows.len())));
        }
        if table.rows.iter().any(|row| row.cells.iter().any(|cell| cell.row_span > 1)) {
            return Err(invalid("row moves need a table without vertical merges"));
        }
        let row = table.rows.remove(from);
        table.rows.insert(to, row);
        Ok(to)
    }

    /// Copies one row next to itself, returning the copy's position.
    ///
    /// The copy keeps the source cells and height; only the grid position is new.
    /// `index` defaults to the row right after the source. Vertical merges refuse
    /// the copy, because a duplicated merge would cover the copied row twice.
    pub fn duplicate_table_row(&mut self, slide: &SlideId, element: &ElementId, row: usize, index: Option<usize>) -> Result<usize, DeckError> {
        let table = self.table_mut(slide, element, "only tables carry rows")?;
        if row >= table.rows.len() {
            return Err(invalid(format!("`row` {row} is outside the table's {} rows", table.rows.len())));
        }
        if table.rows.iter().any(|candidate| candidate.cells.iter().any(|cell| cell.row_span > 1)) {
            return Err(invalid("row duplication needs a table without vertical merges"));
        }
        let index = index.unwrap_or(row + 1);
        if index > table.rows.len() {
            return Err(invalid(format!("`index` {index} is outside the table's insertion points 0..={}", table.rows.len())));
        }
        let copy = table.rows[row].clone();
        table.rows.insert(index, copy);
        Ok(index)
    }

    /// Re-seats one column, keeping its cells and its width.
    ///
    /// `to` counts the columns that remain after the column is detached.
    /// Horizontal merges refuse the move. Returns the final position.
    pub fn move_table_column(&mut self, slide: &SlideId, element: &ElementId, from: usize, to: usize) -> Result<usize, DeckError> {
        let table = self.table_mut(slide, element, "only tables carry columns")?;
        let columns = table.column_widths_emu.len();
        if from >= columns {
            return Err(invalid(format!("`column` {from} is outside the table's {columns} columns")));
        }
        if to >= columns {
            return Err(invalid(format!("`index` {to} is outside the table's {columns} columns")));
        }
        if table.rows.iter().any(|row| row.cells.iter().any(|cell| cell.column_span > 1)) {
            return Err(invalid("column moves need a table without horizontal merges"));
        }
        if table.rows.iter().any(|row| row.cells.len() != columns) {
            return Err(invalid("column moves need a rectangular table; every row must carry one cell per column"));
        }
        for row in &mut table.rows {
            let cell = row.cells.remove(from);
            row.cells.insert(to, cell);
        }
        let width = table.column_widths_emu.remove(from);
        table.column_widths_emu.insert(to, width);
        Ok(to)
    }

    /// Copies one column next to itself, returning the copy's position.
    ///
    /// `index` defaults to the column right after the source. Horizontal merges
    /// refuse the copy for the same reason row duplication refuses vertical ones.
    pub fn duplicate_table_column(&mut self, slide: &SlideId, element: &ElementId, column: usize, index: Option<usize>) -> Result<usize, DeckError> {
        let table = self.table_mut(slide, element, "only tables carry columns")?;
        let columns = table.column_widths_emu.len();
        if column >= columns {
            return Err(invalid(format!("`column` {column} is outside the table's {columns} columns")));
        }
        if table.rows.iter().any(|row| row.cells.iter().any(|cell| cell.column_span > 1)) {
            return Err(invalid("column duplication needs a table without horizontal merges"));
        }
        if table.rows.iter().any(|row| row.cells.len() != columns) {
            return Err(invalid("column duplication needs a rectangular table; every row must carry one cell per column"));
        }
        let index = index.unwrap_or(column + 1);
        if index > columns {
            return Err(invalid(format!("`index` {index} is outside the table's insertion points 0..={columns}")));
        }
        for row in &mut table.rows {
            let copy = row.cells[column].clone();
            row.cells.insert(index, copy);
        }
        let width = table.column_widths_emu[column];
        table.column_widths_emu.insert(index, width);
        Ok(index)
    }

    /// Swaps rows and columns: the new row count is the old column count, new
    /// row heights take the old column widths and new column widths take the old
    /// row heights. The element identity is preserved.
    pub fn transpose_table(&mut self, slide: &SlideId, element: &ElementId) -> Result<(), DeckError> {
        let table = self.table_mut(slide, element, "transposition needs a table")?;
        if table.rows.iter().any(|row| row.cells.iter().any(|cell| cell.row_span > 1 || cell.column_span > 1)) {
            return Err(invalid("transposition needs a table without merged cells"));
        }
        let columns = table.column_widths_emu.len();
        if table.rows.iter().any(|row| row.cells.len() != columns) {
            return Err(invalid("transposition needs a rectangular table; every row must carry one cell per column"));
        }
        let heights = table.rows.iter().map(|row| row.height_emu).collect::<Vec<_>>();
        let transposed = (0..columns)
            .map(|column| TableRow {
                height_emu: table.column_widths_emu[column],
                cells: table.rows.iter().map(|row| row.cells[column].clone()).collect(),
            })
            .collect::<Vec<_>>();
        table.rows = transposed;
        table.column_widths_emu = heights;
        Ok(())
    }

    /// Moves a table to another slide or to a new paint-order position on the
    /// same slide, keeping its identity. Returns the final position.
    pub fn move_table(&mut self, slide: &SlideId, element: &ElementId, target: TableMoveTarget) -> Result<usize, DeckError> {
        assert_table(self.document().slide(slide), slide, element, "only tables can be moved as tables")?;
        let moved = {
            let source = self.slide_mut(slide)?;
            let index = source
                .elements
                .iter()
                .position(|candidate| &candidate.id == element)
                .ok_or_else(|| missing_element(slide, element))?;
            source.elements.remove(index)
        };
        let destination = self.slide_mut(&target.slide)?;
        let index = target.index.unwrap_or(destination.elements.len()).min(destination.elements.len());
        destination.elements.insert(index, moved);
        Ok(index)
    }

    /// Copies a table with a fresh element identity onto the target slide
    /// (the source slide by default), optionally offset in EMU.
    pub fn duplicate_table(&mut self, slide: &SlideId, element: &ElementId, options: TableDuplicateOptions) -> Result<ElementId, DeckError> {
        let source = {
            let model = self.document().slide(slide).ok_or_else(|| missing_slide(slide))?;
            model.element(element).cloned().ok_or_else(|| missing_element(slide, element))?
        };
        let ElementContent::Table(table) = &source.content else {
            return Err(not_a_table(slide, element, "only tables can be duplicated as tables"));
        };
        let mut transform = source.transform;
        transform.x_emu = transform.x_emu.saturating_add(options.offset_x_emu.unwrap_or(0));
        transform.y_emu = transform.y_emu.saturating_add(options.offset_y_emu.unwrap_or(0));
        let mut copy = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Table(table.clone()),
        );
        copy.alt = source.alt.clone();
        copy.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        let copy_id = copy.id.clone();
        let destination = self.slide_mut(options.target_slide.as_ref().unwrap_or(slide))?;
        destination.elements.push(copy);
        Ok(copy_id)
    }

    /// Borrows the table content of one element, rejecting non-table targets.
    pub(crate) fn table_mut(&mut self, slide: &SlideId, element: &ElementId, needs: &str) -> Result<&mut TableContent, DeckError> {
        let model = self.slide_mut(slide)?;
        let target = model.element_mut(element).ok_or_else(|| missing_element(slide, element))?;
        match &mut target.content {
            ElementContent::Table(table) => Ok(table),
            _ => Err(not_a_table(slide, element, needs)),
        }
    }
}

fn assert_table(model: Option<&super::model::DeckSlide>, slide: &SlideId, element: &ElementId, needs: &str) -> Result<(), DeckError> {
    let model = model.ok_or_else(|| missing_slide(slide))?;
    let target = model.element(element).ok_or_else(|| missing_element(slide, element))?;
    match &target.content {
        ElementContent::Table(_) => Ok(()),
        _ => Err(not_a_table(slide, element, needs)),
    }
}

fn assert_cell_bounds(table: &TableContent, row: usize, column: usize) -> Result<(), DeckError> {
    if row >= table.rows.len() {
        return Err(invalid(format!("`row` {row} is outside the table's {} rows", table.rows.len())));
    }
    let columns = table.rows[row].cells.len();
    if column >= columns {
        return Err(invalid(format!("`column` {column} is outside row {row}'s {columns} cells")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> DeckError {
    DeckError::new(codes::ARGUMENT_INVALID, message)
}

fn missing_slide(slide: &SlideId) -> DeckError {
    DeckError::new(codes::OBJECT_NOT_FOUND, format!("slide `{slide}` does not exist")).with_slide(slide.clone())
}

fn missing_element(slide: &SlideId, element: &ElementId) -> DeckError {
    DeckError::new(codes::OBJECT_NOT_FOUND, format!("element `{element}` does not exist on this slide"))
        .with_slide(slide.clone())
        .with_element(element.clone())
}

fn not_a_table(slide: &SlideId, element: &ElementId, needs: &str) -> DeckError {
    DeckError::new(codes::OBJECT_TYPE_MISMATCH, format!("element `{element}` is not a table; {needs}"))
        .with_slide(slide.clone())
        .with_element(element.clone())
}
