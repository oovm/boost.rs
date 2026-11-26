//! Mandatory wasm-gc type builders for language reference semantics on wasm/wasi.
//!
//! f32 载体合同（冻结，不可协商）：
//!
//! 1. **载体身份**：`f32` 的 valtype 是独立的 [`VALTYPE_F32`]（`0x7D`），
//!    局部/参数/返回/字段/元素槽都按真实 4 字节存放。禁止把 `f32` 装进
//!    `VALTYPE_F64` 槽（含 JS glue、GC boxing、签名驱动的搬运）——那会让
//!    「32 位类型」实际承诺 64 位载荷，位宽与 NaN payload 宽度对外失真。
//! 2. **转换规则**：`f32 → f64` 只能走 `f64.promote_f32`（精确无损）；
//!    `f64 → f32` 只能走 `f32.demote_f64`（单指令 round-to-nearest-even，
//!    溢出到 `inf`、下溢到 0 或次正规）。不存在隐式拓宽，也不允许先存别的槽再搬。
//! 3. **运算与位模式**：f32 算术/比较在单精度上做，不提升到 f64 再降回；
//!    `i32.reinterpret_f32` / `f32.reinterpret_i32` 是 32 位、`i32` 载体、
//!    23 位 NaN payload 与 `-0.0`/次正规保位。
//!
//! `f32` valtype 的身份（`0x7D`）由 `data-binary-wasm` 拥有；本模块只重导出，
//! 其余模块必须引用它，不得各自写字面量。
//!
//! 本模块只保留物理类型段构建器与 valtype 身份重导出；`SemanticType` → wasm
//! 载体的源语义裁决已删除，物理载体映射唯一归 `input.rs` 的 `WasmTypeFact` 入口。
use data_binary_wasm::{VALTYPE_ANYREF, encode_arraytype_raw, encode_structtype_raw};

/// Abbreviated `anyref` valtype byte (`data-binary-wasm` `VALTYPE_ANYREF`).
pub(crate) const WASM_GC_ANYREF: u8 = VALTYPE_ANYREF;

/// `f32` 的 valtype 字节（Wasm 标准 `0x7D`，由 `data-binary-wasm` 定义）。
pub(crate) use data_binary_wasm::VALTYPE_F32;

/// WASM-GC structtype 类型段条目。
pub(crate) fn wasm_gc_struct_type(field_types: &[u8]) -> Vec<u8> {
    encode_structtype_raw(field_types)
}

/// WASM-GC arraytype 类型段条目（GC MVP）。
pub(crate) fn wasm_gc_array_type(element_type: u8) -> Vec<u8> {
    encode_arraytype_raw(element_type)
}
