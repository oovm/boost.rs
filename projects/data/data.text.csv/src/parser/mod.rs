use crate::language::CSV_LANG;
/// A CSV parser.
pub type CsvParser = data_text_dsv::parser::DsvParser<CSV_LANG>;
