//! Wasm 目标私有结算输入：mir/ 子系统的唯一词汇来源。
//!
//! 所有权边界（见 01 设计文档 §5.5）：跨目标公共 IR（`FragmentSubmission` /
//! `TargetExecutionPlan` / `ExecutableFunction` / `SemanticType` / 布局与身份类型）
//! 只在本模块的 [`settle_from_submission`] 处被读取一次；结算之后，wasm lane
//! （`mir/` 全部子模块）只消费本模块定义的私有词汇。公共 IR 的形状变化在
//! settle 处收敛，不再向编码层扩散。
//!
//! 镜像原则：私有类型与公共类型的变体名、字段名、变体声明顺序完全一致，
//! 因此 `{:?}` / `{}` 渲染与公共类型逐字相同，编码层错误文案保持不变。
//! 身份 newtype 手写 `Debug` / `Display`，输出与公共 `SemanticId` 完全同名。

use std::{cmp::Ordering, collections::{BTreeMap, BTreeSet}, fmt};

use compiler_types::{Identifier, NamePath};

use crate::{
    FragmentSubmission, Result,
    lowering::{
        backends::wasm::abi::{WasmAbiCarrier, WasmLibraryExportAbi},
        features::physical_contract::{PhysicalTextBoundary, PhysicalTextCarrier, UTF16_UNRESOLVED_TEXT_ABI},
        shared::physical_layout::{LayoutPolicy, resolve_physical_layouts},
    },
    target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe},
};

// ── 身份 newtype ──────────────────────────────────────────────────────────

/// SSA 值引用（wasm 私有）。渲染与公共 `ValueRef` 一致：`ValueRef(N)`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmValueKey(pub u32);

impl fmt::Debug for WasmValueKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ValueRef({})", self.0)
    }
}

/// 基本块引用（wasm 私有）。渲染与公共 `BlockRef` 一致：`BlockRef(N)`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmBlockKey(pub u32);

impl fmt::Debug for WasmBlockKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlockRef({})", self.0)
    }
}

/// 布局身份（wasm 私有）。公共 `LayoutId` 即 `u32` 别名，渲染为数字。
pub(crate) type WasmLayoutKey = u32;

/// 可调用实例身份（wasm 私有）。渲染与公共 `ItemInstanceId` 一致：`ItemInstanceId(N)`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmCallableKey(pub u32);

impl fmt::Debug for WasmCallableKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ItemInstanceId({})", self.0)
    }
}

impl fmt::Display for WasmCallableKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ItemInstanceId({})", self.0)
    }
}

/// 名义实例身份（wasm 私有）。渲染与公共 `NominalInstanceId` 一致。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmNominalKey(pub u32);

impl fmt::Debug for WasmNominalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NominalInstanceId({})", self.0)
    }
}

impl fmt::Display for WasmNominalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NominalInstanceId({})", self.0)
    }
}

/// sum 变体身份（wasm 私有）。渲染与公共 `VariantId` 一致。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmVariantKey(pub u32);

impl fmt::Debug for WasmVariantKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VariantId({})", self.0)
    }
}

impl fmt::Display for WasmVariantKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VariantId({})", self.0)
    }
}

/// 字段身份（wasm 私有）。渲染与公共 `FieldId` 一致。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmFieldKey(pub u32);

impl fmt::Debug for WasmFieldKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FieldId({})", self.0)
    }
}

impl fmt::Display for WasmFieldKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FieldId({})", self.0)
    }
}

/// 指令身份（wasm 私有）。渲染与公共 `InstructionId` 一致。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmInstructionKey(pub u32);

impl fmt::Debug for WasmInstructionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstructionId({})", self.0)
    }
}

impl fmt::Display for WasmInstructionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstructionId({})", self.0)
    }
}

// ── 类型事实 ──────────────────────────────────────────────────────────────

/// IEEE-754 `f64` 的全序包装（wasm 私有镜像）。
///
/// 相等与排序同源于 `f64::total_cmp`；`Debug` 渲染与公共 `Float64Bits` 一致。
#[derive(Clone, Copy)]
pub(crate) struct WasmFloat64Bits(pub f64);

impl fmt::Debug for WasmFloat64Bits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Float64Bits").field(&self.0).finish()
    }
}

impl PartialEq for WasmFloat64Bits {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}

impl Eq for WasmFloat64Bits {}

impl PartialOrd for WasmFloat64Bits {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WasmFloat64Bits {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl std::hash::Hash for WasmFloat64Bits {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl WasmFloat64Bits {
    /// 取出原始 `f64`。
    pub(crate) fn into_inner(self) -> f64 {
        self.0
    }
}

/// 函数类型（wasm 私有镜像；变体名仍为 `Function`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmFunctionType {
    /// 形参类型，按声明顺序。
    pub params: Vec<WasmTypeFact>,
    /// 返回类型。
    pub return_type: WasmTypeFact,
}

/// 动态 trait 对象 / witness 表类型（wasm 私有镜像；变体名仍为 `TraitObject`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WasmWitnessObject {
    /// trait 身份。
    pub trait_path: Identifier,
    /// 实例化类型参数。
    pub type_arguments: Vec<WasmTypeFact>,
}

/// 平台类型事实（wasm 私有镜像）。
///
/// 变体名、字段名与声明顺序与公共 `SemanticType` 完全一致；`Debug` 手写以
/// 保持与公共派生输出逐字相同（嵌套结构名仍渲染为 `FunctionType` /
/// `WitnessObject`），`Display` 镜像公共实现的全部臂。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum WasmTypeFact {
    Bottom,
    Unit,
    Boolean,
    Integer8 { signed: bool },
    Integer16 { signed: bool },
    Integer32 { signed: bool },
    Integer64 { signed: bool },
    Integer128 { signed: bool },
    Float32,
    Float64,
    Character,
    Utf8,
    Utf16,
    Named(Identifier),
    Nominal { layout: WasmLayoutKey, spelling: Identifier },
    Apply(Box<WasmTypeFact>, Vec<WasmTypeFact>),
    Function(Box<WasmFunctionType>),
    Tuple(Vec<WasmTypeFact>),
    Array(Box<WasmTypeFact>),
    FixedArray { element: Box<WasmTypeFact>, length: usize },
    TraitObject(WasmWitnessObject),
    Nullable(Box<WasmTypeFact>),
    Union(Vec<WasmTypeFact>),
}

impl Default for WasmTypeFact {
    fn default() -> Self {
        WasmTypeFact::Unit
    }
}

impl fmt::Debug for WasmTypeFact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bottom => f.write_str("Bottom"),
            Self::Unit => f.write_str("Unit"),
            Self::Boolean => f.write_str("Boolean"),
            Self::Integer8 { signed } => f.debug_struct("Integer8").field("signed", signed).finish(),
            Self::Integer16 { signed } => f.debug_struct("Integer16").field("signed", signed).finish(),
            Self::Integer32 { signed } => f.debug_struct("Integer32").field("signed", signed).finish(),
            Self::Integer64 { signed } => f.debug_struct("Integer64").field("signed", signed).finish(),
            Self::Integer128 { signed } => f.debug_struct("Integer128").field("signed", signed).finish(),
            Self::Float32 => f.write_str("Float32"),
            Self::Float64 => f.write_str("Float64"),
            Self::Character => f.write_str("Character"),
            Self::Utf8 => f.write_str("Utf8"),
            Self::Utf16 => f.write_str("Utf16"),
            Self::Named(id) => f.debug_tuple("Named").field(id).finish(),
            Self::Nominal { layout, spelling } => f.debug_struct("Nominal").field("layout", layout).field("spelling", spelling).finish(),
            Self::Apply(base, args) => f.debug_tuple("Apply").field(base).field(args).finish(),
            Self::Function(function) => f.debug_tuple("Function").field(&FunctionTypeDebug(function)).finish(),
            Self::Tuple(items) => f.debug_tuple("Tuple").field(items).finish(),
            Self::Array(element) => f.debug_tuple("Array").field(element).finish(),
            Self::FixedArray { element, length } => f.debug_struct("FixedArray").field("element", element).field("length", length).finish(),
            Self::TraitObject(object) => f.debug_tuple("TraitObject").field(&WitnessObjectDebug(object)).finish(),
            Self::Nullable(payload) => f.debug_tuple("Nullable").field(payload).finish(),
            Self::Union(arms) => f.debug_tuple("Union").field(arms).finish(),
        }
    }
}

/// 以公共派生 `Debug` 的字段名渲染私有函数类型结构。
struct FunctionTypeDebug<'a>(&'a WasmFunctionType);

impl fmt::Debug for FunctionTypeDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionType").field("params", &self.0.params).field("return_type", &self.0.return_type).finish()
    }
}

/// 以公共派生 `Debug` 的字段名渲染私有 witness 结构。
struct WitnessObjectDebug<'a>(&'a WasmWitnessObject);

impl fmt::Debug for WitnessObjectDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WitnessObject").field("trait_path", &self.0.trait_path).field("type_arguments", &self.0.type_arguments).finish()
    }
}

impl WasmTypeFact {
    /// 抹掉诊断拼写、只保留布局/结构身份的合同键（镜像公共 `contract_key`）。
    pub(crate) fn contract_key(&self) -> WasmTypeFact {
        match self {
            WasmTypeFact::Nominal { layout, .. } => WasmTypeFact::Nominal { layout: *layout, spelling: Identifier::new("") },
            WasmTypeFact::Apply(base, arguments) => {
                WasmTypeFact::Apply(Box::new(base.contract_key()), arguments.iter().map(WasmTypeFact::contract_key).collect())
            }
            WasmTypeFact::Array(element) => WasmTypeFact::Array(Box::new(element.contract_key())),
            WasmTypeFact::FixedArray { element, length } => WasmTypeFact::FixedArray { element: Box::new(element.contract_key()), length: *length },
            WasmTypeFact::Tuple(items) => WasmTypeFact::Tuple(items.iter().map(WasmTypeFact::contract_key).collect()),
            WasmTypeFact::Nullable(element) => WasmTypeFact::Nullable(Box::new(element.contract_key())),
            WasmTypeFact::Union(items) => WasmTypeFact::Union(items.iter().map(WasmTypeFact::contract_key).collect()),
            other => other.clone(),
        }
    }
}

