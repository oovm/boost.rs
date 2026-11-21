//! Failure type shared with the byte-level PDF engine.

pub use data_binary_pdf::error::{
    PdfError as ServiceError, Result, argument, exists_error, invalid, read_error, unsupported, write_error,
};
