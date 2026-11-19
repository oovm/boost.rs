use std::{
    fs::{self, FileType, Metadata},
    path::{Path, PathBuf},
    rc::Rc,
};

use super::{Entry, Error, ErrorPolicy, Filter, Walker};

/// A predicate that decides what a walk yields and what it traverses.
type EntryFilter = Box<dyn FnMut(&Entry) -> Filter>;

/// Iterator over paths discovered by [`Walker`].
pub struct Iter {
    root: PathBuf,
    min_depth: usize,
    max_depth: usize,
    follow_links: bool,
    sort_by_file_name: bool,
    error_policy: ErrorPolicy,
    filter: Option<EntryFilter>,
    stack: Vec<PendingEntry>,
    started: bool,
    finished: bool,
}

impl std::fmt::Debug for Iter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Iter")
            .field("root", &self.root)
            .field("min_depth", &self.min_depth)
            .field("max_depth", &self.max_depth)
            .field("follow_links", &self.follow_links)
            .field("sort_by_file_name", &self.sort_by_file_name)
            .field("filtered", &self.filter.is_some())
            .field("pending", &self.stack.len())
            .field("finished", &self.finished)
            .finish()
    }
}

/// Folders on the branch that leads to an entry, identified after following links.
///
/// A link is only a loop when it resolves to a folder that already contains the entry, so the
/// branch is compared instead of every folder seen so far. Two sibling links to the same folder are
/// therefore not reported as a loop.
#[derive(Debug)]
struct Ancestor {
    canonical: PathBuf,
    parent: Option<Rc<Ancestor>>,
}

impl Ancestor {
    /// Returns the branch folder that `canonical` resolves to, when it is already on the branch.
    fn matching(&self, canonical: &Path) -> Option<&Path> {
        let mut current = Some(self);
        while let Some(node) = current {
            if node.canonical == canonical {
                return Some(&node.canonical);
            }
            current = node.parent.as_deref();
        }
        None
    }
}

/// An entry on the pending stack. The listing already answered the file type, and on Windows it
/// answered the attributes too, so a pending entry is small and never holds a listing handle.
#[derive(Debug)]
struct PendingEntry {
    path: PathBuf,
    depth: usize,
    ancestors: Option<Rc<Ancestor>>,
    /// Absent only for the walk root, which no listing entry describes.
    file_type: Option<FileType>,
    metadata: Option<Metadata>,
}

/// An entry the walk is ready to yield, plus the canonical folder it belongs to while links are
/// followed.
struct Resolved {
    entry: Entry,
    canonical: Option<PathBuf>,
}

impl Iter {
    pub(crate) fn new(walker: Walker) -> Self {
        Self::build(walker, None)
    }

    pub(crate) fn filtered<F>(walker: Walker, predicate: F) -> Self
    where
        F: FnMut(&Entry) -> Filter + 'static,
    {
        Self::build(walker, Some(Box::new(predicate)))
    }

    fn build(walker: Walker, filter: Option<EntryFilter>) -> Self {
        Self {
            root: walker.root,
            min_depth: walker.min_depth,
            max_depth: walker.max_depth,
            follow_links: walker.follow_links,
            sort_by_file_name: walker.sort_by_file_name,
            error_policy: walker.error_policy,
            filter,
            stack: Vec::new(),
            started: false,
            finished: false,
        }
    }

    /// Report an error according to the configured policy.
    fn failure(&mut self, error: Error) -> Option<Result<Entry, Error>> {
        if self.error_policy == ErrorPolicy::Strict {
            self.finished = true;
        }
        Some(Err(error))
    }

    fn push_children(&mut self, path: &Path, depth: usize, ancestors: Option<Rc<Ancestor>>) -> Result<(), Error> {
        if depth >= self.max_depth {
            return Ok(());
        }

        let read_dir = fs::read_dir(path).map_err(|err| Error::io(path, depth, err))?;
        let mut children = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|err| Error::io(path, depth + 1, err))?;
            let child_path = entry.path();
            if child_path.file_name().is_some_and(|name| name == "." || name == "..") {
                continue;
            }
            let file_type = entry.file_type().map_err(|err| Error::io(&child_path, depth + 1, err))?;
            // On Windows the attributes ride on the listing entry, so capturing them here is a
            // struct copy, not a syscall. Elsewhere they are read on demand by `Entry::metadata`.
            #[cfg(windows)]
            let metadata = entry.metadata().ok();
            #[cfg(not(windows))]
            let metadata = None;
            children.push(PendingEntry {
                path: child_path,
                depth: depth + 1,
                ancestors: ancestors.clone(),
                file_type: Some(file_type),
                metadata,
            });
        }

        if self.sort_by_file_name {
            children.sort_by(|left, right| left.path.file_name().cmp(&right.path.file_name()));
        }
        children.reverse();
        self.stack.extend(children);
        Ok(())
    }

    /// Classify a pending entry, and read the canonical folder it belongs to while links are
    /// followed.
    fn resolve(&self, pending: PendingEntry) -> Result<Resolved, Error> {
        let PendingEntry { path, depth, ancestors, file_type, metadata } = pending;
        // The listing carried the file type for every entry below the root. Only the root is
        // stat'ed, once, and its attributes are then captured like any other entry's.
        let (file_type, metadata) = match file_type {
            Some(file_type) => (file_type, metadata),
            None => {
                let meta = fs::symlink_metadata(&path).map_err(|err| Error::io(&path, depth, err))?;
                (meta.file_type(), Some(meta))
            }
        };
        if !self.follow_links {
            return Ok(Resolved { entry: Entry::new(path, depth, file_type, metadata), canonical: None });
        }

        // Following links is what makes a resolution ambiguous, so the canonical path is computed for
        // every folder on the branch and compared when a link is met.
        let canonical = fs::canonicalize(&path).map_err(|err| Error::io(&path, depth, err))?;
        if !file_type.is_symlink() {
            return Ok(Resolved { entry: Entry::new(path, depth, file_type, metadata), canonical: Some(canonical) });
        }

        if let Some(ancestor) = ancestors.as_deref().and_then(|branch| branch.matching(&canonical)) {
            return Err(Error::loop_at(&path, depth, ancestor.to_path_buf()));
        }

        // The followed metadata replaces the listing attributes: it describes what the walk
        // classified.
        let followed = fs::metadata(&path).map_err(|err| Error::io(&path, depth, err))?;
        Ok(Resolved { entry: Entry::new(path, depth, followed.file_type(), Some(followed)), canonical: Some(canonical) })
    }
}

impl Iterator for Iter {
    type Item = Result<Entry, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        if !self.started {
            self.started = true;
            self.stack.push(PendingEntry { path: self.root.clone(), depth: 0, ancestors: None, file_type: None, metadata: None });
        }

        while let Some(pending) = self.stack.pop() {
            let depth = pending.depth;
            let branch = pending.ancestors.clone();
            let resolved = match self.resolve(pending) {
                Ok(resolved) => resolved,
                Err(error) => return self.failure(error),
            };

            let entry = resolved.entry;
            let decision = match self.filter.as_mut() {
                Some(filter) => filter(&entry),
                None => Filter::default(),
            };

            if entry.is_folder() && decision != Filter::IgnoreFolder {
                let ancestors = match resolved.canonical {
                    Some(canonical) => Some(Rc::new(Ancestor { canonical, parent: branch })),
                    None => branch,
                };
                if let Err(error) = self.push_children(entry.path(), depth, ancestors) {
                    return self.failure(error);
                }
            }

            if decision == Filter::Continue && depth >= self.min_depth {
                return Some(Ok(entry));
            }
        }

        self.finished = true;
        None
    }
}