impl fmt::Display for WasmTypeFact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmTypeFact::Bottom => f.write_str("void"),
            WasmTypeFact::Unit => f.write_str("unit"),
            WasmTypeFact::Boolean => f.write_str("bool"),
            WasmTypeFact::Integer8 { signed: true } => f.write_str("i8"),
            WasmTypeFact::Integer8 { signed: false } => f.write_str("u8"),
            WasmTypeFact::Integer16 { signed: true } => f.write_str("i16"),
            WasmTypeFact::Integer16 { signed: false } => f.write_str("u16"),
            WasmTypeFact::Integer32 { signed: true } => f.write_str("i32"),
            WasmTypeFact::Integer32 { signed: false } => f.write_str("u32"),
            WasmTypeFact::Integer64 { signed: true } => f.write_str("i64"),
            WasmTypeFact::Integer64 { signed: false } => f.write_str("u64"),
            WasmTypeFact::Integer128 { signed: true } => f.write_str("i128"),
            WasmTypeFact::Integer128 { signed: false } => f.write_str("u128"),
            WasmTypeFact::Float32 => f.write_str("f32"),
            WasmTypeFact::Float64 => f.write_str("f64"),
            WasmTypeFact::Character => f.write_str("char"),
            WasmTypeFact::Utf8 => f.write_str("utf8"),
            WasmTypeFact::Utf16 => f.write_str("utf16"),
            WasmTypeFact::Named(name) => write!(f, "{name}"),
            WasmTypeFact::Nominal { spelling, .. } => write!(f, "{spelling}"),
            WasmTypeFact::Apply(base, args) => {
                write!(f, "{base}<")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                f.write_str(">")
            }
            WasmTypeFact::Function(func) => {
                f.write_str("fn(")?;
                for (i, param) in func.params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{param}")?;
                }
                write!(f, ") -> {}", func.return_type)
            }
            WasmTypeFact::Tuple(elems) => {
                f.write_str("(")?;
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{elem}")?;
                }
                f.write_str(")")
            }
            WasmTypeFact::Array(elem) => write!(f, "[{elem}]"),
            WasmTypeFact::FixedArray { element, length } => write!(f, "[{element}; {length}]"),
            WasmTypeFact::TraitObject(object) => {
                write!(f, "dyn {}", object.trait_path)?;
                if object.type_arguments.is_empty() {
                    return Ok(());
                }
                f.write_str("<")?;
                for (i, arg) in object.type_arguments.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                f.write_str(">")
            }
            WasmTypeFact::Nullable(payload) => write!(f, "{payload}?"),
            WasmTypeFact::Union(arms) => {
                for (i, arm) in arms.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{arm}")?;
                }
                Ok(())
            }
        }
    }
}

// ── 指令形状 ──────────────────────────────────────────────────────────────

/// 可执行常量（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WasmConstant {
    Int(i64),
    Float64(WasmFloat64Bits),
    Bool(bool),
    Utf8(String),
    Utf16(String),
    Unit,
    Null,
}

/// 指令 / 终结符操作数（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WasmOperand {
    Value(WasmValueKey),
    Constant(WasmConstant),
    Item(WasmCallableKey),
    Symbol(NamePath),
}

/// 数组槽位初始化方式（wasm 私有镜像公共 `ArrayInitialization`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WasmArrayInit {
    Default,
    Fill(WasmOperand),
}

/// 可执行指令载荷（wasm 私有镜像公共 `InstructionKind`，全部臂与字段名一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WasmOperation {
    LoadConstant { constant: WasmConstant, ty: Option<WasmTypeFact> },
    Copy { source: WasmOperand },
    Cast { source: WasmOperand, ty: WasmTypeFact },
    StoreVar { value: WasmOperand, ty: Option<WasmTypeFact> },
    Call { callee: WasmOperand, arguments: Vec<WasmOperand> },
    LoadFunctionRef { instance: WasmCallableKey, environment: Option<WasmOperand>, captured: Vec<String> },
    StructNew { nominal: WasmNominalKey, fields: Vec<(WasmFieldKey, WasmOperand)> },
    TupleNew { layout_id: WasmLayoutKey, fields: Vec<WasmOperand> },
    TupleGet { object: WasmOperand, layout_id: WasmLayoutKey, index: u32 },
    AggregateCopy { layout_id: WasmLayoutKey, source: WasmOperand, dest: WasmOperand },
    FieldGet { object: WasmOperand, field: WasmFieldKey },
    FieldSet { object: WasmOperand, field: WasmFieldKey, value: WasmOperand },
    SumNew { nominal: WasmNominalKey, variant: WasmVariantKey, payload: Option<WasmOperand> },
    SumPayloadGet { nominal: WasmNominalKey, variant: WasmVariantKey, object: WasmOperand },
    SumVariantIs { nominal: WasmNominalKey, variant: WasmVariantKey, object: WasmOperand },
    ArrayNew { array_type: WasmTypeFact, length: WasmOperand, initialization: WasmArrayInit },
    ArrayFromElements { layout_id: Option<WasmLayoutKey>, array_type: WasmTypeFact, elements: Vec<WasmOperand> },
    ArrayGet { array: WasmOperand, index: WasmOperand },
    ArraySet { array: WasmOperand, index: WasmOperand, value: WasmOperand },
    ArrayLength { array: WasmOperand },
    NullIsNull { value: WasmOperand },
    NullUnwrap { value: WasmOperand },
}

/// 可执行指令（wasm 私有镜像；`provenance` 是语言侧事实，编码层不消费，丢弃）。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WasmInstruction {
    pub id: WasmInstructionKey,
    pub results: Vec<WasmValueKey>,
    pub kind: WasmOperation,
}

impl fmt::Debug for WasmInstruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instruction").field("id", &self.id).field("results", &self.results).field("kind", &self.kind).finish()
    }
}

/// 主结果（镜像公共 `instruction_primary_result`）。
#[inline]
pub(crate) fn primary_result(instruction: &WasmInstruction) -> Option<WasmValueKey> {
    instruction.results.first().copied()
}

/// 基本块终结符（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WasmTerminator {
    Return { value: Option<WasmOperand> },
    Jump { target: WasmBlockKey, arguments: Vec<WasmOperand> },
    Branch { condition: WasmOperand, then_target: WasmBlockKey, else_target: WasmBlockKey },
    Raise { payload: Option<WasmOperand>, resume_target: WasmBlockKey },
    Unreachable,
}

/// 基本块（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmBlock {
    pub id: WasmBlockKey,
    pub label: String,
    pub parameters: Vec<WasmValueKey>,
    pub instructions: Vec<WasmInstruction>,
    pub terminator: WasmTerminator,
}

/// SSA 值定义来源（wasm 私有镜像公共 `ValueOrigin`）。
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum WasmValueOrigin {
    Parameter { index: usize, name: String },
    BlockParameter { block: WasmBlockKey, name: String },
    Literal,
    Path,
    CallResult,
    Temporary,
}

impl fmt::Debug for WasmValueOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parameter { index, name } => f.debug_struct("Parameter").field("index", index).field("name", name).finish(),
            Self::BlockParameter { block, name } => f.debug_struct("BlockParameter").field("block", block).field("name", name).finish(),
            Self::Literal => f.write_str("Literal"),
            Self::Path => f.write_str("Path"),
            Self::CallResult => f.write_str("CallResult"),
            Self::Temporary => f.write_str("Temporary"),
        }
    }
}

/// SSA 值定义（wasm 私有镜像公共 `Value`；`Debug` 渲染与公共派生输出一致）。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WasmValue {
    pub id: WasmValueKey,
    pub origin: WasmValueOrigin,
}

impl fmt::Debug for WasmValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Value").field("id", &self.id).field("origin", &self.origin).finish()
    }
}

/// 函数体（wasm 私有镜像公共 `ExecutableFunction`）。
///
/// `parameter_identities` 是从 `ValueOrigin::Parameter` 结算出的
/// `(声明下标, canonical 参数名)` 原始清单（未排序、未校验）；校验在消费点
/// 由 [`canonical_parameter_identities`] 完成，错误文案与公共实现逐字一致。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WasmFunctionInput {
    pub symbol: String,
    pub return_type: WasmTypeFact,
    pub param_types: Vec<WasmTypeFact>,
    pub value_types: BTreeMap<WasmValueKey, WasmTypeFact>,
    pub value_layouts: BTreeMap<WasmValueKey, WasmLayoutKey>,
    pub return_layout: Option<WasmLayoutKey>,
    pub entry: WasmBlockKey,
    pub values: Vec<WasmValue>,
    pub blocks: Vec<WasmBlock>,
    pub parameter_identities: Vec<(usize, String)>,
}

impl fmt::Debug for WasmFunctionInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutableFunction")
            .field("symbol", &self.symbol)
            .field("return_type", &self.return_type)
            .field("param_types", &self.param_types)
            .field("value_types", &self.value_types)
            .field("value_layouts", &self.value_layouts)
            .field("return_layout", &self.return_layout)
            .field("entry", &self.entry)
            .field("values", &self.values)
            .field("blocks", &self.blocks)
            .finish()
    }
}

/// 入口参数身份校验（wasm 私有镜像公共 `abi_structure::canonical_parameter_identities`）。
///
/// 身份必须来自 `ValueOrigin::Parameter`，数量必须与 `param_types` 一致，下标
/// 必须连续且从 0 开始，参数名必须非空且不重复；任何偏离都在此结构化失败。
pub(crate) fn canonical_parameter_identities(instance_label: &str, function: &WasmFunctionInput) -> Result<Vec<(usize, String)>> {
    let mut identities = function.parameter_identities.clone();
    if identities.len() != function.param_types.len() {
        return Err(format!(
            "库导出 `{instance_label}` 的入口参数身份数量与参数类型数量不一致: parameter_origins={} param_types={}",
            identities.len(),
            function.param_types.len()
        ));
    }
    identities.sort_by_key(|(index, _)| *index);
    let mut names = BTreeSet::new();
    for (position, (index, name)) in identities.iter().enumerate() {
        if *index != position {
            return Err(format!("库导出 `{instance_label}` 参数下标不连续: position={position} index={index}"));
        }
        if name.trim().is_empty() {
            return Err(format!("库导出 `{instance_label}` 参数 {index} 缺少 canonical 参数名"));
        }
        if !names.insert(name.clone()) {
            return Err(format!("库导出 `{instance_label}` 参数 {index} 的参数名 `{name}` 重复"));
        }
    }
    Ok(identities)
}

