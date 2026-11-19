use crate::language::TSV_LANG;

/// A TSV root.
pub type TsvRoot = data_text_dsv::ast::DsvRoot<TSV_LANG>;
/// A TSV record.
pub type TsvRecord = data_text_dsv::ast::DsvRecord<TSV_LANG>;
/// A TSV field.
pub type TsvField = data_text_dsv::ast::DsvField<TSV_LANG>;
