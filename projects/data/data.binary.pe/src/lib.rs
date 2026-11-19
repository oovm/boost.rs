//! `data-binary-pe` — `PE` / `COFF` / `ELF` binary format encoding.
//!
//! Current slice: `coff` object model, lightweight `elf` read/write, `pe` header
//! probing, `NativePeWriter` and the managed `PeWriter` with its `msil` model.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// `COFF` 对象文件模型与写出器。
pub mod coff;
/// `ELF64` 可执行与共享库镜像。
pub mod elf;
/// `MSIL` 文本模型与方法体编码（`CLR` `PE` 写入依赖）。
pub mod msil;
/// `PE/COFF` 解析、原生 `PE32+` 与托管 `CLR` 写出。
pub mod pe;
