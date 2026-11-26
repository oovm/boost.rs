//! `data-binary-container`: the shared owner of Python and installer container
//! formats.
//!
//! OpenIDA (a disassembler) and OpenCTF (a capture-the-flag workbench) are two
//! independent products that need the same facts about the same bytes: a
//! `.pyc`/`marshal` stream, a PyInstaller `CArchive` or `PYZ` archive, an
//! `NSIS` installer header, an Electron `asar` tree and the Tauri v2 asset
//! table. Each product carried its own copy of those parsers, so every fix had
//! to land twice. This crate is the single owner of the facts; the products
//! keep only their report shapes and workflow policy.
//!
//! - [`pyc`] decodes the `marshal` object graph and the 3.11 through 3.13
//!   bytecode; [`pyc_opcodes`] holds the per-version opcode tables.
//! - [`pyinstaller`] reads the `CArchive` cookie/TOC and the `PYZ` archive.
//! - [`nsis`] decodes the first header, the solid framing, the script command
//!   stream and the `File` extraction manifest.
//! - [`asar`] reads the Electron archive header and its JSON file tree.
//! - [`tauri`] walks the interleaved `(key, blob)` asset table.
//!
//! Every entry point takes the whole container image as a byte slice and
//! reports absolute offsets inside it, so a product can feed a mapped file or a
//! carved slice without a session wrapper. Failures carry a stable code and,
//! where the parser knows it, the rejected [`Span`]. Decompression is delegated
//! to `data-compression`; nothing is executed and no external process is
//! spawned.

#![warn(missing_docs)]

pub mod asar;
pub mod error;
pub mod nsis;
pub mod pyc;
pub mod pyc_opcodes;
pub mod pyinstaller;
pub mod tauri;

mod read;

pub use error::{ContainerError, Result, Span};
