//! `data-binary-aarch64` — AArch64 instruction encoding and Android JNI glue.
//!
//! `aarch64` holds the `A64Instruction` constants, the two-pass encoder with
//! fixup, `RegX` and the AAPCS64 function builder; `aarch64::jni_glue` provides
//! the `JNI_OnLoad` / `asgard_invoke_export` glue machine code and merges the
//! glue with logical sections into an `ET_DYN` shared-library image. Only
//! machine-code bytes and exported-symbol offsets are owned here; the `ELF64`
//! `ET_DYN` container writer lives in `data-binary-pe`.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// `AArch64` 指令编码、AAPCS64 ABI 与 Android JNI 胶水。
pub mod aarch64;
/// 本 crate 的错误类型与 `Result` 别名。
pub mod error;

macro_rules! err {
    ($($arg:tt)*) => {
        $crate::error::Error::new(format!($($arg)*))
    };
}

pub(crate) use err;
