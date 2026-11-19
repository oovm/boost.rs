//! `data-binary-tvm` — TextVM `.tvm` regex-engine artifact format.
//!
//! Reads, writes and validates the 32-byte `TVM\0` magic header, including the
//! operation kind (`Exists` / `Find` / `Replace`), flag bits, literal prefix and
//! DFA table offsets. Only the header's binary layout is owned here; the regex
//! engine's execution semantics live elsewhere.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// TextVM `.tvm` 正则引擎产物格式。
pub mod tvm;
