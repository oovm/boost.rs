//! `data-binary-jvm` — JVM `class` / `JAR` binary format encoding.
//!
//! `class` holds the `JVM class` file model and codec starting at the
//! `0xCAFEBABE` magic, covering the constant pool, type descriptors, members and
//! access flags. `jar` holds the `JAR` packaging model; container read/write
//! goes through `data-binary-zip` and output is ZIP32 stored members. Only byte
//! structure and encoding are owned; JVM verification, linking and execution
//! semantics live elsewhere, and adjacent formats such as `DEX` are separate.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// JVM `class` 文件模型与编解码。
pub mod class;
/// JVM `JAR` 归档打包。
pub mod jar;
