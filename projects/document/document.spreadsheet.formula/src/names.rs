//! Workbook-scope defined names bound to sheet rectangles.
//!
//! A [`NameTable`] registers the names a workbook carries; the engine resolves
//! bare identifiers through [`crate::Workbook::named_range`] so a defined name
//! evaluates, feeds the dependency graph and joins cycle detection like the
//! cells it denotes. Row and column edits move the stored rectangles with the
//! same whole-line rules the formula text follows.

use crate::{
    model::{CellRange, parse_reference},
    shift::{Axis, EditDirection, LineEdit},
};

/// Last row of an XLSX sheet.
const MAX_ROW: u32 = 1_048_576;
/// Last column of an XLSX sheet, `XFD`.
const MAX_COLUMN: u32 = 16_384;

/// One defined name bound to one sheet rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRange {
    /// Defined name as the caller typed it.
    pub name: String,
    /// Name of the sheet the range lives on.
    pub sheet: String,
    /// Rectangle the name resolves to.
    pub range: CellRange,
}

/// Registry of the workbook-scope defined names.
///
/// Names are matched case-insensitively, the way a spreadsheet resolves them;
/// the newest spelling of a redefined name is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameTable {
    entries: Vec<NamedRange>,
}

impl NameTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `name` on `sheet` for `range`, replacing a previous entry that
    /// differs only in case. Fails when the name is empty, spells a reference,
    /// contains a `:` or the range is not a valid rectangle inside the grid.
    pub fn define(&mut self, name: &str, sheet: &str, range: CellRange) -> Result<(), String> {
        check_name(name)?;
        if sheet.is_empty() {
            return Err("a named range needs a sheet".to_owned());
        }
        if !range.is_valid() {
            return Err(format!("`{}` does not spell a valid range", reference_text(&range)));
        }
        if range.end_row > MAX_ROW || range.end_column > MAX_COLUMN {
            return Err(format!("`{}` passes the grid", reference_text(&range)));
        }
        let entry = NamedRange { name: name.to_owned(), sheet: sheet.to_owned(), range };
        match self.entries.iter_mut().find(|existing| existing.name.eq_ignore_ascii_case(name)) {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
        Ok(())
    }

    /// Removes one name, reporting whether it was registered.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| !entry.name.eq_ignore_ascii_case(name));
        self.entries.len() != before
    }

    /// Resolves one name, ignoring ASCII case.
    pub fn resolve(&self, name: &str) -> Option<&NamedRange> {
        self.entries.iter().find(|entry| entry.name.eq_ignore_ascii_case(name))
    }

    /// Every registered name in definition order.
    pub fn list(&self) -> &[NamedRange] {
        &self.entries
    }

    /// Gives one registered name a new spelling, reporting whether it applied.
    /// Fails when the name is unknown, the new spelling is not a valid name or
    /// another entry already carries it.
    pub fn rename(&mut self, name: &str, new: &str) -> bool {
        let Some(position) = self.entries.iter().position(|entry| entry.name.eq_ignore_ascii_case(name))
        else {
            return false;
        };
        if check_name(new).is_err()
            || self.entries.iter().enumerate().any(|(index, entry)| index != position && entry.name.eq_ignore_ascii_case(new))
        {
            return false;
        }
        self.entries[position].name = new.to_owned();
        true
    }

    /// Moves every rectangle on the edited sheet through the edit.
    ///
    /// An insert shifts ranges at or past the point down and expands a range
    /// that crosses it; a delete shrinks or moves a range and drops one whose
    /// rectangle falls entirely inside the deleted band.
    pub fn shift(&mut self, edit: &LineEdit) {
        match edit.direction {
            EditDirection::Insert => {
                let (max, position, count) = (axis_max(edit.axis), edit.position, edit.count);
                for entry in &mut self.entries {
                    if !entry.sheet.eq_ignore_ascii_case(&edit.sheet) {
                        continue;
                    }
                    let (start, end) = span(entry, edit.axis);
                    if end < position {
                        continue;
                    }
                    let move_line = |value: u32| value.saturating_add(count).min(max);
                    let new_start = if start >= position { move_line(start) } else { start };
                    let new_end = move_line(end);
                    set_span(entry, edit.axis, new_start, new_end);
                }
            }
            EditDirection::Delete => {
                let (start, end) = (edit.position, edit.delete_end());
                let axis = edit.axis;
                let sheet = edit.sheet.clone();
                self.entries.retain_mut(|entry| {
                    if !entry.sheet.eq_ignore_ascii_case(&sheet) {
                        return true;
                    }
                    let (low, high) = span(entry, axis);
                    match delete_span(low, high, start, end) {
                        Some((new_low, new_high)) => {
                            set_span(entry, axis, new_low, new_high);
                            true
                        }
                        None => false,
                    }
                });
            }
        }
    }

    /// Points every entry at the renamed sheet, ignoring ASCII case.
    pub fn rename_sheet(&mut self, old: &str, new: &str) {
        for entry in &mut self.entries {
            if entry.sheet.eq_ignore_ascii_case(old) {
                entry.sheet = new.to_owned();
            }
        }
    }
}

