//! Moves A1 references when whole rows or columns are inserted or deleted.
//!
//! A formula is scanned as text, so the rewrite keeps every other character
//! verbatim. String literals, error literals, function names, defined names and
//! worksheet qualifiers stay untouched as text; only references that point at
//! the edited sheet move. References to other sheets, to another workbook or to
//! a 3-D sheet span are left alone. A single reference removed by a deletion
//! becomes `#REF!` instead of being dropped, and a range whose ends both fall
//! inside the deleted band collapses to `#REF!` the same way.
//!
//! Absolute anchors keep their `$` spelling and still move, matching the
//! spreadsheet behaviour where inserting a row shifts `$B$4` to `$B$5`.

use document_formats::a1::{column_name, column_number};

/// Last row of an XLSX sheet.
const MAX_ROW: u32 = 1_048_576;
/// Last column of an XLSX sheet, `XFD`.
const MAX_COLUMN: u32 = 16_384;

/// Axis one structural edit moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Rows move; columns stay.
    Row,
    /// Columns move; rows stay.
    Column,
}

/// How one structural edit moves an axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditDirection {
    /// Lines are inserted before the position.
    Insert,
    /// Lines starting at the position are deleted.
    Delete,
}

/// One whole-row or whole-column edit on one sheet.
///
/// The constructors validate the band, so a built edit always stays inside the
/// grid: `position` is at least `1`, `count` is at least `1` and the last
/// affected line does not pass the axis maximum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEdit {
    /// Sheet the edit applies to; only its references move.
    pub sheet: String,
    /// Axis the edit moves.
    pub axis: Axis,
    /// One-based first affected line: the insertion point or the first deletion.
    pub position: u32,
    /// Number of lines inserted or deleted.
    pub count: u32,
    /// Whether the edit inserts or deletes.
    pub direction: EditDirection,
}

impl LineEdit {
    /// Builds an insert of `count` lines before `at`, or `None` when the band
    /// is empty or would pass the grid.
    pub fn insert(sheet: impl Into<String>, axis: Axis, at: u32, count: u32) -> Option<Self> {
        Self::new(sheet.into(), axis, at, count, EditDirection::Insert)
    }

    /// Builds a delete of `count` lines from `start`, or `None` when the band
    /// is empty or would pass the grid.
    pub fn delete(sheet: impl Into<String>, axis: Axis, start: u32, count: u32) -> Option<Self> {
        Self::new(sheet.into(), axis, start, count, EditDirection::Delete)
    }

    fn new(sheet: String, axis: Axis, position: u32, count: u32, direction: EditDirection) -> Option<Self> {
        if count == 0 || position == 0 {
            return None;
        }
        if position.checked_add(count - 1)? > axis_max(axis) {
            return None;
        }
        Some(Self { sheet, axis, position, count, direction })
    }

    /// Last line the edit touches; only meaningful for a deletion.
    pub(crate) fn delete_end(&self) -> u32 {
        self.position + self.count - 1
    }
}

/// Rewrites the references of one formula after a row or column edit.
///
/// `host_sheet` names the sheet the formula lives on, which decides whether an
/// unqualified reference is local: it moves only when the host is the edited
/// sheet. A qualified reference moves only when its qualifier names the edited
/// sheet. The returned text keeps the leading `=`, if any, and every character
/// that is not a movable reference verbatim.
pub fn shift_formula(source: &str, host_sheet: &str, edit: &LineEdit) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '"' {
            index = copy_quoted(&chars, index, '"', &mut out);
            continue;
        }
        if character == '[' {
            index = copy_external(&chars, index, &mut out);
            continue;
        }
        if let Some((qualifier, after)) = parse_qualifier(&chars, index) {
            let left_local = qualifier.name.eq_ignore_ascii_case(&edit.sheet);
            out.push_str(&qualifier.raw);
            if let Some(group) = parse_group(&chars, after) {
                let local = left_local && right_is_local(&group, edit);
                emit_group(&chars, &group, local, edit, &mut out);
                index = group.end;
            }
            else {
                index = after;
            }
            continue;
        }
        if character == '\'' {
            index = copy_quoted(&chars, index, '\'', &mut out);
            continue;
        }
        let after_name = index > 0 && is_name_char(chars[index - 1]);
        if !after_name && let Some(group) = parse_group(&chars, index) {
            let local = host_sheet.eq_ignore_ascii_case(&edit.sheet) && right_is_local(&group, edit);
            emit_group(&chars, &group, local, edit, &mut out);
            index = group.end;
            continue;
        }
        out.push(character);
        index += 1;
    }
    out
}

