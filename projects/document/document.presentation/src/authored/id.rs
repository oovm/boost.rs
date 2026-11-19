//! Stable authored identities.
//!
//! Every deck object carries an opaque identity that the engine allocates.
//! Identities are never derived from a slide index, a shape index or a file
//! name, so reordering or renaming cannot silently retarget an edit.

use serde::{Deserialize, Serialize};

macro_rules! opaque_id {
    ($name:ident, $label:literal) => {
        #[doc = concat!("Opaque ", $label, " identity.")]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Wraps an engine-allocated ", $label, " identity.")]
            ///
            /// Empty strings are rejected, because an empty identity cannot be
            /// addressed by an edit or a diagnostic.
            pub fn new(value: impl Into<String>) -> Option<Self> {
                let value = value.into();
                if value.is_empty() { None } else { Some(Self(value)) }
            }

            /// Borrows the opaque identity string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

opaque_id!(DeckId, "deck");
opaque_id!(SlideId, "slide");
opaque_id!(ElementId, "element");
opaque_id!(MasterId, "master");
opaque_id!(LayoutId, "layout");
opaque_id!(ThemeId, "theme");
opaque_id!(AssetId, "asset");

/// Monotonic allocator behind every authored identity.
///
/// The counter is persisted with the deck, so reopening a project never reuses
/// an identity that already appeared in an earlier revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdAllocator {
    next: u64,
}

impl IdAllocator {
    /// Creates an allocator whose first allocation is `1`.
    pub fn new() -> Self {
        Self { next: 0 }
    }

    /// Restores an allocator from a persisted counter.
    pub fn from_counter(next: u64) -> Self {
        Self { next }
    }

    /// Number of identities handed out so far.
    pub fn counter(&self) -> u64 {
        self.next
    }

    /// Allocates one identity with a readable prefix, such as `slide` or `element`.
    pub fn allocate(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }
}
