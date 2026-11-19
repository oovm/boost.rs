use std::path::Path;

use super::{DocumentSession, FileMetadata, FileType, RevisionTracker, Vfs, WritableVfs};
use crate::{
    Arc,
    source::{DocumentSnapshot, SourceRef, SourceText},
};
use boost_fs::vfs::Overlay;
use std::{collections::HashMap, path::PathBuf, sync::RwLock};

/// A memory-based Virtual File System backed by [`boost_fs::vfs::Overlay`].
#[derive(Default)]
pub struct MemoryVfs {
    overlay: Overlay,
    revisions: RevisionTracker,
    modified: Arc<RwLock<HashMap<Arc<str>, u64>>>,
}

impl Clone for MemoryVfs {
    fn clone(&self) -> Self {
        Self {
            overlay: Overlay::new(),
            revisions: RevisionTracker::new(),
            modified: Arc::new(RwLock::new(self.modified.read().unwrap().clone())),
        }
    }
}

impl MemoryVfs {
    /// Create a new empty MemoryVfs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the revision tracker for open documents.
    pub fn revisions(&self) -> &RevisionTracker {
        &self.revisions
    }

    /// Upsert a file's content in the memory VFS.
    pub fn write_file(&self, uri: &str, content: impl Into<Arc<str>>) {
        let uri_arc: Arc<str> = Arc::from(uri);
        let content = content.into();
        self.overlay.write(PathBuf::from(uri), content.as_bytes().to_vec());
        self.revisions.bump(uri_arc.clone());
        self.modified.write().unwrap().insert(uri_arc, Self::now());
    }

    /// Remove a file from the memory VFS.
    pub fn remove_file(&self, uri: &str) {
        self.overlay.remove(Path::new(uri));
        self.revisions.clear(uri);
        self.modified.write().unwrap().remove(uri);
    }

    fn snapshot_after_write(&self, uri: &str) -> DocumentSnapshot {
        let revision = self.revisions.get(uri).expect("revision was just assigned");
        let source = self.get_source(uri).expect("document was just written");
        DocumentSnapshot::new(source, revision)
    }

    fn now() -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
        }
        #[cfg(target_arch = "wasm32")]
        {
            0
        }
    }
}

impl DocumentSession for MemoryVfs {
    fn open_document(&self, uri: &str, text: Arc<str>) -> DocumentSnapshot {
        self.write_file(uri, text);
        self.snapshot_after_write(uri)
    }

    fn close_document(&self, uri: &str) {
        self.remove_file(uri);
    }

    fn document_snapshot(&self, uri: &str) -> Option<DocumentSnapshot> {
        let revision = self.revisions.get(uri)?;
        self.get_source(uri).map(|source| DocumentSnapshot::new(source, revision))
    }

    fn request_source(&self, uri: &str) -> Option<SourceText> {
        self.document_snapshot(uri).map(|snapshot| snapshot.source().clone()).or_else(|| self.get_source(uri))
    }
}

impl Vfs for MemoryVfs {
    type Source = SourceText;

    fn get_source(&self, uri: &str) -> Option<SourceText> {
        let content = self.overlay.read(Path::new(uri))?;
        let text = String::from_utf8(content.bytes).ok()?;
        Some(match SourceRef::new("text", uri) {
            Ok(reference) => SourceText::new_with_ref(text, reference),
            Err(_) => SourceText::new(text),
        })
    }

    fn exists(&self, uri: &str) -> bool {
        self.overlay.read(Path::new(uri)).is_some()
    }

    fn metadata(&self, uri: &str) -> Option<FileMetadata> {
        let content = self.overlay.read(Path::new(uri))?;
        let modified = self.modified.read().unwrap().get(uri).copied();
        Some(FileMetadata { file_type: FileType::File, len: content.bytes.len() as u64, modified })
    }

    fn read_dir(&self, _uri: &str) -> Option<Vec<Arc<str>>> {
        None
    }
}

impl WritableVfs for MemoryVfs {
    fn write_file(&self, uri: &str, content: Arc<str>) {
        MemoryVfs::write_file(self, uri, content);
    }

    fn remove_file(&self, uri: &str) {
        MemoryVfs::remove_file(self, uri);
    }
}
