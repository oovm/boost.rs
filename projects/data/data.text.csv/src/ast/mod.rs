use crate::language::CSV_LANG;

/// A CSV root.
pub type CsvRoot = data_text_dsv::ast::DsvRoot<CSV_LANG>;
/// A CSV record.
pub type CsvRecord = data_text_dsv::ast::DsvRecord<CSV_LANG>;
/// A CSV field.
pub type CsvField = data_text_dsv::ast::DsvField<CSV_LANG>;
