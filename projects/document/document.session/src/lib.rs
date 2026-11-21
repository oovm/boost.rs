//! Process-local document handle table.
//!
//! Every document product exposes the same lifecycle: an opaque handle, a
//! monotonic revision bumped by each committed mutation, and a bounded undo
//! stack that never reaches the saved container. This crate owns that
//! lifecycle so products keep only their own state types and operation
//! schemas.
//!
//! ```text
//! static STORE: LazyLock<Store<Entry>> = LazyLock::new(Store::new);
//!
//! let handle = STORE.insert(entry);
//! STORE.with_mut(handle, |session| {
//!     let before = session.state().clone();
//!     session.state_mut().graph.bump_revision();
//!     session.commit(before);
//! });
//! ```

#![warn(missing_docs)]

use std::{
    collections::HashMap,
    fmt,
    ops::{Deref, DerefMut},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

pub use document_ir::DocumentGraph;

/// Default in-memory undo depth.
pub const DEFAULT_HISTORY_LIMIT: usize = 50;

/// State whose revision is monotonic across committed mutations.
///
/// Undo restores the content of a snapshot but never restores its revision:
/// an undone document stays distinguishable from the one it replaced, so a
/// stale revision check still rejects a stale client.
pub trait Revisioned {
    /// Current revision.
    fn revision(&self) -> u64;
    /// Overwrites the revision.
    fn set_revision(&mut self, revision: u64);
}

impl Revisioned for DocumentGraph {
    fn revision(&self) -> u64 {
        self.revision
    }

    fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

/// Why one session operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionErrorKind {
    /// The handle is not open.
    NotFound,
    /// Fewer snapshots are available than the requested undo depth.
    UndoUnavailable,
    /// Fewer snapshots are available than the requested redo depth.
    RedoUnavailable,
}

/// Failure of one session operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    /// Failure category.
    pub kind: SessionErrorKind,
    /// Human-readable reason.
    pub message: String,
}

impl SessionError {
    /// Builds a failure.
    pub fn new(kind: SessionErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionError {}

/// Undo and redo availability with the depth of each stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct History {
    /// Number of snapshots undo can restore.
    pub undo_depth: usize,
    /// Number of snapshots redo can replay.
    pub redo_depth: usize,
}

impl History {
    /// Whether one undo step is available.
    pub fn can_undo(self) -> bool {
        self.undo_depth > 0
    }

    /// Whether one redo step is available.
    pub fn can_redo(self) -> bool {
        self.redo_depth > 0
    }
}

/// One open document: its state plus the bounded undo and redo stacks.
#[derive(Debug)]
pub struct Record<T> {
    state: T,
    undo: Vec<T>,
    redo: Vec<T>,
    limit: usize,
}

impl<T> Record<T> {
    /// Wraps one state with the default history depth.
    pub fn new(state: T) -> Self {
        Self::with_history_limit(state, DEFAULT_HISTORY_LIMIT)
    }

    /// Wraps one state with an explicit history depth.
    pub fn with_history_limit(state: T, limit: usize) -> Self {
        Self { state, undo: Vec::new(), redo: Vec::new(), limit }
    }

    /// Borrows the state.
    pub fn state(&self) -> &T {
        &self.state
    }

    /// Mutably borrows the state.
    pub fn state_mut(&mut self) -> &mut T {
        &mut self.state
    }

    /// Unwraps the state, discarding history.
    pub fn into_state(self) -> T {
        self.state
    }

    /// Undo and redo availability.
    pub fn history(&self) -> History {
        History { undo_depth: self.undo.len(), redo_depth: self.redo.len() }
    }

    /// Borrows one record for an edit that may commit, undo or redo.
    pub fn session(&mut self) -> Session<'_, T> {
        Session { record: self }
    }
}

impl<T: Revisioned + Clone> Record<T> {
    /// Records one committed mutation: the prior state joins the undo stack and
    /// the redo path is discarded, because the future it described is gone.
    pub fn commit(&mut self, before: T) {
        push_capped(&mut self.undo, before, self.limit);
        self.redo.clear();
    }