/// Rewrites every worksheet qualifier that names `old` to spell `new` instead.
///
/// Qualifiers are matched ignoring ASCII case, both bare (`Sheet1!`) and
/// quoted (`'My Sheet'!`). The new name is emitted in the minimal safe
/// spelling: bare when it cannot be mistaken for a reference, quoted otherwise.
/// String literals, external-workbook references and defined names that merely
/// look like a sheet name stay verbatim.
pub fn rename_sheet(source: &str, old: &str, new: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '"' {
            index = copy_quoted(&chars, index, '"', &mut out);
            continue;
        }
        if character == '[' {
            index = copy_external(&chars, index, &mut out);
            continue;
        }
        if let Some((qualifier, after)) = parse_qualifier(&chars, index) {
            if qualifier.name.eq_ignore_ascii_case(old) {
                out.push_str(&render_qualifier(new));
            }
            else {
                out.push_str(&qualifier.raw);
            }
            index = after;
            continue;
        }
        if character == '\'' {
            index = copy_quoted(&chars, index, '\'', &mut out);
            continue;
        }
        out.push(character);
        index += 1;
    }
    out
}

/// Writes one reference group, or its verbatim source text when it stays put.
fn emit_group(chars: &[char], group: &Group, local: bool, edit: &LineEdit, out: &mut String) {
    if local {
        match rewrite_group(group, edit) {
            Transformed::Unchanged => out.extend(chars[group.start..group.end].iter()),
            Transformed::Text(text) => out.push_str(&text),
            Transformed::Deleted => out.push_str("#REF!"),
        }
    }
    else {
        out.extend(chars[group.start..group.end].iter());
    }
}

/// Copies a double-quoted string literal, including doubled quote escapes.
fn copy_quoted(chars: &[char], start: usize, delimiter: char, out: &mut String) -> usize {
    let mut index = start + 1;
    while index < chars.len() {
        if chars[index] == delimiter {
            if chars.get(index + 1) == Some(&delimiter) {
                index += 2;
                continue;
            }
            index += 1;
            break;
        }
        index += 1;
    }
    let end = index.min(chars.len());
    out.extend(chars[start..end].iter());
    end
}

/// Copies an external-workbook or structured-reference bracket verbatim.
fn copy_external(chars: &[char], start: usize, out: &mut String) -> usize {
    let mut index = start;
    while index < chars.len() && chars[index] != ']' {
        index += 1;
    }
    if index < chars.len() {
        index += 1;
    }
    out.extend(chars[start..index].iter());
    if let Some((qualifier, after)) = parse_qualifier(chars, index) {
        out.push_str(&qualifier.raw);
        if let Some(group) = parse_group(chars, after) {
            out.extend(chars[group.start..group.end].iter());
            return group.end;
        }
        return after;
    }
    index
}

/// Whether a character continues a defined name, sheet name or number.
fn is_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '.' || character == '\\'
}

/// Skips whitespace between a name and a following `(`.
fn skip_spaces(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    index
}

/// A worksheet qualifier such as `Sheet2!` or `'My Sheet'!`.
#[derive(Clone)]
struct Qualifier {
    /// Unquoted sheet name the qualifier spells.
    name: String,
    /// Source text of the qualifier, including the trailing `!`.
    raw: String,
}

