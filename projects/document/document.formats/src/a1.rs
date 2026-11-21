//! A1 notation shared by spreadsheet tooling.
//!
//! The SpreadsheetML importer keeps its own parser because it resolves the same
//! references the reader writes back. Products that maintain a spreadsheet model
//! (formula editors, range pickers, inspectors) need the identical rules, so this
//! module exposes them with the spreadsheet contract instead of a copy per crate:
//! a one-based row and column, `$` markers ignored, and a single reference treated
//! as a one-cell range.

use crate::import::xlsx::reference;

/// Parses one cell reference such as `A1` or `$B$4` into a one-based `(row, column)`.
#[must_use]
pub fn parse_reference(text: &str) -> Option<(u32, u32)> {
    reference::parse_reference(text)
}

/// Parses `A1:B4` into `((start_row, start_column), (end_row, end_column))`.
///
/// A single reference becomes a one-cell range. Corners are normalised, so
/// `B4:A1` and `A1:B4` describe the same rectangle.
#[must_use]
pub fn parse_range(text: &str) -> Option<((u32, u32), (u32, u32))> {
    let range = reference::parse_range(text)?;
    Some(((range.start_row, range.start_column), (range.end_row, range.end_column)))
}

/// Parses column letters such as `AB` into a one-based column index.
#[must_use]
pub fn column_number(letters: &str) -> Option<u32> {
    reference::parse_column(letters)
}

/// Formats a one-based column index as letters, so `27` reads `AA`.
#[must_use]
pub fn column_name(column: u32) -> String {
    reference::column_letters(column)
}

/// Formats a one-based row and column as `A1`.
#[must_use]
pub fn format_reference(row: u32, column: u32) -> String {
    reference::reference(row, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_round_trip_through_letters() {
        assert_eq!(column_name(1), "A");
        assert_eq!(column_name(27), "AA");
        assert_eq!(column_name(16384), "XFD");
        assert_eq!(column_number("XFD"), Some(16384));
        assert_eq!(format_reference(4, 2), "B4");
        assert_eq!(parse_reference("$B$4"), Some((4, 2)));
    }

    #[test]
    fn ranges_report_normalised_corners() {
        assert_eq!(parse_range("B2:D5"), Some(((2, 2), (5, 4))));
        assert_eq!(parse_range("D5:B2"), Some(((2, 2), (5, 4))));
        assert_eq!(parse_range("C3"), Some(((3, 3), (3, 3))));
        assert_eq!(parse_range("C3:"), None);
    }
}