    /// Restores the state `steps` mutations back and bumps the revision once.
    pub fn undo(&mut self, steps: usize) -> Result<(), SessionError> {
        if steps == 0 || steps > self.undo.len() {
            return Err(SessionError::new(
                SessionErrorKind::UndoUnavailable,
                format!("cannot undo {steps} step(s); {} available", self.undo.len()),
            ));
        }
        let revision = self.state.revision();
        for _ in 0..steps {
            let snapshot = self.undo.pop().expect("the step count was checked against the stack");
            push_capped(&mut self.redo, std::mem::replace(&mut self.state, snapshot), self.limit);
        }
        self.state.set_revision(revision + 1);
        Ok(())
    }

    /// Replays `steps` undone mutations and bumps the revision once.
    pub fn redo(&mut self, steps: usize) -> Result<(), SessionError> {
        if steps == 0 || steps > self.redo.len() {
            return Err(SessionError::new(
                SessionErrorKind::RedoUnavailable,
                format!("cannot redo {steps} step(s); {} available", self.redo.len()),
            ));
        }
        let revision = self.state.revision();
        for _ in 0..steps {
            let snapshot = self.redo.pop().expect("the step count was checked against the stack");
            push_capped(&mut self.undo, std::mem::replace(&mut self.state, snapshot), self.limit);
        }
        self.state.set_revision(revision + 1);
        Ok(())
    }

    /// Whether an undo snapshot is available.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether a redo snapshot is available.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

fn push_capped<T>(stack: &mut Vec<T>, snapshot: T, limit: usize) {
    if limit > 0 && stack.len() == limit {
        stack.remove(0);
    }
    stack.push(snapshot);
}

/// Borrowed view of one open document.
///
/// Deref reaches the product state, so `session.graph` keeps working; the
/// history methods stay on the view and cannot be mixed up with fields.
#[derive(Debug)]
pub struct Session<'a, T> {
    record: &'a mut Record<T>,
}

impl<'a, T> Session<'a, T> {
    /// Borrows one record.
    pub fn new(record: &'a mut Record<T>) -> Self {
        Self { record }
    }

    /// Immutable access to the product state.
    pub fn state(&self) -> &T {
        self.record.state()
    }

    /// Mutable access to the product state.
    pub fn state_mut(&mut self) -> &mut T {
        self.record.state_mut()
    }

    /// Immutable access to the underlying record.
    pub fn record(&self) -> &Record<T> {
        self.record
    }

    /// Mutable access to the underlying record.
    pub fn record_mut(&mut self) -> &mut Record<T> {
        self.record
    }
}

impl<T> Deref for Session<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.record.state()
    }
}

impl<T> DerefMut for Session<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.record.state_mut()
    }
}

impl<T: Revisioned + Clone> Session<'_, T> {
    /// Records one committed mutation.
    pub fn commit(&mut self, before: T) {
        self.record.commit(before);
    }

    /// Restores the state `steps` mutations back.
    pub fn undo(&mut self, steps: usize) -> Result<(), SessionError> {
        self.record.undo(steps)
    }

    /// Replays `steps` undone mutations.
    pub fn redo(&mut self, steps: usize) -> Result<(), SessionError> {
        self.record.redo(steps)
    }

    /// Whether an undo snapshot is available.
    pub fn can_undo(&self) -> bool {
        self.record.can_undo()
    }

    /// Whether a redo snapshot is available.
    pub fn can_redo(&self) -> bool {
        self.record.can_redo()
    }
}

/// Process-local handle table.
///
/// One store instance serves one state type. Products hold it in a
/// `LazyLock<Store<_>>`, so the handle space, locking and poisoning policy are
/// shared while the state type stays theirs.
#[derive(Debug)]
pub struct Store<T> {
    entries: Mutex<Option<HashMap<u64, Record<T>>>>,
    next: AtomicU64,
    limit: usize,
}

impl<T> Default for Store<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Store<T> {
    /// Builds an empty store with the default history depth.
    pub fn new() -> Self {
        Self::with_history_limit(DEFAULT_HISTORY_LIMIT)
    }