/// 分派形态（wasm 私有镜像公共 `DispatchKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WasmDispatchKind {
    Static,
    Indirect,
    Witness,
}

/// receiver 传参提示（wasm 私有镜像公共 `ReceiverPassingKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WasmReceiverPassingKind {
    ByValue,
    ByAddress,
}

// ── 布局 / 导入 / sum / singleton 事实 ────────────────────────────────────

/// 存储类（wasm 私有镜像公共 `StorageKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WasmStorageClass {
    Value,
    Reference,
}

impl WasmStorageClass {
    pub(crate) fn is_value(self) -> bool {
        matches!(self, Self::Value)
    }

    pub(crate) fn is_reference(self) -> bool {
        matches!(self, Self::Reference)
    }
}

/// 匿名聚合的结构键（wasm 私有镜像公共 `LayoutKey`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum WasmLayoutKeyFact {
    Tuple(Vec<WasmTypeFact>),
    FixedArray { element: Box<WasmTypeFact>, length: usize },
}

/// 聚合字段物理布局（wasm 私有镜像公共 `ResolvedFieldLayout`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmFieldFact {
    pub name: String,
    pub ty: WasmTypeFact,
    pub offset: u32,
    pub size: u32,
    pub align: u32,
}

/// 聚合物理布局（wasm 私有镜像公共 `ResolvedAggregateLayout`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmLayoutFact {
    pub id: WasmLayoutKey,
    pub name: String,
    pub namespace: String,
    pub key: Option<WasmLayoutKeyFact>,
    pub storage: WasmStorageClass,
    pub size: u32,
    pub align: u32,
    pub fields: Vec<WasmFieldFact>,
}

/// 外部导入链接（wasm 私有镜像公共 `ExternalImportLink`；`Display` 逐字一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmImportLinkFact {
    pub boundary: Identifier,
    pub platform_tag: Option<Identifier>,
    pub locator_segments: Vec<String>,
}

impl WasmImportLinkFact {
    pub(crate) fn matches_boundary(&self, boundary: &str) -> bool {
        self.boundary.as_str() == boundary
    }

    pub(crate) fn matches_platform_tag(&self, platform_tag: &str) -> bool {
        self.platform_tag.as_ref().is_some_and(|current| current.as_str() == platform_tag)
    }

    pub(crate) fn matches_host_platform(&self, platform_tag: &str) -> bool {
        self.matches_boundary("host") && self.matches_platform_tag(platform_tag)
    }

    pub(crate) fn locator_segments(&self) -> &[String] {
        &self.locator_segments
    }
}

impl fmt::Display for WasmImportLinkFact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.boundary)?;
        if let Some(platform_tag) = &self.platform_tag {
            write!(f, "[{}]", platform_tag)?;
        }
        write!(f, "(")?;
        for (index, segment) in self.locator_segments.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            write!(f, "\"{}\"", segment)?;
        }
        write!(f, ")")
    }
}

/// 后端导入签名事实（wasm 私有镜像公共 `BackendImport`；`index` 编码层不消费，省略）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmImportFact {
    pub links: Vec<WasmImportLinkFact>,
    pub parameter_types: Vec<WasmTypeFact>,
    pub return_type: WasmTypeFact,
}

/// sum 变体的平台合同（wasm 私有镜像公共 `SumVariantRepresentation`；
/// payload 只以「是否存在」被消费，`TypeId` 身份不进私有词汇）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WasmSumVariantFact {
    pub tag: u32,
    pub has_payload: bool,
}

/// sum 平台合同（wasm 私有镜像公共 `SumRepresentation`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmSumRep {
    pub nominal: WasmNominalKey,
    pub variants: BTreeMap<WasmVariantKey, WasmSumVariantFact>,
}

/// sum 变体布局（wasm 私有镜像公共 `SumVariantLayout`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmSumVariantLayoutFact {
    pub id: WasmVariantKey,
    pub name: String,
    pub tag: u32,
    pub payload_type: Option<WasmTypeFact>,
}

/// sum 判别布局（wasm 私有镜像公共 `SumTypeLayout`；`identity` 由 settle 用
/// 私有 [`sum_identity_facts`] 预结算，编码层不再现场派生）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmSumLayoutFact {
    pub nominal: WasmNominalKey,
    pub name: String,
    pub is_unite: bool,
    pub variants: Vec<WasmSumVariantLayoutFact>,
    pub identity: String,
}

/// 程序级文本载体表示（wasm 私有镜像公共 `ProgramTextRepresentation`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WasmProgramTextRepresentation {
    Raw,
    Managed,
}

/// singleton 实例生命周期事实（wasm 私有镜像公共 `SingletonInstancePlan`，
/// 并附 settle 解析出的物理尺寸；布局缺失保留为 `None`，由 lane 函数报 SMIR010）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmSingletonFact {
    pub namespace: String,
    pub name: String,
    pub instance_field: String,
    pub is_lazy: bool,
    pub constructor_symbol: Option<String>,
    pub finalizer_symbol: Option<String>,
    pub layout_size: Option<u32>,
    pub layout_align: Option<u32>,
    pub accessor_method: String,
    pub qualified_name: String,
    pub supports_unload: bool,
}

// ── ABI 结构（wasm 私有镜像公共 `AbiStructure` 族）────────────────────────

/// 目标中立的 ABI 语义结构（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WasmAbiStructure {
    Unit,
    I32,
    I64,
    F32,
    F64,
    Bool,
    Char,
    Utf8,
    Array { element: Box<WasmAbiStructure>, element_key: String },
    FixedArray { element: Box<WasmAbiStructure>, length: usize },
    Option { inner: Box<WasmAbiStructure> },
    Tuple { fields: Vec<WasmAbiStructure> },
    Aggregate(WasmAggregateStructure),
    Sequence(WasmSequenceStructure),
    InstanceHandle { layout: WasmLayoutKey, name: String, module_path: String },
    Unite {
        nominal: WasmNominalKey,
        name: String,
        identity: String,
        variants: Vec<WasmAbiVariant>,
        payloads: Vec<Option<Box<WasmAbiStructure>>>,
    },
    Unsupported { reason: String },
}

impl WasmAbiStructure {
    /// 返回结构化失败原因（若本结构不可跨库导出边界）。
    pub(crate) fn unsupported_reason(&self) -> Option<&str> {
        match self {
            Self::Unsupported { reason } => Some(reason),
            _ => None,
        }
    }
}

/// 具名聚合的结构描述（wasm 私有镜像公共 `AggregateStructure`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmAggregateStructure {
    pub layout: Option<WasmLayoutKey>,
    pub name: String,
    pub module_path: String,
    pub generic_args: Vec<WasmAbiStructure>,
    pub fields: Vec<WasmAbiField>,
}

/// 聚合字段结构（wasm 私有镜像公共 `AbiField`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmAbiField {
    pub name: String,
    pub ty: WasmAbiStructure,
}

/// sum 变体身份（wasm 私有镜像公共 `AbiVariant`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WasmAbiVariant {
    pub name: String,
    pub tag: u32,
}

/// 宿主序列投影结构（wasm 私有镜像公共 `SequenceStructure`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmSequenceStructure {
    pub layout: WasmLayoutKey,
    pub name: String,
    pub module_path: String,
    pub levels: Vec<WasmSequenceLevel>,
    pub element: Box<WasmAbiStructure>,
    pub element_key: String,
    pub length_ty: Box<WasmAbiStructure>,
}

/// 宿主序列投影单层槽位事实（wasm 私有镜像公共 `SequenceLevel`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmSequenceLevel {
    pub layout: WasmLayoutKey,
    pub field_count: usize,
    pub items_slot: Option<usize>,
    pub length_slot: Option<usize>,
    pub inner_slot: Option<usize>,
}

/// sum 身份事实：类型名、是否 unite、全部变体名与判别 tag（镜像公共实现）。
pub(crate) fn sum_identity_facts(name: &str, is_unite: bool, variants: &[WasmAbiVariant]) -> String {
    let mut facts = String::new();
    facts.push_str(name);
    facts.push('\n');
    facts.push_str(if is_unite { "unite" } else { "sum" });
    facts.push('\n');
    for variant in variants {
        facts.push_str(&format!("{}:{}\n", variant.name, variant.tag));
    }
    facts
}

/// 从冻结 sum 布局提取结构身份（镜像公共 `sum_variants_of`）。
pub(crate) fn sum_variants_of(sum: &WasmSumLayoutFact) -> Vec<WasmAbiVariant> {
    sum.variants.iter().map(|variant| WasmAbiVariant { name: variant.name.clone(), tag: variant.tag }).collect()
}

/// 类型 canonical 拼写（镜像公共 `canonical_type_spelling`）。
pub(crate) fn canonical_type_spelling(ty: &WasmTypeFact) -> String {
    format!("{ty:?}")
}

/// 布局字段是否为整数计数字段候选（镜像公共 `is_integer_layout_type`）。
fn is_integer_layout_type(ty: &WasmTypeFact) -> bool {
    matches!(
        ty,
        WasmTypeFact::Integer8 { .. } | WasmTypeFact::Integer16 { .. } | WasmTypeFact::Integer32 { .. } | WasmTypeFact::Integer64 { .. }
    )
}

/// 字段类型解析出的布局身份（镜像公共 `field_layout_id`）。
fn field_layout_id(ty: &WasmTypeFact, layout_by_type: &BTreeMap<WasmTypeFact, WasmLayoutKey>) -> Option<WasmLayoutKey> {
    match ty {
        WasmTypeFact::Nominal { layout, .. } => Some(*layout),
        other => layout_by_type.get(other).copied(),
    }
}

/// 递归深度上限（镜像公共 `ABI_STRUCTURE_MAX_DEPTH`）。
const ABI_STRUCTURE_MAX_DEPTH: usize = 32;

/// 嵌套存储层深度上限（镜像公共 `HOST_PROJECTION_MAX_DEPTH`）。
const HOST_PROJECTION_MAX_DEPTH: usize = 8;

fn identity_aggregate(name: String, module_path: String, generic_args: Vec<WasmAbiStructure>) -> WasmAbiStructure {
    WasmAbiStructure::Aggregate(WasmAggregateStructure { layout: None, name, module_path, generic_args, fields: Vec::new() })
}

