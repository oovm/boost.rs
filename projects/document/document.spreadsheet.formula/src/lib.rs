#![warn(missing_docs)]
//! Spreadsheet formula engine.
//!
//! A product supplies a [`Workbook`] view of its own model; the engine lexes,
//! parses and evaluates formulas in dependency order, and reports cycles. The
//! value and range types here are format-free so no workbook schema is forced
//! on the caller.
//!
//! ```text
//! let report = evaluate(&view);
//! let value = report.values.get(&(0, 1, 1));
//! ```

mod engine;
mod model;

use std::collections::{HashMap, HashSet};

pub use engine::{CellKey, FUNCTIONS, evaluate, find_cycles, validate};
pub use model::{CellRange, CellValue, format_number, parse_reference};

/// One formula cell the engine must evaluate.
#[derive(Debug, Clone, PartialEq)]
pub struct FormulaCell {
    /// Sheet index the cell lives on.
    pub sheet: usize,
    /// One-based row.
    pub row: u32,
    /// One-based column.
    pub column: u32,
    /// Formula source without the leading `=`.
    pub source: String,
}

/// Read-only view of one cell.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSnapshot {
    /// Stored value; a formula cell keeps its authored literal here.
    pub value: CellValue,
    /// Whether the cell carries a formula.
    pub has_formula: bool,
    /// Value the source writer cached, when present.
    pub cached: Option<CellValue>,
}

/// Workbook view the engine reads.
///
/// Implement it over the product model; the engine never mutates the workbook.
pub trait Workbook {
    /// Resolves one sheet name to its index.
    fn sheet_index(&self, name: &str) -> Option<usize>;
    /// Reads one cell.
    fn cell(&self, sheet: usize, row: u32, column: u32) -> Option<CellSnapshot>;
    /// Every formula cell in the workbook.
    fn formula_cells(&self) -> Vec<FormulaCell>;
}

/// Resolved evaluation results.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvalReport {
    /// Computed value per formula cell.
    pub values: HashMap<CellKey, CellValue>,
    /// Formula cells whose expression used an unsupported construct.
    pub unsupported: HashSet<CellKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeWorkbook {
        sheets: Vec<Vec<(u32, u32, CellValue, Option<String>)>>,
        names: Vec<String>,
    }

    impl Workbook for FakeWorkbook {
        fn sheet_index(&self, name: &str) -> Option<usize> {
            self.names.iter().position(|candidate| candidate.eq_ignore_ascii_case(name))
        }

        fn cell(&self, sheet: usize, row: u32, column: u32) -> Option<CellSnapshot> {
            let cell = self.sheets.get(sheet)?.iter().find(|cell| cell.0 == row && cell.1 == column)?;
            Some(CellSnapshot { value: cell.2.clone(), has_formula: cell.3.is_some(), cached: None })
        }

        fn formula_cells(&self) -> Vec<FormulaCell> {
            let mut cells = Vec::new();
            for (sheet, entries) in self.sheets.iter().enumerate() {
                for (row, column, _, formula) in entries {
                    if let Some(source) = formula {
                        cells.push(FormulaCell {
                            sheet,
                            row: *row,
                            column: *column,
                            source: source.trim_start_matches('=').to_owned(),
                        });
                    }
                }
            }
            cells
        }
    }

    fn workbook() -> FakeWorkbook {
        FakeWorkbook {
            names: vec!["Sheet1".to_owned(), "Data".to_owned()],
            sheets: vec![
                vec![
                    (1, 1, CellValue::number(2.0), None),
                    (1, 2, CellValue::Empty, Some("=A1*3".to_owned())),
                    (2, 1, CellValue::text("hello"), None),
                    (2, 2, CellValue::Empty, Some("=UPPER(A2)".to_owned())),
                    (3, 1, CellValue::Empty, Some("=SUM(Data!A1:A2)".to_owned())),
                ],
                vec![
                    (1, 1, CellValue::number(10.0), None),
                    (2, 1, CellValue::number(20.0), None),
                ],
            ],
        }
    }

    #[test]
    fn evaluates_arithmetic_text_and_cross_sheet_ranges() {
        let report = evaluate(&workbook());
        assert_eq!(report.values.get(&(0, 1, 2)), Some(&CellValue::number(6.0)));
        assert_eq!(report.values.get(&(0, 2, 2)), Some(&CellValue::text("HELLO")));
        assert_eq!(report.values.get(&(0, 3, 1)), Some(&CellValue::number(30.0)));
        assert!(report.unsupported.is_empty());
    }

    #[test]
    fn detects_direct_cycles() {
        let book = FakeWorkbook {
            names: vec!["Sheet1".to_owned()],
            sheets: vec![vec![
                (1, 1, CellValue::Empty, Some("=B1".to_owned())),
                (1, 2, CellValue::Empty, Some("=A1".to_owned())),
            ]],
        };
        let cycles = find_cycles(&book);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 2);
        assert_eq!(evaluate(&book).values.get(&(0, 1, 1)), Some(&CellValue::error("#CYCLE!")));
    }

    #[test]
    fn rejects_unparseable_formulas_and_reports_unknown_functions() {
        assert!(validate("SUM(A1:A2)").is_ok());
        assert!(validate("SUM(").is_err());
        let book = FakeWorkbook {
            names: vec!["Sheet1".to_owned()],
            sheets: vec![vec![(1, 1, CellValue::Empty, Some("=MYSTERY(1)".to_owned()))]],
        };
        let report = evaluate(&book);
        assert!(report.unsupported.contains(&(0, 1, 1)));
        assert_eq!(report.values.get(&(0, 1, 1)), Some(&CellValue::error("#NAME?")));
    }

    #[test]
    fn parses_a1_references() {
        assert_eq!(parse_reference("$B$4"), Some((4, 2)));
        let range = CellRange::between(2, 4, 5, 2);
        assert_eq!((range.start_row, range.end_row, range.start_column, range.end_column), (2, 5, 2, 4));
        assert_eq!(range.rows(), 4);
        assert_eq!(range.columns(), 3);
    }
}
