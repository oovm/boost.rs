//! Helper utilities for common operations in the text parsing framework.
//!
//! This module provides utility functions for file system operations
//! and other common tasks that are useful when working with the parsing framework.

use crate::errors::TextError;

use crate::source::SourceText;

use std::fs::File;

/// Reads source text from a file path.
pub fn source_from_path(path: &std::path::Path) -> Result<SourceText, TextError> {
    match std::fs::read_to_string(path) {
        Ok(o) => Ok(SourceText::new(o)),
        Err(e) => Err(TextError::custom_error(format!("failed to read file {}: {}", path.display(), e))),
    }
}

/// Opens a file and returns a file handle.
pub fn open_file(path: &std::path::Path) -> Result<File, TextError> {
    match File::open(path) {
        Ok(o) => Ok(o),
        Err(e) => Err(TextError::custom_error(format!("failed to open file {}: {}", path.display(), e))),
    }
}

/// Creates a file and its parent directories.
pub fn create_file(path: &std::path::Path) -> Result<std::fs::File, TextError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TextError::custom_error(e.to_string()))?
    }
    std::fs::File::create(path).map_err(|e| TextError::custom_error(e.to_string()))
}