/// 把类型事实派生为纯身份结构（不读取任何布局事实；镜像公共 `abi_structure_for_type`）。
pub(crate) fn abi_structure_for_type(ty: &WasmTypeFact) -> WasmAbiStructure {
    match ty {
        WasmTypeFact::Bottom => WasmAbiStructure::Unsupported { reason: "`void` / bottom 类型不能出现在库导出 ABI 位置".to_string() },
        WasmTypeFact::Unit => WasmAbiStructure::Unit,

        WasmTypeFact::Boolean => WasmAbiStructure::Bool,
        WasmTypeFact::Character => WasmAbiStructure::Char,

        WasmTypeFact::Integer8 { .. } | WasmTypeFact::Integer16 { .. } | WasmTypeFact::Integer32 { .. } => WasmAbiStructure::I32,
        WasmTypeFact::Integer64 { .. } => WasmAbiStructure::I64,
        WasmTypeFact::Integer128 { .. } => {
            WasmAbiStructure::Unsupported { reason: "`i128` 没有稳定的库导出载体，需要显式拆分合同".to_string() }
        }

        WasmTypeFact::Float32 => WasmAbiStructure::F32,
        WasmTypeFact::Float64 => WasmAbiStructure::F64,

        WasmTypeFact::Utf8 => WasmAbiStructure::Utf8,
        WasmTypeFact::Utf16 => WasmAbiStructure::Unsupported { reason: UTF16_UNRESOLVED_TEXT_ABI.to_string() },

        WasmTypeFact::Array(element) => {
            WasmAbiStructure::Array { element: Box::new(abi_structure_for_type(element)), element_key: canonical_type_spelling(element) }
        }
        WasmTypeFact::FixedArray { element, length } => {
            WasmAbiStructure::FixedArray { element: Box::new(abi_structure_for_type(element)), length: *length }
        }

        WasmTypeFact::Nullable(inner) => WasmAbiStructure::Option { inner: Box::new(abi_structure_for_type(inner)) },

        WasmTypeFact::Tuple(fields) => WasmAbiStructure::Tuple { fields: fields.iter().map(abi_structure_for_type).collect() },

        WasmTypeFact::Named(id) => identity_aggregate(crate::abi_structure::simple_name_of(id), crate::abi_structure::module_path_of(id), Vec::new()),
        WasmTypeFact::Nominal { spelling, .. } => {
            identity_aggregate(crate::abi_structure::simple_name_of(spelling), crate::abi_structure::module_path_of(spelling), Vec::new())
        }

        WasmTypeFact::Apply(base, args) => {
            let (name, module_path, base_generic_args) = match base.as_ref() {
                WasmTypeFact::Named(id) => (crate::abi_structure::simple_name_of(id), crate::abi_structure::module_path_of(id), Vec::new()),
                WasmTypeFact::Nominal { spelling, .. } => {
                    (crate::abi_structure::simple_name_of(spelling), crate::abi_structure::module_path_of(spelling), Vec::new())
                }
                WasmTypeFact::Apply(_, _) => flatten_apply_identity(base.as_ref()),
                other => {
                    return WasmAbiStructure::Unsupported {
                        reason: format!("Apply 的 Base 必须是 Named 或 Apply，实际是 {other:?}——无法提取语义 identity"),
                    };
                }
            };
            let mut generic_args: Vec<WasmAbiStructure> = base_generic_args.iter().map(abi_structure_for_type).collect();
            generic_args.extend(args.iter().map(abi_structure_for_type));
            identity_aggregate(name, module_path, generic_args)
        }

        WasmTypeFact::Function(_) => WasmAbiStructure::Unsupported { reason: "函数类型不能出现在库导出 ABI 位置".to_string() },

        WasmTypeFact::TraitObject(_) => {
            WasmAbiStructure::Unsupported { reason: "TraitObject（dynamic dispatch）没有稳定的库导出载体".to_string() }
        }

        WasmTypeFact::Union(_) => WasmAbiStructure::Unsupported { reason: "匿名 union 的库导出 ABI 合同尚未定义".to_string() },
    }
}

/// 携带冻结布局事实的结构派生（镜像公共 `abi_structure_for_type_with_layouts`）。
pub(crate) fn abi_structure_for_type_with_layouts(input: &WasmModuleInput, ty: &WasmTypeFact) -> WasmAbiStructure {
    abi_structure_with_depth(input, ty, 0)
}

fn abi_structure_with_depth(input: &WasmModuleInput, ty: &WasmTypeFact, depth: usize) -> WasmAbiStructure {
    if depth > ABI_STRUCTURE_MAX_DEPTH {
        return WasmAbiStructure::Unsupported {
            reason: format!("ABI 结构递归超过 {ABI_STRUCTURE_MAX_DEPTH} 层，拒绝无界展开 `{ty:?}`"),
        };
    }
    // sum 的物理布局是 lowering 冻结的 `[i32 tag, anyref payload]`：零载荷与带
    // 载荷变体都由同一份 sum 布局派生，目标层据此解析真实 sum 布局。
    if let Some(sum) = input.sum_layouts_by_type.get(ty) {
        let variants = sum_variants_of(sum);
        let payloads = sum
            .variants
            .iter()
            .map(|variant| variant.payload_type.as_ref().map(|payload| Box::new(abi_structure_with_depth(input, payload, depth + 1))))
            .collect();
        return WasmAbiStructure::Unite {
            nominal: sum.nominal,
            name: sum.name.clone(),
            identity: sum_identity_facts(&sum.name, sum.is_unite, &variants),
            variants,
            payloads,
        };
    }
    match ty {
        WasmTypeFact::Nominal { layout, spelling } => aggregate_structure(
            input,
            crate::abi_structure::simple_name_of(spelling),
            crate::abi_structure::module_path_of(spelling),
            *layout,
        ),
        // `Apply` 不携带布局身份；布局身份只能来自 canonical 计划冻结的类型索引。
        // 索引里没有就退回身份结构，目标层据此结构化失败，绝不按名字猜布局。
        WasmTypeFact::Apply(..) => match input.layout_by_type.get(ty) {
            Some(layout) => {
                let (name, module_path, _) = flatten_apply_identity(ty);
                aggregate_structure(input, name, module_path, *layout)
            }
            None => abi_structure_for_type(ty),
        },
        WasmTypeFact::Array(element) => WasmAbiStructure::Array {
            element: Box::new(abi_structure_with_depth(input, element, depth + 1)),
            element_key: canonical_type_spelling(element),
        },
        WasmTypeFact::FixedArray { element, length } => WasmAbiStructure::FixedArray {
            element: Box::new(abi_structure_with_depth(input, element, depth + 1)),
            length: *length,
        },
        WasmTypeFact::Nullable(inner) => {
            WasmAbiStructure::Option { inner: Box::new(abi_structure_with_depth(input, inner, depth + 1)) }
        }
        WasmTypeFact::Tuple(fields) => WasmAbiStructure::Tuple {
            fields: fields.iter().map(|field| abi_structure_with_depth(input, field, depth + 1)).collect(),
        },
        other => abi_structure_for_type(other),
    }
}

/// 引用存储名义实例保留共享身份；值存储聚合按字段结构投影（镜像公共实现）。
fn aggregate_structure(input: &WasmModuleInput, name: String, module_path: String, layout_id: WasmLayoutKey) -> WasmAbiStructure {
    let Some(aggregate) = input.layouts.iter().find(|item| item.id == layout_id)
    else {
        return WasmAbiStructure::Unsupported { reason: format!("layout `{layout_id}` 未注册，无法派生字段访问合同") };
    };
    // `[host_projection]` 声明授权宿主序列投影：只按冻结投影集合触发，
    // 形状仅在已声明类型内部定位 backing array 与计数槽位。声明是授权不是
    // 强制：布局事实不完整时回退既有存储语义，绝不因投影缺事实打断整条交付。
    if input.host_sequence_projections.contains(&layout_id) {
        let projection = host_sequence_projection(input, aggregate, name.clone(), module_path.clone());
        if !matches!(projection, WasmAbiStructure::Unsupported { .. }) {
            return projection;
        }
    }
    // 引用存储名义实例是共享身份，不是字段快照。
    if aggregate.storage == WasmStorageClass::Reference {
        return WasmAbiStructure::InstanceHandle { layout: layout_id, name, module_path };
    }
    let mut fields = Vec::with_capacity(aggregate.fields.len());
    for field in &aggregate.fields {
        let field_structure = abi_structure_for_type_with_layouts(input, &field.ty);
        if let Some(reason) = field_structure.unsupported_reason() {
            return WasmAbiStructure::Unsupported {
                reason: format!("聚合 `{}::{}` 字段 `{}` 没有 ABI 合同: {reason}", aggregate.namespace, aggregate.name, field.name),
            };
        }
        fields.push(WasmAbiField { name: field.name.clone(), ty: field_structure });
    }
    WasmAbiStructure::Aggregate(WasmAggregateStructure { layout: Some(layout_id), name, module_path, generic_args: Vec::new(), fields })
}

