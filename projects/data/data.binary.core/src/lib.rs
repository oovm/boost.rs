//! `data-binary-core`: address spaces, byte extents, layout contracts, parse
//! budgets, and bit-level cursors for the `data.binary` capability family.
//!
//! This crate freezes address spaces, spans, partial semantics, node states,
//! and parse budgets. It does not perform I/O or format-specific decoding.
//!
//! Type identity boundary: positions here live inside session-local address
//! spaces (`File`, `View`, `Decoded`, `Virtual`, `Object`) declared by a binary
//! adapter, and every range is relative to its space. They are layout-domain
//! coordinates, not source identities. `boost-source` owns the shared
//! `SourceRef`, revision, and `ByteRange` substrate for the raw bytes of one
//! snapshot; decoded, virtual, and object spaces have no backing snapshot at
//! all. The layout provenance in this crate records the container-to-member
//! transformation chain and stays in this domain for the same reason. The
//! start-plus-length `ByteExtent` is deliberately distinct from
//! `boost-source`'s start-end `ByteRange`.
//!
//! Budgets ([`Budget`], [`BudgetTracker`]) and node states ([`NodeState`]) are
//! the authoritative binary-side home. Codec-specific limits and stream states
//! in `data-compression` and `data-encoding` describe algorithm constraints
//! (window, expansion ratio, decoder lifecycle) and are not isomorphic with
//! session resource accounting, so no shared misc crate is introduced.
//!
//! Structured diagnostics reuse the shared `boost-harness` diagnostic core;
//! this crate only adds the stable `binary.*` domain codes and builders.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![deny(missing_docs)]

/// Bit-level input and output primitives over finite buffers.
pub mod bit;

mod address;
mod budget;
pub mod diagnostic;
mod layout;
mod partial;
mod reference;
mod span;
mod state;
mod write_plan;

pub use address::{Address, AddressSpaceId, AddressSpaceKind, ByteExtent, ByteExtentError};
pub use budget::{Budget, BudgetError, BudgetTracker};
pub use layout::{LayoutGraph, LayoutNode, LayoutNodeId, ReferenceEdge, evidence_absolute};
pub use partial::{Availability, NeedRange};
pub use reference::{JumpError, ReferenceEvidence, ReferenceKind, VisitKey, checked_absolute_jump, checked_relative_jump};
pub use span::{LayoutSpan, Provenance, ProvenanceStep};
pub use state::NodeState;
pub use write_plan::{Endian, LayoutPlan, PatchPoint, PlanError, PlanSlot, PlanSlotId, Relocation, RelocationId, RelocationTarget};
