//! 文本 ABI 的编码 × 边界 × 载体事实（backend-private）。
//!
//! 这里只保留各 wasm 宿主边界共用的文本 ABI 词汇与唯一查询点：编码身份来自
//! 声明的 `SemanticType`，载体来自该边界，物理落点不得各自重新推断。submission
//! 时代的逐值物理类别计划已随公共 IR 删除，各 lane 在自己的私有输入上冻结
//! 物理事实。

use compiler_types::SemanticType;

/// 文本 ABI 的编码身份：只来自声明的 `SemanticType`，不看函数名或包名。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalTextEncoding {
    Utf8,
    Utf16,
}

/// 文本 ABI 的宿主边界。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalTextBoundary {
    WasmJsGlue,
    WasiComponent,
}

/// 该边界上已经落地的宿主载体。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalTextCarrier {
    /// `utf8` / `utf16`：共享的 i32 JS string 句柄（`hostIntern` / `const_utf8`）。
    /// 句柄不携带编码标签，编码身份由 `PhysicalTextProjection` 记录提供。
    JsStringHandle,
    /// WASI canonical ABI：UTF-8 字节列表偏移。
    CanonicalUtf8Bytes,
}

/// 单个文本 ABI 位置的冻结事实：编码 × 边界 × 载体。
///
/// 同一载体可以被多个编码共用（JS string 句柄不携带编码标签），编码身份只存在于
/// 本记录。`WasiComponent` + `utf16` 不存在 canonical 表示，因此不产生 projection；
/// 编码在边界上没有 carrier 的形态也不产生 projection。
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalTextProjection {
    pub encoding: PhysicalTextEncoding,
    pub boundary: PhysicalTextBoundary,
    pub carrier: Option<PhysicalTextCarrier>,
}

/// 物理槽没有冻结 `utf16` 载体时的统一拒绝理由（GC 聚合槽、`WasiComponent` 边界等）。
/// `WasmJsGlue` 的 ABI 位置已冻结 `JsStringHandle`，不消费该理由。
#[doc(hidden)]
pub const UTF16_UNRESOLVED_TEXT_ABI: &str = "`utf16` 没有已声明的文本 ABI 载体：canonical ABI 的字符串是 UTF-8 字节列表，\
     该边界不存在 `utf16` 的 canonical 表示，必须由显式 UTF-16 文本 ABI 契约提供载体";

/// 文本 ABI 事实的唯一查询点：编码来自声明的 `SemanticType`，载体来自该边界。
pub fn declared_text_projection(boundary: PhysicalTextBoundary, ty: &SemanticType) -> Option<PhysicalTextProjection> {
    let encoding = match ty {
        SemanticType::Utf8 => PhysicalTextEncoding::Utf8,
        SemanticType::Utf16 => PhysicalTextEncoding::Utf16,
        _ => return None,
    };
    let carrier = match (encoding, boundary) {
        (PhysicalTextEncoding::Utf8, PhysicalTextBoundary::WasmJsGlue) => Some(PhysicalTextCarrier::JsStringHandle),
        (PhysicalTextEncoding::Utf8, PhysicalTextBoundary::WasiComponent) => Some(PhysicalTextCarrier::CanonicalUtf8Bytes),
        // `WasiComponent` 的 canonical ABI 只有 UTF-8 字节列表，没有 `utf16` 的
        // canonical 表示，因此该边界不存在 `utf16` 的文本 ABI 事实。
        (PhysicalTextEncoding::Utf16, PhysicalTextBoundary::WasiComponent) => return None,
        // `WasmJsGlue` 与 `utf8` 共用同一张 JS string 句柄表：JS 只有一种字符串
        // 类型，`utf8` 与 `utf16` 的差异只在入表前是否解码，句柄本身不携带编码标签。
        (PhysicalTextEncoding::Utf16, PhysicalTextBoundary::WasmJsGlue) => Some(PhysicalTextCarrier::JsStringHandle),
    };
    Some(PhysicalTextProjection { encoding, boundary, carrier })
}

/// 未落地文本 ABI 载体的统一拒绝理由。
pub fn unresolved_text_abi_reason(ty: &SemanticType) -> Option<&'static str> {
    match ty {
        SemanticType::Utf16 => Some(UTF16_UNRESOLVED_TEXT_ABI),
        _ => None,
    }
}

#[cfg(test)]
mod text_abi_tests {
    use super::*;

    #[test]
    fn text_abi_fact_binds_encoding_boundary_and_carrier_from_declared_type() {
        let js_utf8 = declared_text_projection(PhysicalTextBoundary::WasmJsGlue, &SemanticType::Utf8).expect("`utf8` 必须已有冻结载体");
        assert_eq!(js_utf8.encoding, PhysicalTextEncoding::Utf8);
        assert_eq!(js_utf8.boundary, PhysicalTextBoundary::WasmJsGlue);
        assert_eq!(js_utf8.carrier, Some(PhysicalTextCarrier::JsStringHandle));

        for boundary in [PhysicalTextBoundary::WasmJsGlue, PhysicalTextBoundary::WasiComponent] {
            let projection = declared_text_projection(boundary, &SemanticType::Utf16);
            match boundary {
                // `WasmJsGlue` 已冻结编码身份与共享载体，但 lowering 仍未接入该载体。
                PhysicalTextBoundary::WasmJsGlue => {
                    let projection = projection.expect("`WasmJsGlue` 的 `utf16` 编码身份必须已冻结");
                    assert_eq!(projection.encoding, PhysicalTextEncoding::Utf16);
                    assert_eq!(projection.boundary, boundary);
                    assert_eq!(projection.carrier, Some(PhysicalTextCarrier::JsStringHandle), "`utf16` 与 `utf8` 共用一个 JS string 句柄表");
                }
                // canonical ABI 没有 `utf16` 表示，该边界不存在文本 ABI 事实。
                PhysicalTextBoundary::WasiComponent => assert_eq!(projection, None),
            }
        }
        assert_eq!(unresolved_text_abi_reason(&SemanticType::Utf16), Some(UTF16_UNRESOLVED_TEXT_ABI));
        assert_eq!(unresolved_text_abi_reason(&SemanticType::Utf8), None);
        assert_eq!(declared_text_projection(PhysicalTextBoundary::WasmJsGlue, &SemanticType::Unit), None);
    }
}