    /// Builds an empty store with an explicit history depth.
    pub fn with_history_limit(limit: usize) -> Self {
        Self { entries: Mutex::new(None), next: AtomicU64::new(1), limit }
    }

    fn entries(&self) -> MutexGuard<'_, Option<HashMap<u64, Record<T>>>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Opens one handle.
    pub fn insert(&self, state: T) -> u64 {
        let handle = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries().get_or_insert_with(HashMap::new).insert(handle, Record::with_history_limit(state, self.limit));
        handle
    }

    /// Runs `work` against one open handle without taking a second lock.
    pub fn with<R>(&self, handle: u64, work: impl FnOnce(&T) -> R) -> Option<R> {
        self.entries().as_ref().and_then(|table| table.get(&handle)).map(|record| work(record.state()))
    }

    /// Runs `work` against one mutable open handle.
    pub fn with_mut<R>(&self, handle: u64, work: impl FnOnce(&mut Record<T>) -> R) -> Option<R> {
        self.entries().as_mut().and_then(|table| table.get_mut(&handle)).map(work)
    }

    /// Runs `work` against one borrowed session view.
    pub fn with_session<R>(&self, handle: u64, work: impl FnOnce(&mut Session<'_, T>) -> R) -> Option<R> {
        self.with_mut(handle, |record| work(&mut record.session()))
    }

    /// Removes one handle; returns whether it was live.
    pub fn remove(&self, handle: u64) -> bool {
        self.entries().as_mut().and_then(|table| table.remove(&handle)).is_some()
    }

    /// Whether one handle is open.
    pub fn contains(&self, handle: u64) -> bool {
        self.entries().as_ref().is_some_and(|table| table.contains_key(&handle))
    }

    /// Number of open handles.
    pub fn len(&self) -> usize {
        self.entries().as_ref().map_or(0, HashMap::len)
    }

    /// Whether no handle is open.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{DocumentGraph, Revisioned, SessionErrorKind, Store};
    use document_ir::DocumentId;

    fn graph() -> DocumentGraph {
        DocumentGraph::new(DocumentId(1))
    }

    #[test]
    fn handles_are_monotonic_and_close_is_idempotent() {
        let store = Store::new();
        let first = store.insert(graph());
        let second = store.insert(graph());
        assert!(second > first);
        assert!(store.remove(first));
        assert!(!store.remove(first));
        assert!(store.contains(second));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn undo_restores_content_but_keeps_revision_moving() {
        let store = Store::new();
        let handle = store.insert(graph());
        store.with_session(handle, |session| {
            let before = session.state().clone();
            session.state_mut().bump_revision();
            session.commit(before);
            let before = session.state().clone();
            session.state_mut().bump_revision();
            session.commit(before);
        });
        store.with_session(handle, |session| {
            session.undo(2).expect("two snapshots");
            assert_eq!(session.state().revision(), 3);
            assert!(session.can_redo());
            assert!(!session.can_undo());
            session.redo(1).expect("one redo");
            assert_eq!(session.state().revision(), 4);
        });
    }

    #[test]
    fn undo_past_the_stack_is_rejected_without_changing_state() {
        let store = Store::new();
        let handle = store.insert(graph());
        store.with_session(handle, |session| {
            let error = session.undo(1).expect_err("no snapshot");
            assert_eq!(error.kind, SessionErrorKind::UndoUnavailable);
            assert_eq!(session.state().revision(), 0);
        });
    }

    #[test]
    fn history_is_bounded() {
        let store = Store::with_history_limit(2);
        let handle = store.insert(graph());
        store.with_session(handle, |session| {
            for _ in 0..5 {
                let before = session.state().clone();
                session.state_mut().bump_revision();
                session.commit(before);
            }
            assert_eq!(session.record().history().undo_depth, 2);
            session.undo(2).expect("two snapshots");
            assert_eq!(session.state().revision(), 6);
        });
    }

    #[test]
    fn unknown_handle_reports_none() {
        let store: Store<DocumentGraph> = Store::new();
        assert!(store.with(7, |_| ()).is_none());
        assert!(store.with_mut(7, |_| ()).is_none());
    }
}
