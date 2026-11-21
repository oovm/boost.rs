//! Value and reference types the formula engine works with.
//!
//! They are deliberately format-free: a product maps its own cell model onto
//! these before evaluating, so the engine never depends on a workbook schema.

/// Resolved value of one cell or one expression result.
#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    /// Text value.
    Text {
        /// Cell text.
        text: String,
    },
    /// Numeric value.
    Number {
        /// Numeric value.
        number: f64,
    },
    /// Boolean value.
    Bool {
        /// Boolean value.
        value: bool,
    },
    /// Spreadsheet error literal such as `#N/A`.
    Error {
        /// Error literal.
        code: String,
    },
    /// No value.
    Empty,
}

impl CellValue {
    /// Builds a text value.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Builds a numeric value.
    pub fn number(number: f64) -> Self {
        Self::Number { number }
    }

    /// Builds a boolean value.
    pub fn boolean(value: bool) -> Self {
        Self::Bool { value }
    }

    /// Builds an error literal.
    pub fn error(code: impl Into<String>) -> Self {
        Self::Error { code: code.into() }
    }

    /// Whether the value carries no data.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Display text used by text projections.
    pub fn display_text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Number { number } => format_number(*number),
            Self::Bool { value } => {
                if *value {
                    "TRUE".to_owned()
                }
                else {
                    "FALSE".to_owned()
                }
            }
            Self::Error { code } => code.clone(),
            Self::Empty => String::new(),
        }
    }
}

/// Formats a number without a trailing `.0` for whole values.
pub fn format_number(number: f64) -> String {
    if !number.is_finite() {
        return number.to_string();
    }
    if number.fract() == 0.0 && number.abs() < 1.0e15 { format!("{number:.0}") } else { number.to_string() }
}

/// Inclusive one-based rectangular reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRange {
    /// First row.
    pub start_row: u32,
    /// Last row.
    pub end_row: u32,
    /// First column.
    pub start_column: u32,
    /// Last column.
    pub end_column: u32,
}

impl CellRange {
    /// Builds a range from two corners in either order.
    pub fn between(start_row: u32, start_column: u32, end_row: u32, end_column: u32) -> Self {
        Self {
            start_row: start_row.min(end_row),
            end_row: start_row.max(end_row),
            start_column: start_column.min(end_column),
            end_column: start_column.max(end_column),
        }
    }

    /// Row count.
    pub fn rows(&self) -> u32 {
        self.end_row.saturating_sub(self.start_row).saturating_add(1)
    }

    /// Column count.
    pub fn columns(&self) -> u32 {
        self.end_column.saturating_sub(self.start_column).saturating_add(1)
    }

    /// Whether both corners are non-zero and ordered.
    pub fn is_valid(&self) -> bool {
        self.start_row >= 1 && self.start_column >= 1 && self.end_row >= self.start_row && self.end_column >= self.start_column
    }
}

/// Parses a single A1 cell reference such as `$B$4` into a one-based row and column.
pub fn parse_reference(text: &str) -> Option<(u32, u32)> {
    document_formats::a1::parse_reference(text)
}