/// 为已声明 `[host_projection]` 的聚合构造宿主序列投影（镜像公共实现）。
fn host_sequence_projection(input: &WasmModuleInput, root: &WasmLayoutFact, name: String, module_path: String) -> WasmAbiStructure {
    let unsupported = |reason: String| WasmAbiStructure::Unsupported { reason: format!("宿主序列投影 `{name}` 失败闭合：{reason}") };
    let mut levels: Vec<WasmSequenceLevel> = Vec::new();
    let mut visited = BTreeSet::new();
    let mut current = root;
    loop {
        if !visited.insert(current.id) {
            return unsupported("嵌套存储路径成环，没有唯一的 backing array 路径".to_string());
        }
        if levels.len() >= HOST_PROJECTION_MAX_DEPTH {
            return unsupported(format!("嵌套存储深度超过上限 {HOST_PROJECTION_MAX_DEPTH}"));
        }
        let items_slots: Vec<usize> =
            current.fields.iter().enumerate().filter(|(_, field)| matches!(field.ty, WasmTypeFact::Array(_))).map(|(index, _)| index).collect();
        if items_slots.len() > 1 {
            return unsupported("同一层存在多个数组字段，无法唯一确定 backing array".to_string());
        }
        let length_slots: Vec<usize> =
            current.fields.iter().enumerate().filter(|(_, field)| is_integer_layout_type(&field.ty)).map(|(index, _)| index).collect();
        if length_slots.len() > 1 {
            return unsupported("同一层存在多个整数字段，无法唯一确定计数槽位".to_string());
        }
        let length_slot = length_slots.first().copied();
        if let Some(items_slot) = items_slots.first().copied() {
            // 叶子层：backing array 就在本层。
            levels.push(WasmSequenceLevel {
                layout: current.id,
                field_count: current.fields.len(),
                items_slot: Some(items_slot),
                length_slot,
                inner_slot: None,
            });
            let WasmTypeFact::Array(element) = &current.fields[items_slot].ty
            else {
                return unsupported("叶子层 backing array 字段事实缺失".to_string());
            };
            let element_structure = abi_structure_for_type_with_layouts(input, element);
            if let Some(reason) = element_structure.unsupported_reason() {
                return unsupported(format!("元素类型没有 ABI 合同: {reason}"));
            }
            let Some(root_length_slot) = levels[0].length_slot
            else {
                return unsupported("根层没有整数计数字段，无法读回长度".to_string());
            };
            let length_ty_structure = abi_structure_for_type_with_layouts(input, &root.fields[root_length_slot].ty);
            if let Some(reason) = length_ty_structure.unsupported_reason() {
                return unsupported(format!("计数字段没有 ABI 合同: {reason}"));
            }
            return WasmAbiStructure::Sequence(WasmSequenceStructure {
                layout: root.id,
                name,
                module_path,
                levels,
                element: Box::new(element_structure),
                element_key: canonical_type_spelling(element),
                length_ty: Box::new(length_ty_structure),
            });
        }
        // 非叶子层：沿唯一携带布局身份的名义字段下降。
        let inner_slots: Vec<(usize, WasmLayoutKey)> =
            current
                .fields
                .iter()
                .enumerate()
                .filter_map(|(index, field)| field_layout_id(&field.ty, &input.layout_by_type).map(|id| (index, id)))
                .collect();
        if inner_slots.len() != 1 {
            let spellings = current.fields.iter().map(|field| format!("{}: {:?}", field.name, field.ty)).collect::<Vec<_>>().join(" | ");
            return unsupported(format!("层 `{}` 既无 backing array，也没有可唯一定位的嵌套存储字段（字段：{spellings}）", current.id));
        }
        let (inner_slot, inner_layout) = inner_slots[0];
        levels.push(WasmSequenceLevel {
            layout: current.id,
            field_count: current.fields.len(),
            items_slot: None,
            length_slot,
            inner_slot: Some(inner_slot),
        });
        let Some(next) = input.layouts.iter().find(|item| item.id == inner_layout)
        else {
            return unsupported(format!("嵌套存储字段的布局 `{inner_layout}` 未注册"));
        };
        current = next;
    }
}

/// 把嵌套的 `Apply(Apply(Named("A"), ...), ...)` 展平（镜像公共实现）。
fn flatten_apply_identity(mut ty: &WasmTypeFact) -> (String, String, Vec<WasmTypeFact>) {
    let mut accumulated: Vec<WasmTypeFact> = Vec::new();
    loop {
        match ty {
            WasmTypeFact::Named(id) => return (crate::abi_structure::simple_name_of(id), crate::abi_structure::module_path_of(id), accumulated),
            WasmTypeFact::Nominal { spelling, .. } => {
                return (crate::abi_structure::simple_name_of(spelling), crate::abi_structure::module_path_of(spelling), accumulated)
            }
            WasmTypeFact::Apply(base, args) => {
                accumulated.extend(args.iter().cloned());
                ty = base.as_ref();
            }
            other => {
                return (format!("<unexpected:{other:?}>"), String::new(), accumulated);
            }
        }
    }
}

// ── 文本 ABI / GC 载体 / operator 载体（私有镜像）─────────────────────────

/// 未落地文本 ABI 载体的统一拒绝理由（镜像公共 `unresolved_text_abi_reason`）。
pub(crate) fn unresolved_text_abi_reason(ty: &WasmTypeFact) -> Option<&'static str> {
    match ty {
        WasmTypeFact::Utf16 => Some(UTF16_UNRESOLVED_TEXT_ABI),
        _ => None,
    }
}

/// 文本 ABI 事实查询（镜像公共 `declared_text_projection` 的载体结论）。
pub(crate) fn declared_text_projection(boundary: PhysicalTextBoundary, ty: &WasmTypeFact) -> Option<PhysicalTextCarrier> {
    match ty {
        WasmTypeFact::Utf8 => match boundary {
            PhysicalTextBoundary::WasmJsGlue => Some(PhysicalTextCarrier::JsStringHandle),
            PhysicalTextBoundary::WasiComponent => Some(PhysicalTextCarrier::CanonicalUtf8Bytes),
        },
        // `WasmJsGlue` 与 `utf8` 共用同一张 JS string 句柄表；`WasiComponent`
        // 的 canonical ABI 只有 UTF-8 字节列表，不存在 `utf16` 的文本 ABI 事实。
        WasmTypeFact::Utf16 => match boundary {
            PhysicalTextBoundary::WasmJsGlue => Some(PhysicalTextCarrier::JsStringHandle),
            PhysicalTextBoundary::WasiComponent => None,
        },
        _ => None,
    }
}

/// GC 聚合字段槽/数组元素的载体映射（镜像 `wasm::gc` 同名表，类型换成私有）。
pub(crate) fn wasm_gc_value_carrier_valtype(ty: &WasmTypeFact) -> Option<u8> {
    use data_binary_wasm::{VALTYPE_ANYREF, VALTYPE_F32, VALTYPE_F64, VALTYPE_I32, VALTYPE_I64};
    Some(match ty {
        WasmTypeFact::Utf16 => return None,
        WasmTypeFact::Float32 => VALTYPE_F32,
        WasmTypeFact::Float64 => VALTYPE_F64,
        WasmTypeFact::Integer64 { .. } | WasmTypeFact::Integer128 { .. } => VALTYPE_I64,
        WasmTypeFact::Named(_)
        | WasmTypeFact::Nominal { .. }
        | WasmTypeFact::Apply(_, _)
        | WasmTypeFact::Function(_)
        | WasmTypeFact::Array(_)
        | WasmTypeFact::TraitObject(_)
        | WasmTypeFact::Nullable(_)
        | WasmTypeFact::Union(_) => VALTYPE_ANYREF,
        WasmTypeFact::Bottom
        | WasmTypeFact::Unit
        | WasmTypeFact::Boolean
        | WasmTypeFact::Integer8 { .. }
        | WasmTypeFact::Integer16 { .. }
        | WasmTypeFact::Integer32 { .. }
        | WasmTypeFact::Character
        | WasmTypeFact::Utf8 => VALTYPE_I32,
        WasmTypeFact::Tuple(_) | WasmTypeFact::FixedArray { .. } => VALTYPE_I32,
    })
}

/// `WasmJsGlue` 边界上的值载体映射（镜像 `wasm::gc` 同名表）。
pub(crate) fn wasm_js_glue_value_carrier_valtype(ty: &WasmTypeFact) -> Option<u8> {
    match ty {
        WasmTypeFact::Utf8 | WasmTypeFact::Utf16 => Some(data_binary_wasm::VALTYPE_I32),
        other => wasm_gc_value_carrier_valtype(other),
    }
}

/// 将类型事实映射到 wasm-gc 字段/元素值类型字节（镜像 `mir::representation` 同名表）。
pub(crate) fn wasm_gc_field_type_byte(ty: &WasmTypeFact) -> Result<u8> {
    wasm_gc_value_carrier_valtype(ty).ok_or_else(|| match ty {
        WasmTypeFact::Utf16 => "WASM GC 字段/元素槽没有 `utf16` 的冻结载体：`utf16` 不得借用 `utf8` 的 i32 宿主句柄（句柄不携带编码身份），\
             也不能默认落线性内存 i32；需要显式 UTF-16 文本 ABI 契约为该槽提供载体"
            .to_string(),
        WasmTypeFact::Float32 => "WASM GC 字段/元素槽的 `f32` 载体映射缺失：必须返回 `VALTYPE_F32`（`0x7D`），\
            禁止拓宽到 `VALTYPE_F64` 或借用 i32 假载体"
            .to_string(),
        other => format!("WASM GC 字段/元素载体映射缺少类型分支: {other:?}"),
    })
}

/// 操作数载体（镜像公共 `target_operator::operand_carrier`，类型换成私有）。
pub(crate) fn operand_carrier(ty: &WasmTypeFact) -> Option<TargetOperandType> {
    match ty {
        WasmTypeFact::Integer32 { .. } => Some(TargetOperandType::I32),
        // `char`（u16 code unit）在这些 lane 的物理载体是 i32。
        WasmTypeFact::Character => Some(TargetOperandType::I32),
        WasmTypeFact::Integer64 { .. } => Some(TargetOperandType::I64),
        WasmTypeFact::Float32 => Some(TargetOperandType::F32),
        WasmTypeFact::Float64 => Some(TargetOperandType::F64),
        _ => None,
    }
}

/// 结果槽载体（镜像公共 `target_operator::result_carrier`）。
pub(crate) fn result_carrier(ty: &WasmTypeFact) -> Option<TargetOperandType> {
    match ty {
        WasmTypeFact::Boolean => Some(TargetOperandType::I32),
        _ => operand_carrier(ty),
    }
}