/// Which kind of reference a token is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RefKind {
    /// A cell such as `B4`.
    Cell,
    /// A whole column such as `B`, valid only as a range end.
    Column,
    /// A whole row such as `4`, valid only as a range end.
    Row,
}

/// One lexed A1 reference.
#[derive(Clone, Copy)]
struct RefToken {
    kind: RefKind,
    column: u32,
    row: u32,
    absolute_column: bool,
    absolute_row: bool,
    end: usize,
}

/// One single reference or one `start:end` range.
struct Group {
    start: usize,
    end: usize,
    left: RefToken,
    right: Option<(Option<Qualifier>, RefToken)>,
}

/// Outcome of moving one reference or range.
enum Transformed {
    /// The reference does not move.
    Unchanged,
    /// The reference moves to the given text.
    Text(String),
    /// The reference was deleted and becomes `#REF!`.
    Deleted,
}

/// Outcome of moving one line on the edited axis.
enum PointShift {
    Kept,
    Moved(u32),
    Removed,
}

/// Parses a worksheet qualifier ending in `!`.
fn parse_qualifier(chars: &[char], start: usize) -> Option<(Qualifier, usize)> {
    match *chars.get(start)? {
        '\'' => {
            let mut index = start + 1;
            let mut name = String::new();
            loop {
                let character = *chars.get(index)?;
                if character == '\'' {
                    if chars.get(index + 1) == Some(&'\'') {
                        name.push('\'');
                        index += 2;
                    }
                    else {
                        index += 1;
                        break;
                    }
                }
                else {
                    name.push(character);
                    index += 1;
                }
            }
            if chars.get(index) != Some(&'!') {
                return None;
            }
            let raw: String = chars[start..=index].iter().collect();
            Some((Qualifier { name, raw }, index + 1))
        }
        character if character.is_ascii_alphabetic() || character == '_' || character == '\\' => {
            let mut index = start;
            while index < chars.len() && is_name_char(chars[index]) {
                index += 1;
            }
            if chars.get(index) != Some(&'!') {
                return None;
            }
            let name: String = chars[start..index].iter().collect();
            let raw: String = chars[start..=index].iter().collect();
            Some((Qualifier { name, raw }, index + 1))
        }
        _ => None,
    }
}

/// Parses one cell, column or row reference token.
fn parse_ref(chars: &[char], start: usize) -> Option<RefToken> {
    let mut index = start;
    let absolute_column = chars.get(index) == Some(&'$');
    if absolute_column {
        index += 1;
    }
    match *chars.get(index)? {
        first if first.is_ascii_alphabetic() => {
            let letters_start = index;
            while index < chars.len() && chars[index].is_ascii_alphabetic() {
                index += 1;
            }
            let letters: String = chars[letters_start..index].iter().collect();
            if letters.len() > 3 {
                return None;
            }
            let column = column_number(&letters)?;
            let absolute_row = chars.get(index) == Some(&'$');
            if absolute_row {
                index += 1;
            }
            if chars.get(index).is_some_and(|character| character.is_ascii_digit()) {
                let row_start = index;
                while index < chars.len() && chars[index].is_ascii_digit() {
                    index += 1;
                }
                if index - row_start > 7 {
                    return None;
                }
                let row: u32 = chars[row_start..index].iter().collect::<String>().parse().ok()?;
                if !(1..=MAX_ROW).contains(&row) {
                    return None;
                }
                if chars.get(index).is_some_and(|character| is_name_char(*character)) {
                    return None;
                }
                // `LOG10(...)` is a function name, not the cell `LOG10`.
                if chars.get(skip_spaces(chars, index)) == Some(&'(') {
                    return None;
                }
                Some(RefToken { kind: RefKind::Cell, column, row, absolute_column, absolute_row, end: index })
            }
            else {
                if chars.get(index).is_some_and(|character| is_name_char(*character)) {
                    return None;
                }
                Some(RefToken { kind: RefKind::Column, column, row: 0, absolute_column, absolute_row: false, end: index })
            }
        }
        first if first.is_ascii_digit() => {
            let row_start = index;
            while index < chars.len() && chars[index].is_ascii_digit() {
                index += 1;
            }
            if index - row_start > 7 {
                return None;
            }
            let row: u32 = chars[row_start..index].iter().collect::<String>().parse().ok()?;
            if !(1..=MAX_ROW).contains(&row) {
                return None;
            }
            if chars.get(index).is_some_and(|character| is_name_char(*character)) {
                return None;
            }
            Some(RefToken { kind: RefKind::Row, column: 0, row, absolute_column: false, absolute_row: absolute_column, end: index })
        }
        _ => None,
    }
}

