//! Wasm 私有库导出 ABI 载体词汇。
//!
//! 载体是 **Wasm 物理投影**的冻结事实：它必须与 `nyar.library_invoke` 消费的
//! `WasmAbiType` 槽位一一对应，不是跨目标中立的“标量/句柄”分类。其它 lane
//! 拥有自己的载体投影与本 lane 合同，不得复用本表。
//!
//! 导出 ABI 的冻结与参数身份校验由 [`crate::lowering::backends::wasm::input`]
//! 的私有派生点唯一实现；本模块只定义载体枚举与冻结 ABI 结构，供私有词汇
//! 与语言侧交付事实共同消费，不读取任何跨目标公共 IR 载荷。

/// Wasm 库导出参数或返回值的物理载体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WasmAbiCarrier {
    /// `i32` 槽位。
    I32,
    /// `i64` 槽位。
    I64,
    /// `f32` 槽位。
    F32,
    /// `f64` 槽位。
    F64,
    /// 宿主句柄（数组 / Option / Tuple / 具名聚合）。
    Handle,
    /// 无返回值。
    Void,
}

impl WasmAbiCarrier {
    /// 返回载体名称（lowercase）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Handle => "handle",
            Self::Void => "void",
        }
    }
}

/// 一个库导出实例的冻结 Wasm ABI 载体合同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmLibraryExportAbi {
    /// 按声明顺序排列的参数载体。
    pub params: Vec<WasmAbiCarrier>,
    /// 返回值载体；无返回值时为 [`WasmAbiCarrier::Void`]。
    pub returns: WasmAbiCarrier,
}
