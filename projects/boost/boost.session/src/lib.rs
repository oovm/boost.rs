#![deny(missing_docs)]
//! Shared document session skeleton.
//!
//! A domain editor keeps a [`DocumentMeta`] next to its document value, records
//! snapshots in a bounded [`History`], and resolves object handles through a
//! [`Registry`]. Saving goes through `boost-fs`, so a published file is either
//! the previous revision or the new one and never a partial write.
//!
//! The crate owns only the mechanics: it never interprets the document, and it
//! never invents a domain operation.

use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::Path,
};

use boost_fs::{OverwritePolicy, PublishOptions, publish_bytes_with_options};
use serde::{Deserialize, Serialize};

/// Identity and revision header shared by every saved document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentMeta {
    /// Document format keyword, such as `openpcb.board`.
    pub format: String,
    /// On-disk format version.
    pub version: u32,
    /// Monotonic revision counter.
    pub revision: u64,
    /// Length unit the document coordinates use, such as `mm`.
    pub units: String,
}

impl DocumentMeta {
    /// Create a document header at revision zero.
    pub fn new(format: impl Into<String>, version: u32, units: impl Into<String>) -> Self {
        Self { format: format.into(), version, revision: 0, units: units.into() }
    }

    /// Return a copy with the revision advanced by one.
    pub fn bumped(&self) -> Self {
        Self { revision: self.revision + 1, ..self.clone() }
    }
}

impl Versioned for DocumentMeta {
    fn revision(&self) -> u64 {
        self.revision
    }
}

/// A document that exposes its revision counter.
pub trait Versioned {
    /// The current revision.
    fn revision(&self) -> u64;
}

/// A bounded undo/redo stack of document snapshots.
///
/// Recording a snapshot drops the oldest entry once `limit` is reached and
/// clears the redo stack, which is the usual linear-history contract.
#[derive(Debug, Clone)]
pub struct History<T> {
    limit: usize,
    undo: VecDeque<T>,
    redo: Vec<T>,
}

impl<T> History<T> {
    /// Create an empty history that keeps at most `limit` snapshots.
    ///
    /// A zero limit is treated as one, so a recorded snapshot is never lost
    /// before it can be undone once.
    pub fn new(limit: usize) -> Self {
        Self { limit: limit.max(1), undo: VecDeque::new(), redo: Vec::new() }
    }

    /// The configured bound.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Record the state that is being replaced.
    pub fn record(&mut self, state: T) {
        self.push_undo(state);
        self.redo.clear();
    }

    /// Replace `current` with the most recent snapshot, pushing `current` onto redo.
    pub fn undo(&mut self, current: T) -> Option<T> {
        let previous = self.undo.pop_back()?;
        self.redo.push(current);
        Some(previous)
    }

    /// Replace `current` with the most recently undone state, pushing `current` onto undo.
    pub fn redo(&mut self, current: T) -> Option<T> {
        let next = self.redo.pop()?;
        self.push_undo(current);
        Some(next)
    }

    /// Whether an [`Self::undo`] call can succeed.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether a [`Self::redo`] call can succeed.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Number of retained undo snapshots.
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Number of retained redo snapshots.
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Drop both stacks.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    fn push_undo(&mut self, state: T) {
        if self.undo.len() == self.limit {
            self.undo.pop_front();
        }
        self.undo.push_back(state);
    }
}

/// An ordered handle table keyed by object identity.
///
/// Entries iterate in key order, so every export or report built from the
/// registry is deterministic.
#[derive(Debug, Clone, Default)]
pub struct Registry<T> {
    items: BTreeMap<String, T>,
}

impl<T> Registry<T> {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { items: BTreeMap::new() }
    }

    /// Number of live handles.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the registry holds no handle.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Handle identities in ascending order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.items.keys().map(String::as_str)
    }

    /// Borrow one handle value.
    pub fn get(&self, id: &str) -> Option<&T> {
        self.items.get(id)
    }

    /// Mutably borrow one handle value.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut T> {
        self.items.get_mut(id)
    }

    /// Insert or replace a handle and return the previous value.
    pub fn insert(&mut self, id: impl Into<String>, value: T) -> Option<T> {
        self.items.insert(id.into(), value)
    }

    /// Remove a handle and return it.
    pub fn remove(&mut self, id: &str) -> Option<T> {
        self.items.remove(id)
    }

    /// Iterate handles in ascending identity order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.items.iter().map(|(id, value)| (id.as_str(), value))
    }
}

impl<T> IntoIterator for Registry<T> {
    type Item = (String, T);
    type IntoIter = std::collections::btree_map::IntoIter<String, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

/// Write `bytes` to `path`, replacing an existing file atomically.
pub fn save_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let options = PublishOptions { overwrite: OverwritePolicy::Replace, ..PublishOptions::default() };
    publish_bytes_with_options(path, bytes, options)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("boost-session-{}-{unique}-{name}", std::process::id()))
    }

    #[test]
    fn meta_bumps_revision() {
        let meta = DocumentMeta::new("openpcb.board", 1, "mm");
        assert_eq!(meta.revision(), 0);
        let next = meta.bumped();
        assert_eq!(next.revision(), 1);
        assert_eq!(next.format, "openpcb.board");
        assert_eq!(next.version, 1);
    }

    #[test]
    fn history_is_bounded_and_redo_is_cleared_by_recording() {
        let mut history = History::new(2);
        history.record(1);
        history.record(2);
        history.record(3);
        assert_eq!(history.undo_len(), 2);
        assert_eq!(history.undo(4), Some(3));
        assert_eq!(history.undo(3), Some(2));
        assert_eq!(history.undo(2), None);
        assert!(history.can_redo());
        assert_eq!(history.redo(2), Some(3));
        history.record(9);
        assert!(!history.can_redo());
    }

    #[test]
    fn history_round_trips_through_undo_and_redo() {
        let mut history = History::new(4);
        history.record("a");
        history.record("b");
        let restored = history.undo("c").expect("undo");
        assert_eq!(restored, "b");
        let redone = history.redo("b").expect("redo");
        assert_eq!(redone, "c");
        assert_eq!(history.undo_len(), 2);
        assert_eq!(history.redo_len(), 0);
    }

    #[test]
    fn registry_iterates_in_identity_order() {
        let mut registry = Registry::new();
        registry.insert("b", 2);
        registry.insert("a", 1);
        registry.insert("c", 3);
        let ids: Vec<&str> = registry.ids().collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(registry.get("b"), Some(&2));
        *registry.get_mut("b").expect("handle") = 20;
        assert_eq!(registry.get("b"), Some(&20));
        assert_eq!(registry.remove("b"), Some(20));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn save_atomic_replaces_existing_content() {
        let path = temp_path("doc.json");
        save_atomic(&path, b"first").expect("first save");
        save_atomic(&path, b"second").expect("second save");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        std::fs::remove_file(&path).ok();
    }
}