/// Parses a single reference or a `reference:reference` range.
///
/// A bare column or row token is only a reference when it is the end of a
/// range, so function names and plain numbers never become references.
fn parse_group(chars: &[char], start: usize) -> Option<Group> {
    let left = parse_ref(chars, start)?;
    let mut end = left.end;
    let mut right = None;
    if chars.get(end) == Some(&':') {
        let after_colon = end + 1;
        let (qualifier, next) = match parse_qualifier(chars, after_colon) {
            Some((qualifier, next)) => (Some(qualifier), next),
            None => (None, after_colon),
        };
        if let Some(right_ref) = parse_ref(chars, next)
            && right_ref.kind == left.kind
        {
            end = right_ref.end;
            right = Some((qualifier, right_ref));
        }
    }
    if right.is_none() && left.kind != RefKind::Cell {
        return None;
    }
    Some(Group { start, end, left, right })
}

/// Whether the right end of a group stays on the edited sheet.
///
/// A missing right qualifier inherits the left one, so an unqualified right end
/// is local whenever the left end is. This helper is only consulted after the
/// left end was found local.
fn right_is_local(group: &Group, edit: &LineEdit) -> bool {
    match &group.right {
        Some((Some(qualifier), _)) => qualifier.name.eq_ignore_ascii_case(&edit.sheet),
        _ => true,
    }
}

/// Whether a reference kind moves along the edited axis.
fn applies(kind: RefKind, axis: Axis) -> bool {
    matches!((kind, axis), (RefKind::Cell, _) | (RefKind::Column, Axis::Column) | (RefKind::Row, Axis::Row))
}

/// Coordinate of a reference on the edited axis.
fn coordinate(reference: &RefToken, axis: Axis) -> u32 {
    match axis {
        Axis::Row => reference.row,
        Axis::Column => reference.column,
    }
}

/// Last valid coordinate on the edited axis.
fn axis_max(axis: Axis) -> u32 {
    match axis {
        Axis::Row => MAX_ROW,
        Axis::Column => MAX_COLUMN,
    }
}

/// Maps one line through the edit.
fn shift_point(value: u32, edit: &LineEdit) -> PointShift {
    match edit.direction {
        EditDirection::Insert => {
            if value >= edit.position {
                match value.checked_add(edit.count) {
                    Some(moved) if moved <= axis_max(edit.axis) => PointShift::Moved(moved),
                    _ => PointShift::Removed,
                }
            }
            else {
                PointShift::Kept
            }
        }
        EditDirection::Delete => {
            let end = edit.delete_end();
            if value < edit.position {
                PointShift::Kept
            }
            else if value > end {
                PointShift::Moved(value - edit.count)
            }
            else {
                PointShift::Removed
            }
        }
    }
}

/// Rewrites a single reference or a range.
fn rewrite_group(group: &Group, edit: &LineEdit) -> Transformed {
    match &group.right {
        None => rewrite_single(&group.left, edit),
        Some(_) => rewrite_range(group, edit),
    }
}

