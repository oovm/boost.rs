//! `data-binary-x64` — x86-64 instruction encoding and native host images.
//!
//! `x86_64` holds the `X64Instruction` IR, the two-pass encoder with fixup
//! back-patching, `Reg64` / `ConditionCode`, MSVC / SysV calling-convention
//! builders and the `EncodedModule` → `COFF` object layout. `native_pe` arranges
//! encoded `.text` / `.rdata` and the import table into a `PE32+` executable
//! image; `native_elf` arranges them into an `ELF64` `ET_EXEC` or AArch64
//! `ET_DYN` shared library. Only machine-code bytes and container orchestration
//! are owned here; the `PE` / `COFF` / `ELF` container writers live in
//! `data-binary-pe`.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// 本 crate 的错误类型与 `Result` 别名。
pub mod error;
/// x86-64 原生 `ELF64` 可执行镜像构建。
pub mod native_elf;
/// x86-64 原生 `PE32+` 可执行镜像构建。
pub mod native_pe;
/// x86-64 指令编码、ABI 与 `COFF` 布局。
pub mod x86_64;

macro_rules! err {
    ($($arg:tt)*) => {
        $crate::error::Error::new(format!($($arg)*))
    };
}

pub(crate) use err;
