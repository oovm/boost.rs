//! WebAssembly 数值。

/// 一个 wasm 数值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WasmValue {
    /// `i32`
    I32(i32),
    /// `i64`
    I64(i64),
    /// `f32`
    F32(f32),
    /// `f64`
    F64(f64),
}

impl WasmValue {
    /// 值类型名（wasm 文本口径）。
    pub fn type_name(self) -> &'static str {
        match self {
            Self::I32(_) => "i32",
            Self::I64(_) => "i64",
            Self::F32(_) => "f32",
            Self::F64(_) => "f64",
        }
    }

    /// 取 `i32` 值。
    pub fn as_i32(self) -> Option<i32> {
        match self {
            Self::I32(value) => Some(value),
            _ => None,
        }
    }

    /// 取 `i64` 值。
    pub fn as_i64(self) -> Option<i64> {
        match self {
            Self::I64(value) => Some(value),
            _ => None,
        }
    }
}