/// 校验 lane 冻结的 operator 计划与导入声明签名一致（镜像公共同名函数）。
pub(crate) fn validate_frozen_operator<E>(
    mnemonic: &str,
    instance: WasmCallableKey,
    function: &str,
    parameter_types: &[WasmTypeFact],
    return_type: &WasmTypeFact,
    plan: &TargetOperatorPlan<E>,
) -> Result<()> {
    if parameter_types.len() != plan.arity {
        return Err(format!(
            "目标 operator 绑定 `{mnemonic}` 的声明 arity 与冻结计划不符: function={function} expected={} actual={} instance={instance:?}",
            plan.arity,
            parameter_types.len()
        ));
    }
    for (index, ty) in parameter_types.iter().enumerate() {
        let Some(carrier) = operand_carrier(ty)
        else {
            return Err(format!(
                "目标 operator 绑定 `{mnemonic}` 的参数 {index} 类型没有目标载体: function={function} type={ty:?} instance={instance:?}"
            ));
        };
        if carrier != plan.operand_type {
            return Err(format!(
                "目标 operator 绑定 `{mnemonic}` 的参数 {index} 操作数载体与冻结计划不符: function={function} expected={:?} actual={:?} instance={instance:?}",
                plan.operand_type, carrier
            ));
        }
    }
    let Some(result) = result_carrier(return_type)
    else {
        return Err(format!(
            "目标 operator 绑定 `{mnemonic}` 的返回类型没有结果槽载体: function={function} type={return_type:?} instance={instance:?}"
        ));
    };
    if result != plan.result_type {
        return Err(format!(
            "目标 operator 绑定 `{mnemonic}` 的结果槽载体与冻结计划不符: function={function} expected={:?} actual={:?} instance={instance:?}",
            plan.result_type, result
        ));
    }
    if let TargetOperatorRecipe::Steps(steps) = plan.recipe {
        for step in steps {
            if let TargetOperandStep::IntegerConstant { operand_type, .. } = step {
                if *operand_type != plan.operand_type {
                    return Err(format!(
                        "目标 operator 绑定 `{mnemonic}` 的常量配方载体与冻结操作数载体不符: function={function} expected={:?} actual={:?} instance={instance:?}",
                        plan.operand_type, operand_type
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Wasm 物理导出 ABI 的私有派生点（镜像 `abi::freeze_wasm_export_abi_for`）。
pub(crate) fn wasm_export_abi(input: &WasmModuleInput, instance: &WasmCallableKey) -> Result<WasmLibraryExportAbi> {
    let function = input
        .function(instance)
        .ok_or_else(|| format!("库导出 `{instance}` 缺少 Compiler 函数体，无法冻结 Wasm ABI 载体合同"))?;
    let mut params = Vec::with_capacity(function.param_types.len());
    for (index, ty) in function.param_types.iter().enumerate() {
        let carrier = freeze_wasm_carrier(ty)
            .map_err(|reason| format!("函数 `{}` 参数 {index} 类型 `{ty}` 没有库导出 ABI 载体: {reason}", function.symbol))?;
        if carrier == WasmAbiCarrier::Void {
            return Err(format!("函数 `{}` 参数 {index} 类型 `{ty}` 映射为 Void 载体，Void 只允许出现在返回位", function.symbol));
        }
        params.push(carrier);
    }
    let returns = freeze_wasm_carrier(&function.return_type)
        .map_err(|reason| format!("函数 `{}` 返回类型 `{}` 没有库导出 ABI 载体: {reason}", function.symbol, function.return_type))?;
    Ok(WasmLibraryExportAbi { params, returns })
}

fn freeze_wasm_carrier(ty: &WasmTypeFact) -> Result<WasmAbiCarrier> {
    match ty {
        WasmTypeFact::Unit => Ok(WasmAbiCarrier::Void),
        WasmTypeFact::Boolean | WasmTypeFact::Character => Ok(WasmAbiCarrier::I32),
        WasmTypeFact::Integer8 { .. } | WasmTypeFact::Integer16 { .. } | WasmTypeFact::Integer32 { .. } => Ok(WasmAbiCarrier::I32),
        WasmTypeFact::Integer64 { .. } => Ok(WasmAbiCarrier::I64),
        WasmTypeFact::Float32 => Ok(WasmAbiCarrier::F32),
        WasmTypeFact::Float64 => Ok(WasmAbiCarrier::F64),
        WasmTypeFact::Utf8 => Ok(WasmAbiCarrier::I32),
        WasmTypeFact::Array(_) | WasmTypeFact::FixedArray { .. } | WasmTypeFact::Nullable(_) | WasmTypeFact::Tuple(_) => {
            Ok(WasmAbiCarrier::Handle)
        }
        WasmTypeFact::Named(_) | WasmTypeFact::Nominal { .. } | WasmTypeFact::Apply(_, _) => Ok(WasmAbiCarrier::Handle),
        WasmTypeFact::Integer128 { .. }
        | WasmTypeFact::Utf16
        | WasmTypeFact::Bottom
        | WasmTypeFact::Function(_)
        | WasmTypeFact::TraitObject(_)
        | WasmTypeFact::Union(_) => Err(format!("Wasm 导出面没有该类型的稳定 ABI 载体: {ty}")),
    }
}

/// 数据流标准逆后序可达块（镜像 `shared::executable::collect_reachable_blocks`）。
pub(crate) fn collect_reachable_blocks(function: &WasmFunctionInput) -> Vec<WasmBlockKey> {
    let mut adjacency: Vec<(WasmBlockKey, WasmBlockKey)> = Vec::new();
    for (index, block) in function.blocks.iter().enumerate() {
        let from = WasmBlockKey(index as u32);
        let targets: Vec<WasmBlockKey> = match &block.terminator {
            WasmTerminator::Return { .. } | WasmTerminator::Unreachable => Vec::new(),
            WasmTerminator::Jump { target, .. } => vec![*target],
            WasmTerminator::Branch { then_target, else_target, .. } => vec![*then_target, *else_target],
            WasmTerminator::Raise { resume_target, .. } => vec![*resume_target],
        };
        adjacency.extend(targets.into_iter().map(|target| (from, target)));
    }

    let mut graph = boost_collection::MemoryGraph::new(true);
    graph.add_vertex(function.entry).expect("fresh graph");
    for index in 0..function.blocks.len() {
        graph.add_vertex(WasmBlockKey(index as u32)).ok();
    }
    for (_, target) in &adjacency {
        graph.add_vertex(*target).ok();
    }
    for (from, target) in adjacency {
        graph.add_edge(from, target, ()).expect("both endpoints were added as vertices");
    }

    boost_math::graph_theory::reverse_postorder(&graph, &function.entry).expect("the entry is a vertex of the graph").into_iter().collect()
}

// ── 模块级私有输入 ────────────────────────────────────────────────────────

/// Wasm 目标私有结算输入：mir/ 编码层消费的唯一形状。
#[derive(Debug, Clone)]
pub(crate) struct WasmModuleInput {
    /// 按实例身份升序的函数体。
    pub functions: BTreeMap<WasmCallableKey, WasmFunctionInput>,
    /// 实例顺序（公共 `instances()` 的结算形态）。
    pub instances: Vec<WasmCallableKey>,
    /// 程序入口实例。
    pub entry_operation: Option<WasmCallableKey>,
    /// 声明 `[export]` 的导出名。
    pub export_names: BTreeMap<WasmCallableKey, String>,
    /// 导出注解名（优先于 `[export]` 名）。
    pub export_annotations: BTreeMap<WasmCallableKey, String>,
    /// 导出面操作清单。
    pub exported_operations: Vec<WasmCallableKey>,
    /// 实例到 ABI 限定名（`<wasm-operator import …>` 诊断标签来源）。
    pub abi_labels: BTreeMap<WasmCallableKey, String>,
    pub module_name: String,
    pub fragment_id: String,
    pub required_capabilities: Vec<String>,
    pub theory_rules: usize,
    pub theory_equations: usize,
    /// 后端导入签名事实。
    pub imports: BTreeMap<WasmCallableKey, WasmImportFact>,
    /// 实例的外部导入链接候选。
    pub external_import_links: BTreeMap<WasmCallableKey, Vec<WasmImportLinkFact>>,
    /// WASM32 物理布局（settle 一次性解析）。
    pub layouts: Vec<WasmLayoutFact>,
    /// 名义实例 → 布局身份。
    pub layout_by_nominal_id: BTreeMap<WasmNominalKey, WasmLayoutKey>,
    /// 字段身份 → `(布局身份, 字段槽位)`。
    pub layout_by_field_id: BTreeMap<WasmFieldKey, (WasmLayoutKey, u32)>,
    /// 冻结类型 → 布局身份（含契约归一键）。
    pub layout_by_type: BTreeMap<WasmTypeFact, WasmLayoutKey>,
    /// 冻结类型 → sum 布局（含预结算 identity）。
    pub sum_layouts_by_type: BTreeMap<WasmTypeFact, WasmSumLayoutFact>,
    /// sum 平台合同（tag / payload 有无）。
    pub sum_reps: BTreeMap<WasmNominalKey, WasmSumRep>,
    /// 程序级文本载体表示。
    pub program_text_representation: Option<WasmProgramTextRepresentation>,
    /// singleton 生命周期事实（附物理尺寸，缺失为 `None`）。
    pub singleton_facts: Vec<WasmSingletonFact>,
    /// `[host_projection]` 声明授权的宿主序列布局集合。
    pub host_sequence_projections: BTreeSet<WasmLayoutKey>,
}

impl WasmModuleInput {
    /// 按布局身份查物理布局。
    pub(crate) fn layout_by_id(&self, id: WasmLayoutKey) -> Option<&WasmLayoutFact> {
        self.layouts.iter().find(|layout| layout.id == id)
    }

    /// 按名义实例身份查物理布局。
    pub(crate) fn layout_by_nominal(&self, nominal: WasmNominalKey) -> Option<&WasmLayoutFact> {
        self.layout_by_nominal_id.get(&nominal).and_then(|id| self.layout_by_id(*id))
    }

    /// 按字段身份查 `(布局, 字段, 槽位)`。
    pub(crate) fn field_layout_by_id(&self, field: WasmFieldKey) -> Option<(&WasmLayoutFact, &WasmFieldFact, u32)> {
        let (layout_id, slot) = *self.layout_by_field_id.get(&field)?;
        let layout = self.layout_by_id(layout_id)?;
        let field = layout.fields.get(slot as usize)?;
        Some((layout, field, slot))
    }

    /// 按类型事实查物理布局：`Nominal` 用内嵌身份，其余查冻结索引。
    pub(crate) fn layout_for_type(&self, ty: &WasmTypeFact) -> Option<&WasmLayoutFact> {
        if let WasmTypeFact::Nominal { layout, .. } = ty {
            return self.layout_by_id(*layout);
        }
        self.layout_by_type.get(ty).and_then(|id| self.layout_by_id(*id))
    }

    /// 按实例身份查函数体。
    pub(crate) fn function(&self, instance: &WasmCallableKey) -> Option<&WasmFunctionInput> {
        self.functions.get(instance)
    }

    /// 实例顺序清单。
    pub(crate) fn instances(&self) -> &[WasmCallableKey] {
        &self.instances
    }

    /// 导出公开名：注解名优先，缺省用 `[export]` 声明名。
    pub(crate) fn export_public_name(&self, instance: &WasmCallableKey) -> Option<&String> {
        self.export_annotations.get(instance).or_else(|| self.export_names.get(instance))
    }

    /// 测试专用：全空结算输入，供直接构造私有事实的单元测试使用。
    #[cfg(test)]
    pub(crate) fn testing_empty() -> Self {
        Self {
            functions: BTreeMap::new(),
            instances: Vec::new(),
            entry_operation: None,
            export_names: BTreeMap::new(),
            export_annotations: BTreeMap::new(),
            exported_operations: Vec::new(),
            abi_labels: BTreeMap::new(),
            module_name: String::new(),
            fragment_id: String::new(),
            required_capabilities: Vec::new(),
            theory_rules: 0,
            theory_equations: 0,
            imports: BTreeMap::new(),
            external_import_links: BTreeMap::new(),
            layouts: Vec::new(),
            layout_by_nominal_id: BTreeMap::new(),
            layout_by_field_id: BTreeMap::new(),
            layout_by_type: BTreeMap::new(),
            sum_layouts_by_type: BTreeMap::new(),
            sum_reps: BTreeMap::new(),
            program_text_representation: None,
            singleton_facts: Vec::new(),
            host_sequence_projections: BTreeSet::new(),
        }
    }
}

// ── settle：唯一公共 IR 读取点 ────────────────────────────────────────────

/// 把跨目标公共 IR 一次结算为 Wasm 目标私有输入。
///
/// 本函数是 `mir/` 子系统之外唯一允许读取 `FragmentSubmission` /
/// `TargetExecutionPlan` / `ExecutableFunction` / `SemanticType` 的地方。
/// 物理布局解析在这里完成（原先发生在 mir 编码中段）；解析失败即结构化失败，
/// 错误文案原样透传公共 `resolve_physical_layouts` 的产出。
pub(crate) fn settle_from_submission(submission: &FragmentSubmission) -> Result<WasmModuleInput> {
    let plan = &submission.backend_plan;

    let mut functions = BTreeMap::new();
    let instances: Vec<WasmCallableKey> = plan.instances().into_iter().map(|instance| WasmCallableKey(instance.index())).collect();
    for instance in plan.instances() {
        let view = plan.get_function(&instance).expect("Compiler 实例必须有函数体");
        functions.insert(WasmCallableKey(instance.index()), settle_function(&view.function));
    }

    let entry_operation = plan.entry_operation().map(|instance| WasmCallableKey(instance.index()));
    let export_names = plan.export_names().iter().map(|(instance, name)| (WasmCallableKey(instance.index()), name.clone())).collect();
    let export_annotations = plan.export_annotations().iter().map(|(instance, name)| (WasmCallableKey(instance.index()), name.clone())).collect();
    let exported_operations = plan.exported_operations().iter().map(|instance| WasmCallableKey(instance.index())).collect();
    let mut abi_labels = BTreeMap::new();
    for instance in plan.instances() {
        if let Some(name) = plan.abi_name_for_instance(instance) {
            abi_labels.insert(WasmCallableKey(instance.index()), name.to_string());
        }
    }
    let module_name = plan.module_name().to_string();
    let fragment_id = plan.fragment_id().as_str().to_string();
    let required_capabilities = plan.required_capabilities().iter().map(|capability| capability.as_str().to_string()).collect();
    let merged_theory = plan.theory_bundle().merged();
    let theory_rules = merged_theory.rules.len();
    let theory_equations = merged_theory.equations.len();

    let imports = plan
        .imports()
        .iter()
        .map(|(instance, import)| {
            (
                WasmCallableKey(instance.index()),
                WasmImportFact {
                    links: import.links.iter().map(settle_import_link).collect(),
                    parameter_types: import.parameter_types.iter().map(settle_type).collect(),
                    return_type: settle_type(&import.return_type),
                },
            )
        })
        .collect();
    let external_import_links = plan
        .external_import_links()
        .iter()
        .map(|(instance, links)| (WasmCallableKey(instance.index()), links.iter().map(settle_import_link).collect()))
        .collect();

    let resolved = resolve_physical_layouts(plan.aggregate_layouts(), LayoutPolicy::WASM32)?;
    let layouts = resolved.layouts.iter().map(settle_layout).collect();
    let layout_by_nominal_id =
        plan.aggregate_layout_by_nominal().iter().map(|(nominal, id)| (WasmNominalKey(nominal.index()), *id)).collect();
    let layout_by_field_id = plan
        .aggregate_layout_by_field()
        .iter()
        .map(|(field, (id, slot))| (WasmFieldKey(field.index()), (*id, *slot)))
        .collect();
    let mut layout_by_type: BTreeMap<WasmTypeFact, WasmLayoutKey> = BTreeMap::new();
    for (ty, id) in plan.aggregate_layouts().layout_by_semantic_type.iter() {
        layout_by_type.insert(settle_type(ty), *id);
    }
    // 公共查询顺序是「先原键、后契约归一键」；这里同时登记两种键，
    // 归一键不覆盖已存在的原键。
    for (ty, id) in plan.aggregate_layouts().layout_by_semantic_type.iter() {
        layout_by_type.entry(settle_type(ty).contract_key()).or_insert(*id);
    }
    let sum_layouts_by_type = plan
        .sum_layout_by_semantic_type()
        .iter()
        .map(|(ty, sum)| {
            let fact = settle_sum_layout(sum);
            (settle_type(ty), fact)
        })
        .collect();
    let sum_reps = plan
        .sum_representations()
        .iter()
        .map(|(nominal, rep)| {
            (
                WasmNominalKey(nominal.index()),
                WasmSumRep {
                    nominal: WasmNominalKey(rep.nominal.index()),
                    variants: rep
                        .variants
                        .iter()
                        .map(|(variant, contract)| (WasmVariantKey(variant.index()), WasmSumVariantFact { tag: contract.tag, has_payload: contract.payload_type.is_some() }))
                        .collect(),
                },
            )
        })
        .collect();
    let program_text_representation = plan.program_text_representation().map(|representation| match representation {
        crate::target_execution_plan::ProgramTextRepresentation::Raw => WasmProgramTextRepresentation::Raw,
        crate::target_execution_plan::ProgramTextRepresentation::Managed => WasmProgramTextRepresentation::Managed,
    });
    let singleton_facts = plan
        .singleton_instances()
        .iter()
        .map(|singleton| {
            let layout = resolved.layouts.iter().find(|layout| layout.name == singleton.name && layout.namespace == singleton.namespace);
            WasmSingletonFact {
                namespace: singleton.namespace.clone(),
                name: singleton.name.clone(),
                instance_field: singleton.instance_field.clone(),
                is_lazy: singleton.is_lazy,
                constructor_symbol: singleton.constructor_symbol.clone(),
                finalizer_symbol: singleton.finalizer_symbol.clone(),
                layout_size: layout.map(|layout| layout.size),
                layout_align: layout.map(|layout| layout.align),
                accessor_method: singleton.accessor_method().to_string(),
                qualified_name: singleton.qualified_name(),
                supports_unload: singleton.supports_unload(),
            }
        })
        .collect();
    let host_sequence_projections = plan.aggregate_layouts().host_sequence_projections.iter().copied().collect();

    Ok(WasmModuleInput {
        functions,
        instances,
        entry_operation,
        export_names,
        export_annotations,
        exported_operations,
        abi_labels,
        module_name,
        fragment_id,
        required_capabilities,
        theory_rules,
        theory_equations,
        imports,
        external_import_links,
        layouts,
        layout_by_nominal_id,
        layout_by_field_id,
        layout_by_type,
        sum_layouts_by_type,
        sum_reps,
        program_text_representation,
        singleton_facts,
        host_sequence_projections,
    })
}

fn settle_function(function: &crate::contracts::ExecutableFunction) -> WasmFunctionInput {
    WasmFunctionInput {
        symbol: function.symbol.clone(),
        return_type: settle_type(&function.return_type),
        param_types: function.param_types.iter().map(settle_type).collect(),
        value_types: function.value_types.iter().map(|(value, ty)| (WasmValueKey(value.0), settle_type(ty))).collect(),
        value_layouts: function.value_layouts.iter().map(|(value, id)| (WasmValueKey(value.0), *id)).collect(),
        return_layout: function.return_layout,
        entry: WasmBlockKey(function.entry.0),
        values: function
            .values
            .iter()
            .map(|value| WasmValue {
                id: WasmValueKey(value.id.0),
                origin: match &value.origin {
                    crate::contracts::ValueOrigin::Parameter { index, name } => WasmValueOrigin::Parameter { index: *index, name: name.clone() },
                    crate::contracts::ValueOrigin::BlockParameter { block, name } => WasmValueOrigin::BlockParameter { block: WasmBlockKey(block.0), name: name.clone() },
                    crate::contracts::ValueOrigin::Literal => WasmValueOrigin::Literal,
                    crate::contracts::ValueOrigin::Path => WasmValueOrigin::Path,
                    crate::contracts::ValueOrigin::CallResult => WasmValueOrigin::CallResult,
                    crate::contracts::ValueOrigin::Temporary => WasmValueOrigin::Temporary,
                },
            })
            .collect(),
        blocks: function.blocks.iter().map(settle_block).collect(),
        parameter_identities: function
            .values
            .iter()
            .filter_map(|value| match &value.origin {
                crate::contracts::ValueOrigin::Parameter { index, name } => Some((*index, name.clone())),
                _ => None,
            })
            .collect(),
    }
}

fn settle_block(block: &crate::contracts::Block) -> WasmBlock {
    WasmBlock {
        id: WasmBlockKey(block.id.0),
        label: block.label.clone(),
        parameters: block.parameters.iter().map(|value| WasmValueKey(value.0)).collect(),
        instructions: block.instructions.iter().map(settle_instruction).collect(),
        terminator: settle_terminator(&block.terminator),
    }
}

fn settle_instruction(instruction: &crate::contracts::Instruction) -> WasmInstruction {
    WasmInstruction {
        id: WasmInstructionKey(instruction.id.index()),
        results: instruction.results.iter().map(|value| WasmValueKey(value.0)).collect(),
        kind: settle_operation(&instruction.kind),
    }
}

fn settle_operation(kind: &crate::contracts::InstructionKind) -> WasmOperation {
    use crate::contracts::InstructionKind as K;
    match kind {
        K::LoadConstant { constant, ty } => WasmOperation::LoadConstant { constant: settle_constant(constant), ty: ty.as_ref().map(settle_type) },
        K::Copy { source } => WasmOperation::Copy { source: settle_operand(source) },
        K::Cast { source, ty } => WasmOperation::Cast { source: settle_operand(source), ty: settle_type(ty) },
        K::StoreVar { value, ty } => WasmOperation::StoreVar { value: settle_operand(value), ty: ty.as_ref().map(settle_type) },
        K::Call { callee, arguments } => WasmOperation::Call { callee: settle_operand(callee), arguments: arguments.iter().map(settle_operand).collect() },
        K::LoadFunctionRef { instance, environment, captured } => WasmOperation::LoadFunctionRef {
            instance: WasmCallableKey(instance.index()),
            environment: environment.as_ref().map(settle_operand),
            captured: captured.clone(),
        },
        K::StructNew { nominal, fields } => WasmOperation::StructNew {
            nominal: WasmNominalKey(nominal.index()),
            fields: fields.iter().map(|(field, value)| (WasmFieldKey(field.index()), settle_operand(value))).collect(),
        },
        K::TupleNew { layout_id, fields } => WasmOperation::TupleNew { layout_id: *layout_id, fields: fields.iter().map(settle_operand).collect() },
        K::TupleGet { object, layout_id, index } => WasmOperation::TupleGet { object: settle_operand(object), layout_id: *layout_id, index: *index },
        K::AggregateCopy { layout_id, source, dest } => {
            WasmOperation::AggregateCopy { layout_id: *layout_id, source: settle_operand(source), dest: settle_operand(dest) }
        }
        K::FieldGet { object, field } => WasmOperation::FieldGet { object: settle_operand(object), field: WasmFieldKey(field.index()) },
        K::FieldSet { object, field, value } => {
            WasmOperation::FieldSet { object: settle_operand(object), field: WasmFieldKey(field.index()), value: settle_operand(value) }
        }
        K::SumNew { nominal, variant, payload } => WasmOperation::SumNew {
            nominal: WasmNominalKey(nominal.index()),
            variant: WasmVariantKey(variant.index()),
            payload: payload.as_ref().map(settle_operand),
        },
        K::SumPayloadGet { nominal, variant, object } => WasmOperation::SumPayloadGet {
            nominal: WasmNominalKey(nominal.index()),
            variant: WasmVariantKey(variant.index()),
            object: settle_operand(object),
        },
        K::SumVariantIs { nominal, variant, object } => WasmOperation::SumVariantIs {
            nominal: WasmNominalKey(nominal.index()),
            variant: WasmVariantKey(variant.index()),
            object: settle_operand(object),
        },
        K::ArrayNew { array_type, length, initialization } => WasmOperation::ArrayNew {
            array_type: settle_type(array_type),
            length: settle_operand(length),
            initialization: match initialization {
                crate::contracts::ArrayInitialization::Default => WasmArrayInit::Default,
                crate::contracts::ArrayInitialization::Fill(value) => WasmArrayInit::Fill(settle_operand(value)),
            },
        },
        K::ArrayFromElements { layout_id, array_type, elements } => WasmOperation::ArrayFromElements {
            layout_id: *layout_id,
            array_type: settle_type(array_type),
            elements: elements.iter().map(settle_operand).collect(),
        },
        K::ArrayGet { array, index } => WasmOperation::ArrayGet { array: settle_operand(array), index: settle_operand(index) },
        K::ArraySet { array, index, value } => {
            WasmOperation::ArraySet { array: settle_operand(array), index: settle_operand(index), value: settle_operand(value) }
        }
        K::ArrayLength { array } => WasmOperation::ArrayLength { array: settle_operand(array) },
        K::NullIsNull { value } => WasmOperation::NullIsNull { value: settle_operand(value) },
        K::NullUnwrap { value } => WasmOperation::NullUnwrap { value: settle_operand(value) },
    }
}

fn settle_terminator(terminator: &crate::contracts::Terminator) -> WasmTerminator {
    use crate::contracts::Terminator as T;
    match terminator {
        T::Return { value } => WasmTerminator::Return { value: value.as_ref().map(settle_operand) },
        T::Jump { target, arguments } => WasmTerminator::Jump { target: WasmBlockKey(target.0), arguments: arguments.iter().map(settle_operand).collect() },
        T::Branch { condition, then_target, else_target } => WasmTerminator::Branch {
            condition: settle_operand(condition),
            then_target: WasmBlockKey(then_target.0),
            else_target: WasmBlockKey(else_target.0),
        },
        T::Raise { payload, resume_target } => {
            WasmTerminator::Raise { payload: payload.as_ref().map(settle_operand), resume_target: WasmBlockKey(resume_target.0) }
        }
        T::Unreachable => WasmTerminator::Unreachable,
    }
}

fn settle_operand(operand: &crate::contracts::Operand) -> WasmOperand {
    use crate::contracts::Operand as O;
    match operand {
        O::Value(value) => WasmOperand::Value(WasmValueKey(value.0)),
        O::Constant(constant) => WasmOperand::Constant(settle_constant(constant)),
        O::Item(instance) => WasmOperand::Item(WasmCallableKey(instance.index())),
        O::Symbol(path) => WasmOperand::Symbol(path.clone()),
    }
}

fn settle_constant(constant: &crate::contracts::Constant) -> WasmConstant {
    use crate::contracts::Constant as C;
    match constant {
        C::Int(value) => WasmConstant::Int(*value),
        C::Float64(value) => WasmConstant::Float64(WasmFloat64Bits(value.into_inner())),
        C::Bool(value) => WasmConstant::Bool(*value),
        C::Utf8(value) => WasmConstant::Utf8(value.clone()),
        C::Utf16(value) => WasmConstant::Utf16(value.clone()),
        C::Unit => WasmConstant::Unit,
        C::Null => WasmConstant::Null,
    }
}

fn settle_type(ty: &compiler_types::SemanticType) -> WasmTypeFact {
    use compiler_types::SemanticType as S;
    match ty {
        S::Bottom => WasmTypeFact::Bottom,
        S::Unit => WasmTypeFact::Unit,
        S::Boolean => WasmTypeFact::Boolean,
        S::Integer8 { signed } => WasmTypeFact::Integer8 { signed: *signed },
        S::Integer16 { signed } => WasmTypeFact::Integer16 { signed: *signed },
        S::Integer32 { signed } => WasmTypeFact::Integer32 { signed: *signed },
        S::Integer64 { signed } => WasmTypeFact::Integer64 { signed: *signed },
        S::Integer128 { signed } => WasmTypeFact::Integer128 { signed: *signed },
        S::Float32 => WasmTypeFact::Float32,
        S::Float64 => WasmTypeFact::Float64,
        S::Character => WasmTypeFact::Character,
        S::Utf8 => WasmTypeFact::Utf8,
        S::Utf16 => WasmTypeFact::Utf16,
        S::Named(name) => WasmTypeFact::Named(name.clone()),
        S::Nominal { layout, spelling } => WasmTypeFact::Nominal { layout: *layout, spelling: spelling.clone() },
        S::Apply(base, args) => WasmTypeFact::Apply(Box::new(settle_type(base)), args.iter().map(settle_type).collect()),
        S::Function(function) => WasmTypeFact::Function(Box::new(WasmFunctionType {
            params: function.params.iter().map(settle_type).collect(),
            return_type: settle_type(&function.return_type),
        })),
        S::Tuple(items) => WasmTypeFact::Tuple(items.iter().map(settle_type).collect()),
        S::Array(element) => WasmTypeFact::Array(Box::new(settle_type(element))),
        S::FixedArray { element, length } => WasmTypeFact::FixedArray { element: Box::new(settle_type(element)), length: *length },
        S::TraitObject(object) => WasmTypeFact::TraitObject(WasmWitnessObject {
            trait_path: object.trait_path.clone(),
            type_arguments: object.type_arguments.iter().map(settle_type).collect(),
        }),
        S::Nullable(element) => WasmTypeFact::Nullable(Box::new(settle_type(element))),
        S::Union(arms) => WasmTypeFact::Union(arms.iter().map(settle_type).collect()),
    }
}

fn settle_layout(layout: &crate::lowering::shared::physical_layout::ResolvedAggregateLayout) -> WasmLayoutFact {
    WasmLayoutFact {
        id: layout.id,
        name: layout.name.clone(),
        namespace: layout.namespace.clone(),
        key: layout.key.as_ref().map(settle_layout_key),
        storage: match layout.storage {
            compiler_types::StorageKind::Value => WasmStorageClass::Value,
            compiler_types::StorageKind::Reference => WasmStorageClass::Reference,
        },
        size: layout.size,
        align: layout.align,
        fields: layout
            .fields
            .iter()
            .map(|field| WasmFieldFact {
                name: field.name.clone(),
                ty: settle_type(&field.ty),
                offset: field.offset,
                size: field.size,
                align: field.align,
            })
            .collect(),
    }
}

fn settle_layout_key(key: &compiler_types::LayoutKey) -> WasmLayoutKeyFact {
    match key {
        compiler_types::LayoutKey::Tuple(items) => WasmLayoutKeyFact::Tuple(items.iter().map(settle_type).collect()),
        compiler_types::LayoutKey::FixedArray { element, length } => WasmLayoutKeyFact::FixedArray { element: Box::new(settle_type(element)), length: *length },
    }
}

fn settle_import_link(link: &compiler_types::ExternalImportLink) -> WasmImportLinkFact {
    WasmImportLinkFact { boundary: link.boundary.clone(), platform_tag: link.platform_tag.clone(), locator_segments: link.locator_segments.clone() }
}

fn settle_sum_layout(sum: &compiler_types::SumTypeLayout) -> WasmSumLayoutFact {
    let variants = sum
        .variants
        .iter()
        .map(|variant| WasmSumVariantLayoutFact {
            id: WasmVariantKey(variant.id.index()),
            name: variant.name.clone(),
            tag: variant.tag,
            payload_type: variant.payload_type.as_ref().map(settle_type),
        })
        .collect::<Vec<_>>();
    let abi_variants = variants.iter().map(|variant| WasmAbiVariant { name: variant.name.clone(), tag: variant.tag }).collect::<Vec<_>>();
    WasmSumLayoutFact {
        nominal: WasmNominalKey(sum.nominal.index()),
        name: sum.name.clone(),
        is_unite: sum.is_unite,
        variants,
        identity: sum_identity_facts(&sum.name, sum.is_unite, &abi_variants),
    }
}
