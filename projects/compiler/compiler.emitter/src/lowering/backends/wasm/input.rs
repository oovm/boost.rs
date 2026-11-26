//! Wasm 目标私有结算输入：mir/ 子系统的唯一词汇来源。
//!
//! 所有权边界（见 01 设计文档 §5.5）：本模块定义的私有词汇是 wasm lane
//! （`mir/` 全部子模块）编码时消费的唯一形状；结算（从语言 MIR 到这里的
//! 私有事实）由语言侧 `pipeline/wasm/` 直出完成，不再经过任何跨目标公共 IR。
//! 这里同时导出装配服务需要的私有原语（ABI 派生、文本载体事实、参数身份
//! 校验），emitter 不再读取共享程序载荷。
//!
//! 词汇边界（D5 架构门）：类型事实只携带 **wasm 目标自身**的编码形状——
//! 名义 / 泛型实例一律结算为布局引用 [`WasmTypeFact::Layout`]，堆类型与字段
//! 结构由冻结布局事实解析；这里不存在源语言类型镜像（裸拼写、名义拼写、
//! 泛型应用、witness 表、匿名 union），编码层也不得按拼写回查任何语言语义。
//! 身份 newtype 手写 `Debug` / `Display`，输出与公共 `SemanticId` 完全同名。

use std::{cmp::Ordering, collections::{BTreeMap, BTreeSet}, fmt};

use compiler_types::{Identifier, NamePath};

use crate::{
    Result,
    lowering::{
        backends::wasm::abi::{WasmAbiCarrier, WasmLibraryExportAbi},
        features::physical_contract::{PhysicalTextBoundary, PhysicalTextCarrier, UTF16_UNRESOLVED_TEXT_ABI},
    },
    target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe},
};

// ── 身份 newtype ──────────────────────────────────────────────────────────

/// SSA 值引用（wasm 私有）。渲染与公共 `ValueRef` 一致：`ValueRef(N)`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WasmValueKey(pub u32);

impl fmt::Debug for WasmValueKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ValueRef({})", self.0)
    }
}

/// 基本块引用（wasm 私有）。渲染与公共 `BlockRef` 一致：`BlockRef(N)`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WasmBlockKey(pub u32);

impl fmt::Debug for WasmBlockKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlockRef({})", self.0)
    }
}

/// 布局身份（wasm 私有）。公共 `LayoutId` 即 `u32` 别名，渲染为数字。
pub type WasmLayoutKey = u32;

/// 可调用实例身份（wasm 私有）。渲染与公共 `ItemInstanceId` 一致：`ItemInstanceId(N)`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WasmCallableKey(pub u32);

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
pub struct WasmNominalKey(pub u32);

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
pub struct WasmVariantKey(pub u32);

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
pub struct WasmFieldKey(pub u32);

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
pub struct WasmInstructionKey(pub u32);

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
pub struct WasmFloat64Bits(pub f64);

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
    pub fn into_inner(self) -> f64 {
        self.0
    }
}

/// 函数类型（wasm 私有镜像；变体名仍为 `Function`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WasmFunctionType {
    /// 形参类型，按声明顺序。
    pub params: Vec<WasmTypeFact>,
    /// 返回类型。
    pub return_type: WasmTypeFact,
}

/// wasm 目标类型事实（wasm 私有词汇，非源语言镜像）。
///
/// 标量与文本是目标编码基元；`Layout` 是 canonical 结算出的布局引用（堆类型
/// 与字段结构由冻结布局事实解析）；`Function` 是目标函数签名；
/// `Tuple` / `Array` / `FixedArray` / `Nullable` 是目标编码形状。
/// 源语言拼写、泛型应用、witness 表与匿名 union 不在本词汇中——
/// 它们必须由语言侧适配器结算为上列目标事实，否则在结算点结构化失败。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WasmTypeFact {
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
    /// 布局引用：名义实例 / 闭合泛型实例的堆类型与字段结构由该冻结布局解析。
    Layout(WasmLayoutKey),
    Function(Box<WasmFunctionType>),
    Tuple(Vec<WasmTypeFact>),
    Array(Box<WasmTypeFact>),
    FixedArray { element: Box<WasmTypeFact>, length: usize },
    Nullable(Box<WasmTypeFact>),
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
            Self::Layout(layout) => f.debug_tuple("Layout").field(layout).finish(),
            Self::Function(function) => f.debug_tuple("Function").field(&FunctionTypeDebug(function)).finish(),
            Self::Tuple(items) => f.debug_tuple("Tuple").field(items).finish(),
            Self::Array(element) => f.debug_tuple("Array").field(element).finish(),
            Self::FixedArray { element, length } => f.debug_struct("FixedArray").field("element", element).field("length", length).finish(),
            Self::Nullable(payload) => f.debug_tuple("Nullable").field(payload).finish(),
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
            WasmTypeFact::Layout(layout) => write!(f, "layout({layout})"),
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
            WasmTypeFact::Nullable(payload) => write!(f, "{payload}?"),
        }
    }
}