/// Rejects names that are empty or spell a reference instead of an identifier.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a defined name cannot be empty".to_owned());
    }
    if name.contains(':') {
        return Err(format!("`{name}` looks like a range, not a name"));
    }
    if parse_reference(name).is_some() {
        return Err(format!("`{name}` is a cell reference, not a name"));
    }
    Ok(())
}

/// Rectangle span of an entry on the edited axis.
fn span(entry: &NamedRange, axis: Axis) -> (u32, u32) {
    match axis {
        Axis::Row => (entry.range.start_row, entry.range.end_row),
        Axis::Column => (entry.range.start_column, entry.range.end_column),
    }
}

/// Writes a rectangle span back on the edited axis.
fn set_span(entry: &mut NamedRange, axis: Axis, start: u32, end: u32) {
    match axis {
        Axis::Row => {
            entry.range.start_row = start;
            entry.range.end_row = end;
        }
        Axis::Column => {
            entry.range.start_column = start;
            entry.range.end_column = end;
        }
    }
}

/// Maps one span through a deleted band; `None` means the span vanished.
fn delete_span(low: u32, high: u32, start: u32, end: u32) -> Option<(u32, u32)> {
    if high < start {
        return Some((low, high));
    }
    if low > end {
        let count = end - start + 1;
        return Some((low - count, high - count));
    }
    let new_low = if low < start { low } else { start };
    let new_high = if high > end { high - (end - start + 1) } else { start - 1 };
    (new_high >= new_low).then_some((new_low, new_high))
}

/// Last valid coordinate on the edited axis.
fn axis_max(axis: Axis) -> u32 {
    match axis {
        Axis::Row => MAX_ROW,
        Axis::Column => MAX_COLUMN,
    }
}

