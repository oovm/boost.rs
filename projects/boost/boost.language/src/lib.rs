//! Capability entry for the language services.
//!
//! Each module re-exports the owning crate; this facade defines no types of its
//! own. Enable only the services an application needs.

#[cfg(feature = "folding")]
pub use language_folding as folding;

#[cfg(feature = "format")]
pub use language_format as format;

#[cfg(feature = "highlight")]
pub use language_highlight as highlight;

#[cfg(feature = "hover")]
pub use language_hover as hover;

#[cfg(feature = "mcp")]
pub use language_mcp as mcp;

#[cfg(feature = "navigation")]
pub use language_navigation as navigation;

#[cfg(feature = "repl")]
pub use language_repl as repl;

#[cfg(feature = "semantic-search")]
pub use language_semantic_search as semantic_search;

#[cfg(feature = "semantic-tokens")]
pub use language_semantic_tokens as semantic_tokens;

#[cfg(feature = "server")]
pub use language_server as server;

#[cfg(feature = "structural-view")]
pub use language_structural_view as structural_view;

#[cfg(feature = "testing")]
pub use language_testing as testing;

#[cfg(feature = "visualize")]
pub use language_visualize as visualize;