/// Rewrites one single reference.
fn rewrite_single(reference: &RefToken, edit: &LineEdit) -> Transformed {
    if !applies(reference.kind, edit.axis) {
        return Transformed::Unchanged;
    }
    match shift_point(coordinate(reference, edit.axis), edit) {
        PointShift::Kept => Transformed::Unchanged,
        PointShift::Moved(value) => Transformed::Text(render_ref(reference, edit.axis, value)),
        PointShift::Removed => Transformed::Deleted,
    }
}

/// Rewrites a two-ended range, clamping its ends when a deletion cuts it.
fn rewrite_range(group: &Group, edit: &LineEdit) -> Transformed {
    let left = group.left;
    let Some((qualifier, right)) = group.right.as_ref()
    else {
        return Transformed::Unchanged;
    };
    if !applies(left.kind, edit.axis) {
        return Transformed::Unchanged;
    }
    let first = coordinate(&left, edit.axis);
    let second = coordinate(right, edit.axis);
    let (low, high) = (first.min(second), first.max(second));
    let (new_low, new_high, changed) = match edit.direction {
        EditDirection::Insert => {
            let at = edit.position;
            let count = edit.count;
            let move_line = |value: u32| if value >= at { value + count } else { value };
            let new_low = move_line(low);
            let new_high = move_line(high);
            (new_low, new_high, new_low != low || new_high != high)
        }
        EditDirection::Delete => {
            let start = edit.position;
            let end = edit.delete_end();
            let count = edit.count;
            if high < start {
                (low, high, false)
            }
            else if low > end {
                (low - count, high - count, true)
            }
            else {
                let new_low = if low < start { low } else { start };
                let new_high = if high > end { high - count } else { start - 1 };
                if new_high < new_low {
                    return Transformed::Deleted;
                }
                (new_low, new_high, new_low != low || new_high != high)
            }
        }
    };
    if new_high > axis_max(edit.axis) {
        return Transformed::Deleted;
    }
    if !changed {
        return Transformed::Unchanged;
    }
    let (first_value, second_value) = if first <= second { (new_low, new_high) } else { (new_high, new_low) };
    let mut text = render_ref(&left, edit.axis, first_value);
    text.push(':');
    if let Some(qualifier) = qualifier {
        text.push_str(&qualifier.raw);
    }
    text.push_str(&render_ref(right, edit.axis, second_value));
    Transformed::Text(text)
}

/// Formats one reference with a replacement coordinate on the edited axis.
fn render_ref(reference: &RefToken, axis: Axis, value: u32) -> String {
    let column = if axis == Axis::Column { value } else { reference.column };
    let row = if axis == Axis::Row { value } else { reference.row };
    match reference.kind {
        RefKind::Cell => format!(
            "{}{}{}{}",
            if reference.absolute_column { "$" } else { "" },
            column_name(column),
            if reference.absolute_row { "$" } else { "" },
            row
        ),
        RefKind::Column => format!("{}{}", if reference.absolute_column { "$" } else { "" }, column_name(column)),
        RefKind::Row => format!("{}{}", if reference.absolute_row { "$" } else { "" }, row),
    }
}

/// Formats a sheet qualifier that needs no quoting, quoting it when it does.
fn render_qualifier(name: &str) -> String {
    let bare = !name.is_empty() && name.chars().all(is_name_char) && !looks_like_reference(name);
    if bare {
        format!("{name}!")
    }
    else {
        let mut text = String::from("'");
        for character in name.chars() {
            if character == '\'' {
                text.push('\'');
            }
            text.push(character);
        }
        text.push_str("'!");
        text
    }
}