// ── 指令形状 ──────────────────────────────────────────────────────────────

/// 可执行常量（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmConstant {
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
pub enum WasmOperand {
    Value(WasmValueKey),
    Constant(WasmConstant),
    Item(WasmCallableKey),
    Symbol(NamePath),
}

/// 数组槽位初始化方式（wasm 私有镜像公共 `ArrayInitialization`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmArrayInit {
    Default,
    Fill(WasmOperand),
}

/// 可执行指令载荷（wasm 私有镜像语言侧操作事实，全部臂与字段名一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmOperation {
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
pub struct WasmInstruction {
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
pub fn primary_result(instruction: &WasmInstruction) -> Option<WasmValueKey> {
    instruction.results.first().copied()
}

/// 基本块终结符（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmTerminator {
    Return { value: Option<WasmOperand> },
    Jump { target: WasmBlockKey, arguments: Vec<WasmOperand> },
    Branch { condition: WasmOperand, then_target: WasmBlockKey, else_target: WasmBlockKey },
    Raise { payload: Option<WasmOperand>, resume_target: WasmBlockKey },
    Unreachable,
}

/// 基本块（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmBlock {
    pub id: WasmBlockKey,
    pub label: String,
    pub parameters: Vec<WasmValueKey>,
    pub instructions: Vec<WasmInstruction>,
    pub terminator: WasmTerminator,
}

/// SSA 值定义来源（wasm 私有镜像公共 `ValueOrigin`）。
#[derive(Clone, PartialEq, Eq)]
pub enum WasmValueOrigin {
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
pub struct WasmValue {
    pub id: WasmValueKey,
    pub origin: WasmValueOrigin,
}

impl fmt::Debug for WasmValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Value").field("id", &self.id).field("origin", &self.origin).finish()
    }
}

/// 函数体（wasm 私有镜像语言侧函数事实）。
///
/// `parameter_identities` 是从 `ValueOrigin::Parameter` 结算出的
/// `(声明下标, canonical 参数名)` 原始清单（未排序、未校验）；校验在消费点
/// 由 [`canonical_parameter_identities`] 完成，错误文案与公共实现逐字一致。
#[derive(Clone, PartialEq, Eq)]
pub struct WasmFunctionInput {
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
        f.debug_struct("WasmFunctionInput")
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

/// 入口参数身份校验（wasm 私有）。
///
/// 身份必须来自 `ValueOrigin::Parameter`，数量必须与 `param_types` 一致，下标
/// 必须连续且从 0 开始，参数名必须非空且不重复；任何偏离都在此结构化失败。
pub fn canonical_parameter_identities(instance_label: &str, function: &WasmFunctionInput) -> Result<Vec<(usize, String)>> {
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
pub enum WasmDispatchKind {
    Static,
    Indirect,
    Witness,
}

/// receiver 传参提示（wasm 私有镜像公共 `ReceiverPassingKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WasmReceiverPassingKind {
    ByValue,
    ByAddress,
}

// ── 布局 / 导入 / sum / singleton 事实 ────────────────────────────────────

/// 存储类（wasm 私有镜像公共 `StorageKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WasmStorageClass {
    Value,
    Reference,
}

impl WasmStorageClass {
    pub fn is_value(self) -> bool {
        matches!(self, Self::Value)
    }

    pub fn is_reference(self) -> bool {
        matches!(self, Self::Reference)
    }
}