/// Formats a rectangle as its A1 text for error messages.
fn reference_text(range: &CellRange) -> String {
    format!(
        "{}:{}",
        document_formats::a1::format_reference(range.start_row, range.start_column),
        document_formats::a1::format_reference(range.end_row, range.end_column)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn define_resolves_case_insensitively_and_replaces() {
        let mut names = NameTable::new();
        names.define("SalesTotal", "Sheet1", CellRange::between(1, 2, 3, 2)).expect("a valid name");
        assert!(names.resolve("salestotal").is_some());
        names.define("SALESTOTAL", "Sheet1", CellRange::between(2, 2, 4, 2)).expect("a valid name");
        assert_eq!(names.list().len(), 1);
        assert_eq!(names.list()[0].name, "SALESTOTAL");
        assert_eq!(names.resolve("SalesTotal").expect("registered").range.start_row, 2);
    }

    #[test]
    fn define_rejects_names_that_are_not_names() {
        let mut names = NameTable::new();
        assert!(names.define("", "Sheet1", CellRange::between(1, 1, 1, 1)).is_err());
        assert!(names.define("A1", "Sheet1", CellRange::between(1, 1, 1, 1)).is_err());
        assert!(names.define("$B$4", "Sheet1", CellRange::between(1, 1, 1, 1)).is_err());
        assert!(names.define("A1:B2", "Sheet1", CellRange::between(1, 1, 1, 1)).is_err());
        assert!(names.define("Sales", "", CellRange::between(1, 1, 1, 1)).is_err());
        assert!(names.define("Sales", "Sheet1", CellRange::between(0, 1, 1, 1)).is_err());
        assert!(names.define("Sales", "Sheet1", CellRange::between(1, 1, MAX_ROW + 1, 1)).is_err());
        assert!(names.list().is_empty());
    }

    #[test]
    fn remove_and_rename_track_entries() {
        let mut names = NameTable::new();
        names.define("Sales", "Sheet1", CellRange::between(1, 1, 2, 2)).expect("a valid name");
        names.define("Other", "Sheet1", CellRange::between(3, 3, 3, 3)).expect("a valid name");
        assert!(!names.rename("Sales", "A1"), "a reference spelling is not a name");
        assert!(!names.rename("Sales", "Other"), "a taken spelling is rejected");
        assert!(!names.rename("Missing", "Total"), "an unknown name cannot be renamed");
        assert!(names.rename("SALES", "Total"));
        assert!(names.resolve("Total").is_some());
        assert!(names.remove("TOTAL"));
        assert!(!names.remove("total"));
        assert_eq!(names.list().len(), 1);
    }

    #[test]
    fn insert_moves_and_expands_named_ranges() {
        let mut names = NameTable::new();
        names.define("Below", "Sheet1", CellRange::between(5, 2, 7, 2)).expect("a valid name");
        names.define("Across", "Sheet1", CellRange::between(2, 2, 6, 2)).expect("a valid name");
        names.define("Above", "Sheet1", CellRange::between(1, 2, 1, 2)).expect("a valid name");
        names.define("Other", "Sheet2", CellRange::between(5, 2, 7, 2)).expect("a valid name");
        names.shift(&LineEdit::insert("Sheet1", Axis::Row, 3, 2).expect("a valid edit"));
        assert_eq!(names.resolve("Below").expect("registered").range, CellRange::between(7, 2, 9, 2));
        assert_eq!(names.resolve("Across").expect("registered").range, CellRange::between(2, 2, 8, 2));
        assert_eq!(names.resolve("Above").expect("registered").range, CellRange::between(1, 2, 1, 2));
        assert_eq!(names.resolve("Other").expect("registered").range, CellRange::between(5, 2, 7, 2));
    }

    #[test]
    fn delete_shrinks_moves_and_drops_named_ranges() {
        let mut names = NameTable::new();
        names.define("Inside", "Sheet1", CellRange::between(2, 1, 3, 1)).expect("a valid name");
        names.define("Clamped", "Sheet1", CellRange::between(1, 1, 5, 1)).expect("a valid name");
        names.define("After", "Sheet1", CellRange::between(6, 1, 8, 1)).expect("a valid name");
        names.define("Before", "Sheet1", CellRange::between(1, 1, 1, 1)).expect("a valid name");
        names.shift(&LineEdit::delete("Sheet1", Axis::Row, 2, 2).expect("a valid edit"));
        assert!(names.resolve("Inside").is_none(), "a range inside the band loses its name");
        assert_eq!(names.resolve("Clamped").expect("registered").range, CellRange::between(1, 1, 3, 1));
        assert_eq!(names.resolve("After").expect("registered").range, CellRange::between(4, 1, 6, 1));
        assert_eq!(names.resolve("Before").expect("registered").range, CellRange::between(1, 1, 1, 1));
    }

    #[test]
    fn column_edits_move_the_column_span() {
        let mut names = NameTable::new();
        names.define("Wide", "Sheet1", CellRange::between(1, 2, 3, 4)).expect("a valid name");
        names.shift(&LineEdit::delete("Sheet1", Axis::Column, 2, 1).expect("a valid edit"));
        assert_eq!(names.resolve("Wide").expect("registered").range, CellRange::between(1, 2, 3, 3));
        names.shift(&LineEdit::insert("Sheet1", Axis::Column, 1, 1).expect("a valid edit"));
        assert_eq!(names.resolve("Wide").expect("registered").range, CellRange::between(1, 3, 3, 4));
    }

    #[test]
    fn rename_sheet_updates_entries() {
        let mut names = NameTable::new();
        names.define("Sales", "Sheet1", CellRange::between(1, 1, 2, 2)).expect("a valid name");
        names.rename_sheet("sheet1", "Data");
        assert_eq!(names.list()[0].sheet, "Data");
    }
}
