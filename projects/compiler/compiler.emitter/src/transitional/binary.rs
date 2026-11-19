//! 二进制格式模型（过渡 `vcc-data` → `acorn-*`）。
//!
//! JVM `class` / `JAR` 已迁入 `data-binary-jvm`。
//! `COFF` / `ELF` / `PE`（含托管 `CLR` 写出）已迁入 `data-binary-pe`。

pub use data_binary_jvm::{class, jar};
pub use data_binary_pe::{coff, elf, pe};