/// Whether the whole text parses as one A1 reference token.
fn looks_like_reference(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    parse_ref(&chars, 0).is_some_and(|token| token.end == chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_row(source: &str, at: u32, count: u32) -> String {
        let edit = LineEdit::insert("Sheet1", Axis::Row, at, count).expect("a valid edit");
        shift_formula(source, "Sheet1", &edit)
    }

    fn delete_row(source: &str, start: u32, count: u32) -> String {
        let edit = LineEdit::delete("Sheet1", Axis::Row, start, count).expect("a valid edit");
        shift_formula(source, "Sheet1", &edit)
    }

    fn insert_column(source: &str, at: u32, count: u32) -> String {
        let edit = LineEdit::insert("Sheet1", Axis::Column, at, count).expect("a valid edit");
        shift_formula(source, "Sheet1", &edit)
    }

    fn delete_column(source: &str, start: u32, count: u32) -> String {
        let edit = LineEdit::delete("Sheet1", Axis::Column, start, count).expect("a valid edit");
        shift_formula(source, "Sheet1", &edit)
    }

    #[test]
    fn edits_outside_the_grid_are_rejected() {
        assert!(LineEdit::insert("Sheet1", Axis::Row, 0, 1).is_none());
        assert!(LineEdit::insert("Sheet1", Axis::Row, 1, 0).is_none());
        assert!(LineEdit::delete("Sheet1", Axis::Row, MAX_ROW, 2).is_none());
        assert!(LineEdit::insert("Sheet1", Axis::Column, MAX_COLUMN, 1).is_some());
    }

    #[test]
    fn inserting_rows_moves_cells_at_and_after_the_point() {
        assert_eq!(insert_row("=A1+A2", 2, 1), "=A1+A3");
        assert_eq!(insert_row("=B4", 1, 2), "=B6");
        assert_eq!(insert_row("=$B$4", 1, 1), "=$B$5");
        assert_eq!(insert_row("=B$4+$B4", 2, 1), "=B$5+$B5");
    }

    #[test]
    fn inserting_rows_expands_ranges_that_cross_the_point() {
        assert_eq!(insert_row("=SUM(A1:A3)", 2, 2), "=SUM(A1:A5)");
        assert_eq!(insert_row("=SUM(A2:A3)", 2, 1), "=SUM(A3:A4)");
        assert_eq!(insert_row("=SUM(A1:A2)", 5, 1), "=SUM(A1:A2)");
    }

    #[test]
    fn deleting_rows_marks_references_inside_the_band() {
        assert_eq!(delete_row("=A5", 2, 2), "=A3");
        assert_eq!(delete_row("=A2", 2, 2), "=#REF!");
        assert_eq!(delete_row("=$A$2", 2, 2), "=#REF!");
    }

    #[test]
    fn deleting_rows_clamps_ranges() {
        assert_eq!(delete_row("=SUM(A1:A5)", 3, 2), "=SUM(A1:A3)");
        assert_eq!(delete_row("=SUM(A5:A9)", 3, 4), "=SUM(A3:A5)");
        assert_eq!(delete_row("=SUM(A1:A9)", 3, 4), "=SUM(A1:A5)");
        assert_eq!(delete_row("=SUM(A3:A4)", 3, 2), "=SUM(#REF!)");
        assert_eq!(delete_row("=SUM(A2:A3)", 5, 2), "=SUM(A2:A3)");
    }

    #[test]
    fn inserting_and_deleting_columns_use_the_same_rules() {
        assert_eq!(insert_column("=B1+C1", 2, 1), "=C1+D1");
        assert_eq!(insert_column("=SUM(B2:C3)", 3, 2), "=SUM(B2:E3)");
        assert_eq!(delete_column("=C1", 1, 2), "=A1");
        assert_eq!(delete_column("=SUM(A1:C1)", 2, 1), "=SUM(A1:B1)");
        assert_eq!(delete_column("=SUM(B1:C1)", 1, 3), "=SUM(#REF!)");
    }

    #[test]
    fn strings_error_literals_and_function_names_stay_verbatim() {
        assert_eq!(insert_row(r#"="A1"&A2"#, 1, 1), r#"="A1"&A3"#);
        assert_eq!(insert_row("=LOG10(A1)", 1, 1), "=LOG10(A2)");
        assert_eq!(insert_row("=#REF!+A1", 1, 1), "=#REF!+A2");
        assert_eq!(insert_row("=MyName+A1", 1, 1), "=MyName+A2");
    }

    #[test]
    fn other_sheets_and_external_workbooks_do_not_move() {
        assert_eq!(insert_row("=Sheet2!A1", 1, 1), "=Sheet2!A1");
        assert_eq!(insert_row("=Sheet1!A1", 1, 1), "=Sheet1!A2");
        assert_eq!(insert_row("=[Book.xlsx]Sheet1!A1", 1, 1), "=[Book.xlsx]Sheet1!A1");
        assert_eq!(insert_row("=Sheet1:Sheet3!A1", 1, 1), "=Sheet1:Sheet3!A1");
        assert_eq!(insert_row("=A1:Sheet2!B2", 1, 1), "=A1:Sheet2!B2");
    }

    #[test]
    fn quoted_sheet_names_move_only_for_the_edited_sheet() {
        let edit = LineEdit::insert("My Sheet", Axis::Row, 1, 1).expect("a valid edit");
        assert_eq!(shift_formula("='My Sheet'!A1+A1", "My Sheet", &edit), "='My Sheet'!A2+A2");
        assert_eq!(insert_row("='My Sheet'!A1", 1, 1), "='My Sheet'!A1");
    }

    #[test]
    fn unqualified_references_on_other_sheets_do_not_move() {
        let edit = LineEdit::insert("Sheet1", Axis::Row, 1, 1).expect("a valid edit");
        assert_eq!(shift_formula("=A1", "Sheet2", &edit), "=A1");
        assert_eq!(shift_formula("=Sheet1!A1", "Sheet2", &edit), "=Sheet1!A2");
    }

    #[test]
    fn whole_column_and_row_ranges_move_on_their_own_axis() {
        assert_eq!(insert_column("=SUM(A:A)", 1, 1), "=SUM(B:B)");
        assert_eq!(insert_column("=SUM($A:$A)", 2, 1), "=SUM($A:$A)");
        assert_eq!(insert_row("=SUM(1:1)", 1, 2), "=SUM(3:3)");
        assert_eq!(insert_row("=SUM(A:A)", 1, 1), "=SUM(A:A)");
    }

    #[test]
    fn formulas_without_references_are_untouched() {
        assert_eq!(insert_row("=1+2", 1, 1), "=1+2");
        assert_eq!(insert_row("=SUM(1,2,3)", 1, 1), "=SUM(1,2,3)");
    }

    #[test]
    fn renaming_a_sheet_rewrites_only_its_qualifiers() {
        assert_eq!(rename_sheet("=Sheet1!A1+Sheet2!B2", "Sheet1", "Data"), "=Data!A1+Sheet2!B2");
        assert_eq!(rename_sheet("=SHEET1!A1", "Sheet1", "Data"), "=Data!A1");
        assert_eq!(rename_sheet("='My Sheet'!A1:A2", "My Sheet", "Data"), "=Data!A1:A2");
        assert_eq!(rename_sheet("=Sheet1!A1", "Sheet1", "My Data"), "='My Data'!A1");
        assert_eq!(rename_sheet("=Sheet1!A1", "Sheet1", "AB12"), "='AB12'!A1");
        assert_eq!(rename_sheet(r#"="Sheet1!A1""#, "Sheet1", "Data"), r#"="Sheet1!A1""#);
        assert_eq!(rename_sheet("=[Book.xlsx]Sheet1!A1", "Sheet1", "Data"), "=[Book.xlsx]Sheet1!A1");
        assert_eq!(rename_sheet("=MyName+A1", "MyName", "Data"), "=MyName+A1");
    }
}
