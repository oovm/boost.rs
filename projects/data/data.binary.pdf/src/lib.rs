//! PDF object model, parser, stream filters and writer.
//!
//! This crate is the byte-level half of the PDF capability: it parses a PDF 1.7
//! file into its live object table and page tree, decodes Flate, LZW, ASCIIHex,
//! ASCII85 and RunLength streams, and serialises a fresh object table back into
//! a complete file with a classic cross-reference table. Cross-reference
//! streams and object streams are understood; encryption and image codecs are
//! refused instead of being guessed.
//!
//! Page-level operations — merge, split, arrange, stamp, text and image
//! extraction — build on this crate in `document-pdf`. Every malformed input
//! becomes a [`PdfError`]; no entry point panics.

#![warn(missing_docs, rustdoc::missing_crate_level_docs)]

pub mod error;
pub mod filters;
pub mod object;
pub mod parse;
pub mod writer;

pub use error::{PdfError, Result};
pub use object::{Dict, PdfObject, PdfStream, encode_text};
pub use parse::{Bookmark, FormField, PdfDocument, PdfPage, decode_text_string, parse_document, resolve_object};
pub use writer::{
    PdfWriter, copy_info, copy_info_fields, copy_object, copy_page, copy_page_with_rotation, media_box_object,
};