/// 匿名聚合的结构键（wasm 私有镜像公共 `LayoutKey`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WasmLayoutKeyFact {
    Tuple(Vec<WasmTypeFact>),
    FixedArray { element: Box<WasmTypeFact>, length: usize },
}

/// 聚合字段物理布局（wasm 私有镜像公共 `ResolvedFieldLayout`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmFieldFact {
    pub name: String,
    pub ty: WasmTypeFact,
    pub offset: u32,
    pub size: u32,
    pub align: u32,
}

/// 聚合物理布局（wasm 私有镜像公共 `ResolvedAggregateLayout`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmLayoutFact {
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
pub struct WasmImportLinkFact {
    pub boundary: Identifier,
    pub platform_tag: Option<Identifier>,
    pub locator_segments: Vec<String>,
}

impl WasmImportLinkFact {
    pub fn matches_boundary(&self, boundary: &str) -> bool {
        self.boundary.as_str() == boundary
    }

    pub fn matches_platform_tag(&self, platform_tag: &str) -> bool {
        self.platform_tag.as_ref().is_some_and(|current| current.as_str() == platform_tag)
    }

    pub fn matches_host_platform(&self, platform_tag: &str) -> bool {
        self.matches_boundary("host") && self.matches_platform_tag(platform_tag)
    }

    pub fn locator_segments(&self) -> &[String] {
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
pub struct WasmImportFact {
    pub links: Vec<WasmImportLinkFact>,
    pub parameter_types: Vec<WasmTypeFact>,
    pub return_type: WasmTypeFact,
}

/// sum 变体的平台合同（wasm 私有镜像公共 `SumVariantRepresentation`；
/// payload 只以「是否存在」被消费，`TypeId` 身份不进私有词汇）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmSumVariantFact {
    pub tag: u32,
    pub has_payload: bool,
}

/// sum 平台合同（wasm 私有镜像公共 `SumRepresentation`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSumRep {
    pub nominal: WasmNominalKey,
    pub variants: BTreeMap<WasmVariantKey, WasmSumVariantFact>,
}

/// sum 变体布局（wasm 私有镜像公共 `SumVariantLayout`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSumVariantLayoutFact {
    pub id: WasmVariantKey,
    pub name: String,
    pub tag: u32,
    pub payload_type: Option<WasmTypeFact>,
}

/// sum 判别布局（wasm 私有镜像公共 `SumTypeLayout`；`identity` 由 settle 用
/// 私有 [`sum_identity_facts`] 预结算，编码层不再现场派生）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSumLayoutFact {
    pub nominal: WasmNominalKey,
    pub name: String,
    pub is_unite: bool,
    pub variants: Vec<WasmSumVariantLayoutFact>,
    pub identity: String,
}

/// 程序级文本载体表示（wasm 私有镜像公共 `ProgramTextRepresentation`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmProgramTextRepresentation {
    Raw,
    Managed,
}

/// singleton 实例生命周期事实（wasm 私有镜像公共 `SingletonInstancePlan`，
/// 并附 settle 解析出的物理尺寸；布局缺失保留为 `None`，由 lane 函数报 SMIR010）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSingletonFact {
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

// ── ABI 结构（wasm 私有）────────────────────────

/// 目标中立的 ABI 语义结构（wasm 私有镜像）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WasmAbiStructure {
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
    pub fn unsupported_reason(&self) -> Option<&str> {
        match self {
            Self::Unsupported { reason } => Some(reason),
            _ => None,
        }
    }
}

/// 具名聚合的结构描述：布局引用是必填身份，字段合同由冻结布局派生。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmAggregateStructure {
    pub layout: WasmLayoutKey,
    pub name: String,
    pub module_path: String,
    pub fields: Vec<WasmAbiField>,
}

/// 聚合字段结构（wasm 私有镜像公共 `AbiField`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmAbiField {
    pub name: String,
    pub ty: WasmAbiStructure,
}

/// sum 变体身份（wasm 私有镜像公共 `AbiVariant`）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WasmAbiVariant {
    pub name: String,
    pub tag: u32,
}

