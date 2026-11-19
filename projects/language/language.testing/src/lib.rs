#![feature(new_range_api)]
#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

/// Testing utilities for tree building.
pub mod building;
/// Testing utilities for lexing.
pub mod lexing;
/// Testing utilities for parsing.
pub mod parsing;

use data_text_core::{errors::TextError, source::SourceRef, source::SourceText};
use std::{fs::File, path::Path};

/// Builds a [`SourceRef`] identifying a file path, for attaching to IO errors.
fn path_source_ref(path: &Path) -> SourceRef {
    SourceRef::new("file", path.display().to_string()).unwrap_or_else(|_| SourceRef::new("file", "unknown").expect("static source ref"))
}

/// Reads source text from a file path.
///
/// Normalizes `\r\n` to `\n` so golden files stay stable across platforms
/// (e.g. Windows `core.autocrlf=true` checkouts).
pub fn source_from_path(path: &Path) -> Result<SourceText, TextError> {
    match std::fs::read_to_string(path) {
        Ok(o) => Ok(SourceText::new(o.replace("\r\n", "\n").replace('\r', "\n"))),
        Err(e) => Err(TextError::io_error(e, path_source_ref(path))),
    }
}

/// Reads JSON data from a file path.
#[cfg(feature = "serde")]
pub fn json_from_path(path: &Path) -> Result<serde_json::Value, TextError> {
    let content = std::fs::read_to_string(path).map_err(|e| TextError::io_error(e, path_source_ref(path)))?;
    serde_json::from_str(&content).map_err(|e| TextError::custom_error(e.to_string()))
}

/// Opens a file and returns a file handle.
pub fn open_file(path: &Path) -> Result<File, TextError> {
    match File::open(path) {
        Ok(o) => Ok(o),
        Err(e) => Err(TextError::io_error(e, path_source_ref(path))),
    }
}

/// Creates a file and its parent directories.
pub fn create_file(path: &Path) -> Result<std::fs::File, TextError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TextError::custom_error(e.to_string()))?
    }
    std::fs::File::create(path).map_err(|e| TextError::custom_error(e.to_string()))
}
