use std::{
    fs::{self, DirEntry, FileType, Metadata},
    io,
    path::{Path, PathBuf},
};

/// Where the walk learned about an entry, which decides how its metadata is read back.
#[derive(Debug)]
pub(crate) enum Source {
    /// The entry came from a folder listing. On Windows the listing already carries the
    /// attributes, so [`Entry::metadata`] answers without a new syscall.
    Listing(DirEntry),
    /// The walk root, which no listing entry describes.
    Root,
    /// A link that the walk followed: the target's metadata was already read to classify it.
    Followed(Metadata),
}

/// A path discovered while recursively walking a folder tree.
///
/// An entry below the root carries its folder listing handle, so [`Entry::metadata`] reuses the
/// attributes the listing already returned instead of stat'ing the path again.
#[derive(Debug)]
pub struct Entry {
    path: PathBuf,
    depth: usize,
    file_type: FileType,
    source: Source,
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
    /// A folder listing already carries the attributes on Windows, so this costs no extra syscall
    /// for entries below the root. A followed link reports the target's metadata; every other
    /// entry is reported without following links, so the result matches [`Entry::file_type`].
    pub fn metadata(&self) -> io::Result<Metadata> {
        match &self.source {
            Source::Listing(entry) => entry.metadata(),
            Source::Followed(metadata) => Ok(metadata.clone()),
            Source::Root => fs::symlink_metadata(&self.path),
        }
    }

    pub(crate) fn new(path: PathBuf, depth: usize, file_type: FileType, source: Source) -> Self {
        Self { path, depth, file_type, source }
    }
}