/// 宿主序列投影结构（wasm 私有镜像公共 `SequenceStructure`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSequenceStructure {
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
pub struct WasmSequenceLevel {
    pub layout: WasmLayoutKey,
    pub field_count: usize,
    pub items_slot: Option<usize>,
    pub length_slot: Option<usize>,
    pub inner_slot: Option<usize>,
}

/// sum 身份事实：类型名、是否 unite、全部变体名与判别 tag。
pub fn sum_identity_facts(name: &str, is_unite: bool, variants: &[WasmAbiVariant]) -> String {
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

/// 从冻结 sum 布局提取结构身份。
pub fn sum_variants_of(sum: &WasmSumLayoutFact) -> Vec<WasmAbiVariant> {
    sum.variants.iter().map(|variant| WasmAbiVariant { name: variant.name.clone(), tag: variant.tag }).collect()
}

/// 类型 canonical 拼写：数组物理名字的稳定身份种子。
pub fn canonical_type_spelling(ty: &WasmTypeFact) -> String {
    format!("{ty:?}")
}

/// 布局字段是否为整数计数字段候选。
fn is_integer_layout_type(ty: &WasmTypeFact) -> bool {
    matches!(
        ty,
        WasmTypeFact::Integer8 { .. } | WasmTypeFact::Integer16 { .. } | WasmTypeFact::Integer32 { .. } | WasmTypeFact::Integer64 { .. }
    )
}

/// 递归深度上限。
const ABI_STRUCTURE_MAX_DEPTH: usize = 32;

/// 嵌套存储层深度上限。
const HOST_PROJECTION_MAX_DEPTH: usize = 8;

/// 携带冻结布局事实的 ABI 结构派生（结算服务，归语言侧适配器在结算期调用；
/// 编码层只消费适配器提交的冻结 schema，不再现场重走本派生）。
///
/// 布局身份的唯一来源是 [`WasmTypeFact::Layout`]：名义实例与闭合泛型实例在
/// 适配器结算点就已绑定布局引用，这里只按身份解析字段合同、实例句柄或 sum
/// 身份；未注册布局结构化失败，绝不按拼写回查。
pub fn abi_structure_for_type(input: &WasmModuleInput, ty: &WasmTypeFact) -> WasmAbiStructure {
    abi_structure_with_depth(input, ty, 0)
}

fn abi_structure_with_depth(input: &WasmModuleInput, ty: &WasmTypeFact, depth: usize) -> WasmAbiStructure {
    if depth > ABI_STRUCTURE_MAX_DEPTH {
        return WasmAbiStructure::Unsupported {
            reason: format!("ABI 结构递归超过 {ABI_STRUCTURE_MAX_DEPTH} 层，拒绝无界展开 `{ty:?}`"),
        };
    }
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
        WasmTypeFact::Array(element) => WasmAbiStructure::Array {
            element: Box::new(abi_structure_with_depth(input, element, depth + 1)),
            element_key: canonical_type_spelling(element),
        },
        WasmTypeFact::FixedArray { element, length } => {
            WasmAbiStructure::FixedArray { element: Box::new(abi_structure_with_depth(input, element, depth + 1)), length: *length }
        }
        WasmTypeFact::Nullable(inner) => WasmAbiStructure::Option { inner: Box::new(abi_structure_with_depth(input, inner, depth + 1)) },
        WasmTypeFact::Tuple(fields) => WasmAbiStructure::Tuple {
            fields: fields.iter().map(|field| abi_structure_with_depth(input, field, depth + 1)).collect(),
        },
        WasmTypeFact::Function(_) => WasmAbiStructure::Unsupported { reason: "函数类型不能出现在库导出 ABI 位置".to_string() },
        // sum 的物理布局是 lowering 冻结的 `[i32 tag, anyref payload]`：零载荷与带
        // 载荷变体都由同一份 sum 布局派生，目标层据此解析真实 sum 布局。
        WasmTypeFact::Layout(layout) => {
            if let Some(sum) = input.sum_layouts_by_type.get(layout) {
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
            aggregate_structure(input, *layout)
        }
    }
}

/// 引用存储名义实例保留共享身份；值存储聚合按字段结构投影。
///
/// 名字与命名空间的唯一来源是冻结布局事实（`namespace` 的 `.` 规范为 `::`，
/// 与 canonical 拼写折叠同源），不再从任何类型拼写拆分。
fn aggregate_structure(input: &WasmModuleInput, layout_id: WasmLayoutKey) -> WasmAbiStructure {
    let Some(aggregate) = input.layout_by_id(layout_id)
    else {
        return WasmAbiStructure::Unsupported { reason: format!("layout `{layout_id}` 未注册，无法派生字段访问合同") };
    };
    let name = aggregate.name.clone();
    let module_path = aggregate.namespace.replace('.', "::");
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
        let field_structure = abi_structure_for_type(input, &field.ty);
        if let Some(reason) = field_structure.unsupported_reason() {
            return WasmAbiStructure::Unsupported {
                reason: format!("聚合 `{}::{}` 字段 `{}` 没有 ABI 合同: {reason}", aggregate.namespace, aggregate.name, field.name),
            };
        }
        fields.push(WasmAbiField { name: field.name.clone(), ty: field_structure });
    }
    WasmAbiStructure::Aggregate(WasmAggregateStructure { layout: layout_id, name, module_path, fields })
}

