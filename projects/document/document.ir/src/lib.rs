#![warn(missing_docs)]
//! Notedown document semantic IR.
//!
//! Stable identities, block semantics, relations, assets, and provenance.
//! Independent from syntax tree, text parsers, Acorn binary views, and Panduck
//! writers.

mod asset;
mod block;
mod graph;
mod id;
mod inline;
mod math;
mod metadata;
mod page;
mod projection;
mod relation;
mod source;
mod status;
mod transform_errors;
mod validate;
mod walk;
mod wire;

pub use asset::{Asset, AssetKind};
pub use block::{Block, BlockNode, ListItem, TableCell, TableRow};
pub use graph::{DocumentGraph, DocumentMetadata, IdAllocator};
pub use id::{AssetId, DocumentId, LinkId, NodeId};
pub use inline::{AttributePair, Attributes, BreakKind, Inline};
pub use math::{
    MathAtom, MathClass, MathColumnAlignment, MathDelimiter, MathDelimiterKind, MathDisplay, MathEnvironment, MathEnvironmentKind,
    MathExpression, MathFormula, MathFractionLine, MathRow, MathRowNumbering, MathRowPlacement, MathSpace, MathSpan, MathStyle, MathStyleKind,
};
pub use metadata::{MetadataEntry, MetadataValue};
pub use page::PageSetup;
pub use projection::{
    DocumentStatistics, DocumentSummary, LossEntry, LossSeverity, ParagraphSummary, SearchHit, SectionSummary, TableSummary,
    WORDS_PER_MINUTE, block_address, child_ids, find, flat_order, losses, parse_block_address, plain_text, roots, statistics,
    summarize,
};
pub use relation::{Relation, RelationEndpoint, RelationKind};
pub use source::{CoverageReport, SourceKind, SourceRef, SourceSpan};
pub use status::{LossMarker, SemanticStatus};
pub use transform_errors::{Diagnostic, RewriteResult, TransformError, Traversal};
pub use validate::{ValidationIssue, ValidationReport};
pub use wire::{DocumentEnvelope, SCHEMA_VERSION, WireError};
