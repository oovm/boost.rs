use super::{SourceRevision, SourceText};

/// Immutable source text paired with the revision that produced it.
///
/// The revision identity itself is owned by `boost-source`; this pairing is
/// the text-snapshot view used by document sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentSnapshot {
    source: SourceText,
    revision: SourceRevision,
}

impl DocumentSnapshot {
    /// Builds a snapshot from source text and revision.
    pub fn new(source: SourceText, revision: SourceRevision) -> Self {
        Self { source, revision }
    }

    /// Returns the source text for this snapshot.
    pub fn source(&self) -> &SourceText {
        &self.source
    }

    /// Returns the revision for this snapshot.
    pub fn revision(&self) -> &SourceRevision {
        &self.revision
    }
}