/// 为已声明 `[host_projection]` 的聚合构造宿主序列投影。
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
            let element_structure = abi_structure_for_type(input, element);
            if let Some(reason) = element_structure.unsupported_reason() {
                return unsupported(format!("元素类型没有 ABI 合同: {reason}"));
            }
            let Some(root_length_slot) = levels[0].length_slot
            else {
                return unsupported("根层没有整数计数字段，无法读回长度".to_string());
            };
            let length_ty_structure = abi_structure_for_type(input, &root.fields[root_length_slot].ty);
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
        // 非叶子层：沿唯一携带布局引用的名义字段下降。
        let inner_slots: Vec<(usize, WasmLayoutKey)> =
            current
                .fields
                .iter()
                .enumerate()
                .filter_map(|(index, field)| match field.ty {
                    WasmTypeFact::Layout(id) => Some((index, id)),
                    _ => None,
                })
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

// ── 文本 ABI / GC 载体 / operator 载体 ────────────────────────────────────

/// 未落地文本 ABI 载体的统一拒绝理由。
pub fn unresolved_text_abi_reason(ty: &WasmTypeFact) -> Option<&'static str> {
    match ty {
        WasmTypeFact::Utf16 => Some(UTF16_UNRESOLVED_TEXT_ABI),
        _ => None,
    }
}

