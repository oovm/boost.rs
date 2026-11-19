use std::{
    fs::{self, FileType, Metadata},
    io,
    path::{Path, PathBuf},
};

/// A path discovered while recursively walking a folder tree.
///
/// On Windows the folder listing already carries the attributes, so every entry below the root
/// holds its metadata without a second syscall. Elsewhere metadata is read on demand, so a walk
/// that only needs the shape of the tree never stats.
#[derive(Debug, Clone)]
pub struct Entry {
    path: PathBuf,
    depth: usize,
    file_type: FileType,
    metadata: Option<Metadata>,
}

impl Entry {
    /// Returns the full path of this entry.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the depth of this entry relative to the walk root.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Returns the file name of this entry.
    pub fn file_name(&self) -> &std::ffi::OsStr {
        self.path.file_name().unwrap_or_default()
    }

    /// Returns the file type of this entry.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// Returns whether this entry is a folder.
    pub fn is_folder(&self) -> bool {
        self.file_type.is_dir()
    }

    /// Returns whether this entry is a file.
    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }

    /// Returns whether this entry is a symlink.
    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    /// Metadata of the entry as the walk classified it.
    ///
    /// A followed link reports the target's metadata. Every other entry is reported without
    /// following links, so the result matches [`Entry::file_type`]. An entry that carries no
    /// captured attributes is read back from its path.
    pub fn metadata(&self) -> io::Result<Metadata> {
        match &self.metadata {
            Some(metadata) => Ok(metadata.clone()),
            None => fs::symlink_metadata(&self.path),
        }
    }

    pub(crate) fn new(path: PathBuf, depth: usize, file_type: FileType, metadata: Option<Metadata>) -> Self {
        Self { path, depth, file_type, metadata }
    }
}
