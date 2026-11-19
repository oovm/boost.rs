//! EPUB import via Acorn OCF and `data-text-html` XHTML lowering.

mod assets;
/// XHTML AST to IR lowering helpers.
pub mod ast_lowering;
/// HTML query helpers shared by EPUB import.
pub mod html_extensions;
/// EPUB navigation document parsing.
pub mod navigation;
mod reader;
/// Spine XHTML import entry points.
pub mod xhtml;

pub use reader::{import_epub, import_epub_bytes};

pub(crate) fn map_ocf_error(error: data_binary_epub::OcfError) -> crate::FormatError {
    crate::FormatError::parse("epub", error.to_string())
}
