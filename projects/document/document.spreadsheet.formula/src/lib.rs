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

pub use engine::{CellKey, FUNCTION_ALIASES, FUNCTIONS, canonical_function, evaluate, find_cycles, validate};
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

    /// Single sheet of literals plus formula cells that may carry a cached value.
    #[derive(Default)]
    struct MatrixWorkbook {
        cells: HashMap<(u32, u32), (CellValue, Option<String>, Option<CellValue>)>,
    }

    impl MatrixWorkbook {
        fn set(&mut self, row: u32, column: u32, value: CellValue) -> &mut Self {
            self.cells.insert((row, column), (value, None, None));
            self
        }

        fn formula(&mut self, row: u32, column: u32, source: &str) -> &mut Self {
            self.cells.insert((row, column), (CellValue::Empty, Some(source.to_owned()), None));
            self
        }

        fn cached_formula(&mut self, row: u32, column: u32, source: &str, cached: CellValue) -> &mut Self {
            self.cells.insert((row, column), (CellValue::Empty, Some(source.to_owned()), Some(cached)));
            self
        }

        /// `B1:B4 = 1,2,2,3`, `D1:D4 = 1,1,2,2` (a tie) and `F1:F5` with a text
        /// cell inside the range; column `G` stays empty.
        fn seeded() -> Self {
            let mut book = MatrixWorkbook::default();
            for (row, value) in [(1u32, 1.0), (2, 2.0), (3, 2.0), (4, 3.0)] {
                book.set(row, 2, CellValue::number(value));
            }
            for (row, value) in [(1u32, 1.0), (2, 1.0), (3, 2.0), (4, 2.0)] {
                book.set(row, 4, CellValue::number(value));
            }
            for (row, value) in [(1u32, 1.0), (2, 2.0), (3, 2.0), (4, 3.0)] {
                book.set(row, 6, CellValue::number(value));
            }
            book.set(5, 6, CellValue::text("x"));
            book
        }
    }

    impl Workbook for MatrixWorkbook {
        fn sheet_index(&self, name: &str) -> Option<usize> {
            if name.eq_ignore_ascii_case("Sheet1") { Some(0) } else { None }
        }

        fn cell(&self, sheet: usize, row: u32, column: u32) -> Option<CellSnapshot> {
            if sheet != 0 {
                return None;
            }
            let (value, formula, cached) = self.cells.get(&(row, column))?;
            Some(CellSnapshot { value: value.clone(), has_formula: formula.is_some(), cached: cached.clone() })
        }

        fn formula_cells(&self) -> Vec<FormulaCell> {
            let mut cells: Vec<FormulaCell> = self
                .cells
                .iter()
                .filter_map(|(&(row, column), entry)| {
                    entry.1.as_ref().map(|source| FormulaCell { sheet: 0, row, column, source: source.clone() })
                })
                .collect();
            cells.sort_by_key(|cell| (cell.row, cell.column));
            cells
        }
    }

    /// Evaluates one formula cell on its own and fails when the engine had to
    /// fall back to the unsupported path.
    fn evaluate_formula(source: &str) -> CellValue {
        let mut book = MatrixWorkbook::seeded();
        book.formula(1, 1, source);
        let report = evaluate(&book);
        assert!(!report.unsupported.contains(&(0, 1, 1)), "`{source}` must reach a registered implementation");
        report.values.get(&(0, 1, 1)).cloned().expect("the formula cell computed")
    }

    /// Evaluates an alias and its canonical spelling and requires the same value.
    fn assert_alias_matches_canonical(alias: &str, canonical: &str) {
        assert_eq!(evaluate_formula(alias), evaluate_formula(canonical), "`{alias}` must evaluate as `{canonical}`");
    }

    #[test]
    fn concatenate_alias_shares_the_concat_implementation() {
        assert_eq!(evaluate_formula("CONCATENATE(\"a\",\"b\",1+1)"), CellValue::text("ab2"));
        assert_eq!(evaluate_formula("CONCATENATE(B1:B4)"), CellValue::text("1223"));
        assert_alias_matches_canonical("CONCATENATE(B1:B4)", "CONCAT(B1:B4)");
        // The engine never rewrites formula text, so literals keep their spelling.
        assert_eq!(evaluate_formula("\"CONCATENATE(\""), CellValue::text("CONCATENATE("));
        let mut book = MatrixWorkbook::seeded();
        book.formula(1, 1, "CONCATENATE(\"a\",\"b\",1+1)");
        assert_eq!(book.formula_cells()[0].source, "CONCATENATE(\"a\",\"b\",1+1)");
    }

    #[test]
    fn mode_sngl_matches_mode_with_tie_and_empty_boundaries() {
        assert_eq!(evaluate_formula("MODE.SNGL(B1:B4)"), CellValue::number(2.0));
        assert_alias_matches_canonical("MODE.SNGL(B1:B4)", "MODE(B1:B4)");
        assert_eq!(evaluate_formula("MODE.SNGL(D1:D4)"), CellValue::number(1.0), "a tie reports the smallest mode");
        assert_eq!(evaluate_formula("MODE.SNGL(B1:B2)"), CellValue::error("#N/A"), "no repeated value has no mode");
        assert_eq!(evaluate_formula("MODE.SNGL(G1:G2)"), CellValue::error("#N/A"), "an empty range has no mode");
        assert_eq!(evaluate_formula("MODE.SNGL(\"x\")"), CellValue::error("#VALUE!"), "a non-numeric scalar is rejected");
    }

    #[test]
    fn stdev_aliases_match_stdev_and_stdevp_with_sample_boundaries() {
        assert_eq!(evaluate_formula("STDEV.S(B1:B4)"), CellValue::number((2.0f64 / 3.0).sqrt()));
        assert_eq!(evaluate_formula("STDEV.P(B1:B4)"), CellValue::number(0.5f64.sqrt()));
        assert_alias_matches_canonical("STDEV.S(B1:B4)", "STDEV(B1:B4)");
        assert_alias_matches_canonical("STDEV.P(B1:B4)", "STDEVP(B1:B4)");
        assert_eq!(evaluate_formula("STDEV.S(B1:B1)"), CellValue::error("#DIV/0!"), "one sample has no degrees of freedom");
        assert_eq!(evaluate_formula("STDEV.P(B1:B1)"), CellValue::number(0.0), "one population value deviates by zero");
        assert_eq!(evaluate_formula("STDEV.S(G1:G2)"), CellValue::error("#DIV/0!"), "an empty range divides by zero");
        assert_eq!(evaluate_formula("STDEV.P(G1:G2)"), CellValue::error("#DIV/0!"), "an empty range divides by zero");
        assert_eq!(evaluate_formula("STDEV.S(\"x\")"), CellValue::error("#VALUE!"), "a non-numeric scalar is rejected");
        assert_eq!(evaluate_formula("STDEV.S(F1:F5)"), evaluate_formula("STDEV.S(B1:B4)"), "text inside a range is ignored");
    }

    #[test]
    fn var_aliases_match_var_and_varp_with_sample_boundaries() {
        assert_eq!(evaluate_formula("VAR.S(B1:B4)"), CellValue::number(2.0 / 3.0));
        assert_eq!(evaluate_formula("VAR.P(B1:B4)"), CellValue::number(0.5));
        assert_alias_matches_canonical("VAR.S(B1:B4)", "VAR(B1:B4)");
        assert_alias_matches_canonical("VAR.P(B1:B4)", "VARP(B1:B4)");
        assert_eq!(evaluate_formula("VAR.S(B1:B1)"), CellValue::error("#DIV/0!"), "one sample has no degrees of freedom");
        assert_eq!(evaluate_formula("VAR.P(B1:B1)"), CellValue::number(0.0), "one population value deviates by zero");
        assert_eq!(evaluate_formula("VAR.S(G1:G2)"), CellValue::error("#DIV/0!"), "an empty range divides by zero");
        assert_eq!(evaluate_formula("VAR.P(G1:G2)"), CellValue::error("#DIV/0!"), "an empty range divides by zero");
        assert_eq!(evaluate_formula("VAR.S(\"x\")"), CellValue::error("#VALUE!"), "a non-numeric scalar is rejected");
    }

    #[test]
    fn rank_eq_matches_rank_including_ties_and_missing_values() {
        assert_eq!(evaluate_formula("RANK.EQ(2,B1:B4)"), CellValue::number(2.0));
        assert_alias_matches_canonical("RANK.EQ(2,B1:B4)", "RANK(2,B1:B4)");
        assert_eq!(evaluate_formula("RANK.EQ(2,B2:B3)"), CellValue::number(1.0), "tied values share the best rank");
        assert_eq!(evaluate_formula("RANK.EQ(1,B1:B4)"), CellValue::number(4.0), "the default order is descending");
        assert_eq!(evaluate_formula("RANK.EQ(1,B1:B4,1)"), CellValue::number(1.0), "a non-zero order ranks ascending");
        assert_eq!(evaluate_formula("RANK.EQ(9,B1:B4)"), CellValue::error("#N/A"), "a value outside the range is not ranked");
    }

    #[test]
    fn alias_lookup_ignores_case_and_near_misses_never_fold() {
        assert_eq!(evaluate_formula("stdev.s(B1:B4)"), evaluate_formula("STDEV(B1:B4)"));
        assert_eq!(evaluate_formula("Mode.Sngl(B1:B4)"), evaluate_formula("MODE(B1:B4)"));
        let mut book = MatrixWorkbook::seeded();
        book.formula(1, 1, "CONCATENATEX(1)");
        book.cached_formula(2, 1, "CONCATENATEX(1)", CellValue::number(42.0));
        let report = evaluate(&book);
        assert!(report.unsupported.contains(&(0, 1, 1)), "a near-miss name is not an alias");
        assert_eq!(report.values.get(&(0, 1, 1)), Some(&CellValue::error("#NAME?")), "without a cache the error stands");
        assert_eq!(report.values.get(&(0, 2, 1)), Some(&CellValue::number(42.0)), "the imported cached value survives");
    }

    #[test]
    fn capability_matrix_registers_every_alias_on_a_canonical_function() {
        let registered = FUNCTION_ALIASES.iter().copied().collect::<std::collections::HashSet<_>>();
        for pair in [
            ("CONCATENATE", "CONCAT"),
            ("MODE.SNGL", "MODE"),
            ("RANK.EQ", "RANK"),
            ("STDEV.P", "STDEVP"),
            ("STDEV.S", "STDEV"),
            ("VAR.P", "VARP"),
            ("VAR.S", "VAR"),
        ] {
            assert!(registered.contains(&pair), "`{}` is missing from the alias registry", pair.0);
            assert!(FUNCTIONS.contains(&pair.1), "`{}` must stay a canonical engine function", pair.1);
            assert_eq!(canonical_function(pair.0), pair.1);
            assert_eq!(canonical_function(&pair.0.to_lowercase()), pair.1);
        }
        assert_eq!(canonical_function("SUM"), "SUM", "canonical names pass through");
        assert_eq!(canonical_function("CONCATENATEX"), "CONCATENATEX", "near misses pass through");
    }
}
