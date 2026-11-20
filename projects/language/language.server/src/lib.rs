#![feature(new_range_api)]
#![recursion_limit = "512"]
#![warn(missing_docs)]
#![doc = "Language Server Protocol (LSP) implementation for `data.text` languages."]

/// Structured diagnostic conversion for LSP clients.
pub mod diagnostic;
/// Code folding contract shared by language implementations.
pub mod folding;
/// Handlers for LSP requests and notifications.
pub mod handlers;
/// Hover information contract shared by language implementations.
pub mod hover;
/// `Lsp` contract that dispatches protocol messages to `data.text` language capabilities.
pub mod lsp;
/// Definition/reference navigation contracts and helpers.
pub mod navigation;
/// LSP protocol messages, methods and capabilities.
pub mod protocol;
/// Semantic tokens contract shared by language implementations.
pub mod semantic_tokens;
/// Language service trait and utilities.
pub mod service;
/// Structure view contract shared by language implementations.
pub mod structural_view;
/// Transport adapters and the driver loop, orthogonal to server providers.
pub mod transport;
/// LSP-specific type definitions.
pub mod types;
/// Workspace and file management for LSP.
pub mod workspace;

pub use data_text_core::vfs::{DocumentSession, FileMetadata, FileType, MemoryVfs, Vfs};
pub use handlers::*;
pub use lsp::Lsp;
pub use protocol::*;
pub use service::LanguageService;
pub use transport::{Driver, DriverError, Inbound, MessageTransport, Outbound, RequestId};
pub use types::*;
pub use workspace::WorkspaceManager;