/// 文本 ABI 事实查询（镜像公共 `declared_text_projection` 的载体结论）。
pub fn declared_text_projection(boundary: PhysicalTextBoundary, ty: &WasmTypeFact) -> Option<PhysicalTextCarrier> {
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
pub fn wasm_gc_value_carrier_valtype(ty: &WasmTypeFact) -> Option<u8> {
    use data_binary_wasm::{VALTYPE_ANYREF, VALTYPE_F32, VALTYPE_F64, VALTYPE_I32, VALTYPE_I64};
    Some(match ty {
        WasmTypeFact::Utf16 => return None,
        WasmTypeFact::Float32 => VALTYPE_F32,
        WasmTypeFact::Float64 => VALTYPE_F64,
        WasmTypeFact::Integer64 { .. } | WasmTypeFact::Integer128 { .. } => VALTYPE_I64,
        WasmTypeFact::Layout(_)
        | WasmTypeFact::Function(_)
        | WasmTypeFact::Array(_)
        | WasmTypeFact::Nullable(_) => VALTYPE_ANYREF,
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
pub fn wasm_js_glue_value_carrier_valtype(ty: &WasmTypeFact) -> Option<u8> {
    match ty {
        WasmTypeFact::Utf8 | WasmTypeFact::Utf16 => Some(data_binary_wasm::VALTYPE_I32),
        other => wasm_gc_value_carrier_valtype(other),
    }
}

/// 将类型事实映射到 wasm-gc 字段/元素值类型字节（镜像 `mir::representation` 同名表）。
pub fn wasm_gc_field_type_byte(ty: &WasmTypeFact) -> Result<u8> {
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
pub fn operand_carrier(ty: &WasmTypeFact) -> Option<TargetOperandType> {
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
pub fn result_carrier(ty: &WasmTypeFact) -> Option<TargetOperandType> {
    match ty {
        WasmTypeFact::Boolean => Some(TargetOperandType::I32),
        _ => operand_carrier(ty),
    }
}

/// 校验 lane 冻结的 operator 计划与导入声明签名一致（镜像公共同名函数）。
pub fn validate_frozen_operator<E>(
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

/// Wasm 物理导出 ABI 的私有派生点：只消费本模块私有词汇。
pub fn wasm_export_abi(input: &WasmModuleInput, instance: &WasmCallableKey) -> Result<WasmLibraryExportAbi> {
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
        WasmTypeFact::Layout(_) => Ok(WasmAbiCarrier::Handle),
        WasmTypeFact::Integer128 { .. } | WasmTypeFact::Utf16 | WasmTypeFact::Bottom | WasmTypeFact::Function(_) => {
            Err(format!("Wasm 导出面没有该类型的稳定 ABI 载体: {ty}"))
        }
    }
}

/// 数据流标准逆后序可达块（镜像 `shared::executable::collect_reachable_blocks`）。
pub fn collect_reachable_blocks(function: &WasmFunctionInput) -> Vec<WasmBlockKey> {
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
pub struct WasmModuleInput {
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
    /// 布局身份 → sum 布局（含预结算 identity）；类型事实经 `Layout` 引用命中。
    pub sum_layouts_by_type: BTreeMap<WasmLayoutKey, WasmSumLayoutFact>,
    /// sum 平台合同（tag / payload 有无）。
    pub sum_reps: BTreeMap<WasmNominalKey, WasmSumRep>,
    /// 程序级文本载体表示。
    pub program_text_representation: Option<WasmProgramTextRepresentation>,
    /// singleton 生命周期事实（附物理尺寸，缺失为 `None`）。
    pub singleton_facts: Vec<WasmSingletonFact>,
    /// `[host_projection]` 声明授权的宿主序列布局集合。
    pub host_sequence_projections: BTreeSet<WasmLayoutKey>,
    /// 适配器在结算期冻结的库导出 ABI 合同（载体 + 参数/返回 schema）。
    /// 编码层只消费冻结事实，不再现场重走 ABI 派生。
    pub export_abis: BTreeMap<WasmCallableKey, WasmFrozenExportAbi>,
    /// 语言 × 目标适配器显式登记的宿主导入 provider（JS glue 边界）。
    /// 语言特有宿主行为（如文本 std 操作）由 provider 提交，glue 不内建。
    pub host_import_providers: Vec<WasmHostImportProviderFact>,
}

/// provider 物理载体 → wasm valtype 字节（`Handle` 是 i32 宿主句柄；`Void` 无槽位）。
pub fn wasm_host_provider_carrier_valtype(carrier: WasmAbiCarrier) -> Option<u8> {
    use data_binary_wasm::{VALTYPE_F64, VALTYPE_I32, VALTYPE_I64};
    match carrier {
        WasmAbiCarrier::I32 | WasmAbiCarrier::Handle => Some(VALTYPE_I32),
        WasmAbiCarrier::I64 => Some(VALTYPE_I64),
        WasmAbiCarrier::F32 => Some(crate::lowering::backends::wasm::gc::VALTYPE_F32),
        WasmAbiCarrier::F64 => Some(VALTYPE_F64),
        WasmAbiCarrier::Void => None,
    }
}

/// 单个库导出的冻结 ABI 合同（结算服务在适配器结算期产出，编码层直接消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmFrozenExportAbi {
    /// 物理载体 ABI（`wasm_export_abi` 结算结果）。
    pub carriers: WasmLibraryExportAbi,
    /// 逐参数的冻结 schema。
    pub params: Vec<crate::lowering::backends::wasm::mir::abi_schema::WasmAbiType>,
    /// 冻结返回 schema。
    pub returns: crate::lowering::backends::wasm::mir::abi_schema::WasmAbiType,
}

/// 宿主导入 provider 事实：语言侧登记字段、物理签名与 JS 实现文本。
///
/// `implementation` 是导入对象的 JS 实现表达式（箭头函数），`helpers` 是随
/// launcher 注入的辅助 JS 声明；glue 只保留通用装载/编组，不内建语言行为。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmHostImportProviderFact {
    /// 导入模块名（如 `env`）。
    pub module: String,
    /// 导入字段名（如 `utf8_concat`）。
    pub field: String,
    /// 物理参数载体（`Handle` 表示 i32 句柄）。
    pub params: Vec<WasmAbiCarrier>,
    /// 物理返回载体（空表示无返回）。
    pub results: Vec<WasmAbiCarrier>,
    /// JS 实现表达式。
    pub implementation: String,
    /// 辅助 JS 声明（可空串）。
    pub helpers: String,
}

impl WasmModuleInput {
    /// 按布局身份查物理布局。
    pub fn layout_by_id(&self, id: WasmLayoutKey) -> Option<&WasmLayoutFact> {
        self.layouts.iter().find(|layout| layout.id == id)
    }

    /// 按名义实例身份查物理布局。
    pub fn layout_by_nominal(&self, nominal: WasmNominalKey) -> Option<&WasmLayoutFact> {
        self.layout_by_nominal_id.get(&nominal).and_then(|id| self.layout_by_id(*id))
    }

    /// 按字段身份查 `(布局, 字段, 槽位)`。
    pub fn field_layout_by_id(&self, field: WasmFieldKey) -> Option<(&WasmLayoutFact, &WasmFieldFact, u32)> {
        let (layout_id, slot) = *self.layout_by_field_id.get(&field)?;
        let layout = self.layout_by_id(layout_id)?;
        let field = layout.fields.get(slot as usize)?;
        Some((layout, field, slot))
    }

    /// 按实例身份查函数体。
    pub fn function(&self, instance: &WasmCallableKey) -> Option<&WasmFunctionInput> {
        self.functions.get(instance)
    }

    /// 实例顺序清单。
    pub fn instances(&self) -> &[WasmCallableKey] {
        &self.instances
    }

    /// 导出公开名：注解名优先，缺省用 `[export]` 声明名。
    pub fn export_public_name(&self, instance: &WasmCallableKey) -> Option<&String> {
        self.export_annotations.get(instance).or_else(|| self.export_names.get(instance))
    }

    /// 测试专用：全空结算输入，供直接构造私有事实的单元测试使用。
    #[cfg(test)]
    pub fn testing_empty() -> Self {
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
            sum_layouts_by_type: BTreeMap::new(),
            sum_reps: BTreeMap::new(),
            program_text_representation: None,
            singleton_facts: Vec::new(),
            host_sequence_projections: BTreeSet::new(),
            export_abis: BTreeMap::new(),
            host_import_providers: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn testing_function(key: WasmCallableKey, symbol: &str, return_type: WasmTypeFact) -> WasmFunctionInput {
        WasmFunctionInput {
            symbol: symbol.to_string(),
            return_type,
            param_types: Vec::new(),
            value_types: BTreeMap::new(),
            value_layouts: BTreeMap::new(),
            return_layout: None,
            entry: WasmBlockKey(0),
            values: Vec::new(),
            blocks: Vec::new(),
            parameter_identities: Vec::new(),
        }
    }

    #[test]
    fn float32_has_independent_carrier() {
        let mut input = WasmModuleInput::testing_empty();
        input.functions.insert(WasmCallableKey(7), testing_function(WasmCallableKey(7), "f", WasmTypeFact::Float32));
        let abi = wasm_export_abi(&input, &WasmCallableKey(7)).expect("Float32 导出 ABI");
        assert_eq!(abi.params, Vec::new());
        assert_eq!(abi.returns, WasmAbiCarrier::F32);
        assert_eq!(abi.returns.as_str(), "f32");
    }

    #[test]
    fn utf16_fails_closed() {
        let mut input = WasmModuleInput::testing_empty();
        input.functions.insert(WasmCallableKey(8), testing_function(WasmCallableKey(8), "g", WasmTypeFact::Utf16));
        let error = wasm_export_abi(&input, &WasmCallableKey(8)).expect_err("UTF-16 不得进入 Wasm 物理 ABI");
        assert!(error.contains("Utf16") || error.contains("utf16"), "{error}");
    }
}
