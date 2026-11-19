use crate::language::CSV_LANG;
/// A CSV lexer.
pub type CsvLexer = data_text_dsv::lexer::DsvLexer<CSV_LANG>;
