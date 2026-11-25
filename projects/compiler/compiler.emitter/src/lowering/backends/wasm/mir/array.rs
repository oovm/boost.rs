//! WASM GC 数组指令绑定的唯一助记符表。
//!
//! `[wasm_operator("array.get")]` 只声明目标操作身份；助记符到**冻结计划**的映射归
//! wasm backend 所有，并在 `TargetExecutionPlan` 构造时一次性写入计划（arity 与操作
//! 身份）。数组 operator 是多步编码（`ref.cast` + `array.get`/`array.set`/
//! `array.new_default`/`array.len`），arraytype 索引随元素类型在编码期从
//! RepresentationPlan 注册表解析——与 MIR `ArrayNew`/`ArrayGet`/`ArraySet`/
//! `ArrayLength` 臂共用同一发射入口，本表不冻结任何类型索引。
//!
//! 本模块只接受下表列出的助记符，未知助记符返回 `None`，计划据此冻结为
//! `对应 lane 的 `Unfrozen` 绑定`，由 emit 结构化失败，禁止猜默认指令。

/// wasm GC 数组目标 operator 的冻结操作。
///
/// 数组 operator 不是单条 opcode：`array.get`/`array.set` 需要先按元素类型
/// `ref.cast` 到具体 arraytype，`array.new` 的 arraytype 也随元素类型变化。
/// arraytype 索引**不冻结**在计划里——它是模块装配期 RepresentationPlan 注册表
/// （`gc_array_type_indices`）的事实，与 MIR `ArrayNew`/`ArrayGet`/`ArraySet`/
/// `ArrayLength` 臂共用同一解析入口；计划只冻结 arity 与操作身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WasmGcArrayOperation {
    /// `new<T>(length) -> [T]`：按元素 arraytype 分配默认值数组。
    New,
    /// `length<T>([T]) -> usize`：`ref.cast array` 后 `array.len`。
    Length,
    /// `get<T>([T], index) -> T`：`ref.cast` 元素 arraytype 后 `array.get`。
    Get,
    /// `set<T>([T], index, T) -> unit`：`ref.cast` 元素 arraytype 后 `array.set`。
    Set,
}

/// 一个已冻结的 wasm GC 数组 operator 计划。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WasmGcArrayOperatorPlan {
    /// 冻结的实参个数；编码前必须与调用实参个数一致。
    pub(crate) arity: usize,
    /// 冻结的数组操作身份。
    pub(crate) operation: WasmGcArrayOperation,
}

const fn array_plan(arity: usize, operation: WasmGcArrayOperation) -> WasmGcArrayOperatorPlan {
    WasmGcArrayOperatorPlan { arity, operation }
}

/// 唯一数组助记符表。未知助记符返回 `None`。
pub(crate) fn freeze_wasm_array_binding(mnemonic: &str) -> Option<WasmGcArrayOperatorPlan> {
    Some(match mnemonic {
        "array.new" => array_plan(1, WasmGcArrayOperation::New),
        "array.length" => array_plan(1, WasmGcArrayOperation::Length),
        "array.get" => array_plan(2, WasmGcArrayOperation::Get),
        "array.set" => array_plan(3, WasmGcArrayOperation::Set),
        _ => return None,
    })
}
