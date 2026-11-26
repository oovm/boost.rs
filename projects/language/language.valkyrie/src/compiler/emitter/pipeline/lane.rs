//! 五条后端 lane 共享的适配上下文：一次结算所有 lane 无关的链接、布局与导出事实。
//!
//! 拆除公共 IR（God IR）后，每条 lane 只写自己的指令映射，不再各自重复投影
//! canonical 事实。本模块从机器码 lane 的 `MachineContext` 提炼而来：保留其
//! 可达闭包、导入解析与逐函数合同校验，并补齐旧公共计划构建路径中
//! 各 lane 都需要的布局表、导出面与文本表示事实。导出面事实
//! （export_names / export_annotations / exported_operations / entry_operation /
//! required_capabilities）经 [`LaneContext::fragment`] 可得，不在这里复制；
//! `theory_bundle` 只被 wasm lane 作为独立参数消费，不进本上下文。
//! 五条 lane 全部消费本上下文：语言 × 目标适配的阶段枚举、结构化错误与
//! canonical 类型投影也归本模块所有，不再有独立的公共 IR 降层模块。

use std::collections::{BTreeMap, BTreeSet};

use compiler_emitter::{BackendImport, ProgramTextRepresentation, SumRepresentation, SumVariantRepresentation};
use compiler_types::{
    CanonicalPrimitiveType, CanonicalTypeKind, FunctionType, ItemInstanceId, QualifiedName, SemanticType, SumTypeLayout, ValueIdentity,
    semantic_ids::{ImportIndex, MirValueId, NominalInstanceId, TypeId, VariantId},
};

use super::super::super::valkyrie_mir::{
    CompiledValkyrieProgram, InvokeLowering, TextRepresentation as PlannedTextRepresentation, ValkyrieMirCallee, ValkyrieMirEffectKind,
    ValkyrieMirFunction, ValkyrieMirOperation, ValkyrieMirProgram, ValkyrieMirTerminator, ValueRepresentation,
};

type Result<T> = std::result::Result<T, String>;

/// 适配失败所在的降层阶段：下游按枚举归因，不再解析文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterStage {
    /// 链接事实一致性：片段身份、导入绑定、根闭包与 ABI 名称。
    Link,
    /// canonical 类型到目标类型的投影。
    Type,
    /// 函数级结构与函数体降层。
    Function,
    /// 表示与布局事实：载体合同、sum tag、文本编码登记。
    Representation,
}

impl AdapterStage {
    /// 阶段标签。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Type => "type",
            Self::Function => "function",
            Self::Representation => "representation",
        }
    }
}

impl std::fmt::Display for AdapterStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 语言 × 目标适配失败：片段/根身份缺失有独立变体，其余降层原因携带
/// 阶段、来源实例与符号，供机械归因；细节文本保留在 `reason`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetPlanError {
    /// 片段身份不在链接结果中。
    UnknownFragment {
        /// 请求的片段身份。
        fragment: String,
    },
    /// 可达根既不是函数体，也不是已声明导入。
    UnknownRoot {
        /// 请求的根实例。
        instance: ItemInstanceId,
    },
    /// 语言合同无法无损投影到目标输入。
    Unsound {
        /// 失败所在的降层阶段。
        stage: AdapterStage,
        /// 出错的可调用实例（阶段不涉及具体实例时为 `None`）。
        instance: Option<ItemInstanceId>,
        /// 出错的可调用 ABI 名称（若已解析）。
        symbol: Option<String>,
        /// 具体原因。
        reason: String,
    },
}

impl TargetPlanError {
    /// 构造携带阶段与来源身份的 `Unsound` 失败。
    fn unsound(stage: AdapterStage, instance: Option<ItemInstanceId>, symbol: Option<String>, reason: String) -> Self {
        Self::Unsound { stage, instance, symbol, reason }
    }
}

impl std::fmt::Display for TargetPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownFragment { fragment } => write!(f, "Valkyrie 片段 `{fragment}` 不存在"),
            Self::UnknownRoot { instance } => write!(f, "Compiler callable 实例 `{instance:?}` 缺少函数体"),
            Self::Unsound { stage, instance, symbol, reason } => {
                write!(f, "target-plan[{stage}]")?;
                if let Some(instance) = instance {
                    write!(f, " instance={instance:?}")?;
                }
                if let Some(symbol) = symbol {
                    write!(f, " symbol={symbol}")?;
                }
                write!(f, ": {reason}")
            }
        }
    }
}

impl std::error::Error for TargetPlanError {}

/// 适配的失败闭合：链接事实与语言合同无法无损投影时按阶段归因。
fn unsound(stage: AdapterStage, instance: Option<ItemInstanceId>, symbol: Option<String>, reason: String) -> TargetPlanError {
    TargetPlanError::unsound(stage, instance, symbol, reason)
}

/// 一个可达函数实例在 lane 侧的预解析视图：语义类型与声明名已冻结，
/// lowering 期不再重复投影 `TypeId`。
pub(crate) struct FunctionView {
    /// ABI 符号名（错误文案的 function 上下文）。
    pub(crate) symbol: String,
    /// 形参语义类型，按声明顺序。
    pub(crate) param_types: Vec<SemanticType>,
    /// 返回语义类型。
    pub(crate) return_type: SemanticType,
    /// SSA 值 → 语义类型（含块参数与指令结果）。
    pub(crate) value_types: BTreeMap<MirValueId, SemanticType>,
    /// canonical 冻结的声明参数名，按位置。
    pub(crate) parameter_names: Vec<String>,
}

/// lane 共享上下文：一次构建，各交付入口复用。
///
/// 字段面与旧公共计划输入同源（除函数体的公共 IR 投影与
/// `theory_bundle`）：lane 无关的链接/布局/导出事实在这里一次结算，
/// 各 lane 只按自己的合同消费其中子集。共享布局/文本事实面是为本阶段
/// 尚未切换的 lane 预留的，机器码 lane 暂不读取。
#[allow(dead_code)]
pub(crate) struct LaneContext<'a> {
    /// 链接后的 canonical 程序（类型、布局、导入与宿主事实的唯一来源）。
    canonical: &'a ValkyrieMirProgram,
    /// 交付片段身份：导出面、注解与入口操作。
    fragment: &'a compiler_types::CanonicalFragment,
    /// lane 可达闭包：导出 + 入口为根，沿 `Invoke{Item}` 边按最小未访问优先遍历。
    reachable: Vec<ItemInstanceId>,
    /// 闭包内函数实例的 ABI 名；导入实例不在此表（旧路径同样不给导入登记符号）。
    abi_names: BTreeMap<ItemInstanceId, QualifiedName>,
    /// 闭包内已解析的宿主导入（operator 调用合同）。
    imports: BTreeMap<ItemInstanceId, BackendImport>,
    /// 闭包内函数实例的预解析视图。
    views: BTreeMap<ItemInstanceId, FunctionView>,
    /// 聚合布局计划；与公共路径一致，已注入语义类型布局索引供按冻结身份结算闭合 `Apply`。
    aggregate_layouts: compiler_types::AggregateLayoutPlan,
    /// nominal → 布局身份。
    aggregate_layout_by_nominal: BTreeMap<NominalInstanceId, compiler_types::LayoutId>,
    /// 字段 → (布局身份, 槽位)。
    aggregate_layout_by_field: BTreeMap<compiler_types::FieldId, (compiler_types::LayoutId, u32)>,
    /// 字段声明值类型 → 冻结布局身份。
    field_type_layouts: BTreeMap<compiler_types::FieldId, compiler_types::LayoutId>,
    /// 完整类型身份 → 布局身份。
    aggregate_layout_by_type: BTreeMap<TypeId, compiler_types::LayoutId>,
    /// 结构化类型 → 布局身份（含契约归一键、未结算键与文本编码折叠键）。
    aggregate_layout_by_semantic_type: BTreeMap<SemanticType, compiler_types::LayoutId>,
    /// flags 类型布局。
    flags_types: Vec<compiler_types::FlagsLayout>,
    /// 单例实例计划。
    singleton_instances: Vec<compiler_types::SingletonInstancePlan>,
    /// 程序级文本表示；`None` 表示生产者未声明该事实，lane 按 managed 语义失败闭合。
    program_text_representation: Option<ProgramTextRepresentation>,
    /// 语义类型 → sum 布局（unite 表示）。
    sum_layout_by_semantic_type: BTreeMap<SemanticType, SumTypeLayout>,
    /// sum 实例的冻结 tag/payload 表示。
    sum_reps: BTreeMap<NominalInstanceId, SumRepresentation>,
}

impl LaneContext<'_> {
    /// 链接事实：类型登记、聚合布局计划与按字段/按类型的布局绑定。
    pub(crate) fn linked(&self) -> &compiler_types::LinkedSemanticProgram {
        &self.canonical.linked
    }

    /// 交付片段的导出面、注解与入口操作。
    pub(crate) fn fragment(&self) -> &compiler_types::CanonicalFragment {
        self.fragment
    }

    /// 可达闭包（镜像符号布局的发射顺序，必须与旧路径一致）。
    pub(crate) fn reachable(&self) -> &[ItemInstanceId] {
        &self.reachable
    }

    /// 已解析宿主导入（operator 调用合同）。
    pub(crate) fn imports(&self) -> &BTreeMap<ItemInstanceId, BackendImport> {
        &self.imports
    }

    /// 闭包内函数实例的 ABI 名（导入实例无登记，与旧路径一致）。
    pub(crate) fn abi_name(&self, instance: &ItemInstanceId) -> Option<&QualifiedName> {
        self.abi_names.get(instance)
    }

    /// 函数实例的预解析视图。
    pub(crate) fn view(&self, instance: &ItemInstanceId) -> Option<&FunctionView> {
        self.views.get(instance)
    }

    /// MIR 函数体。
    pub(crate) fn function(&self, instance: &ItemInstanceId) -> Option<&ValkyrieMirFunction> {
        self.canonical.mir.functions.get(instance)
    }

    /// 语义类型 → sum 布局。
    pub(crate) fn sum_layout_by_semantic_type(&self) -> &BTreeMap<SemanticType, SumTypeLayout> {
        &self.sum_layout_by_semantic_type
    }

    /// sum 实例冻结表示。
    pub(crate) fn sum_representations(&self) -> &BTreeMap<NominalInstanceId, SumRepresentation> {
        &self.sum_reps
    }

    /// canonical 类型投影（指令携带 `TypeId` 的按需解析）。
    pub(crate) fn lower_type(&self, id: TypeId) -> Result<SemanticType> {
        lower_type(self.canonical, id)
    }
}

/// 共享布局与文本事实面访问器：本阶段一次结算，机器码 lane 不读取；
/// 其余四条 lane 切换到 `LaneContext` 时按各自合同消费，故不视为死代码。
#[allow(dead_code)]
impl LaneContext<'_> {
    /// 聚合布局计划（含注入的语义类型布局索引）。
    pub(crate) fn aggregate_layouts(&self) -> &compiler_types::AggregateLayoutPlan {
        &self.aggregate_layouts
    }

    /// nominal → 布局身份。
    pub(crate) fn aggregate_layout_by_nominal(&self) -> &BTreeMap<NominalInstanceId, compiler_types::LayoutId> {
        &self.aggregate_layout_by_nominal
    }

    /// 字段 → (布局身份, 槽位)。
    pub(crate) fn aggregate_layout_by_field(&self) -> &BTreeMap<compiler_types::FieldId, (compiler_types::LayoutId, u32)> {
        &self.aggregate_layout_by_field
    }

    /// 字段声明值类型 → 冻结布局身份。
    pub(crate) fn field_type_layouts(&self) -> &BTreeMap<compiler_types::FieldId, compiler_types::LayoutId> {
        &self.field_type_layouts
    }

    /// 完整类型身份 → 布局身份。
    pub(crate) fn aggregate_layout_by_type(&self) -> &BTreeMap<TypeId, compiler_types::LayoutId> {
        &self.aggregate_layout_by_type
    }

    /// 结构化类型 → 布局身份。
    pub(crate) fn aggregate_layout_by_semantic_type(&self) -> &BTreeMap<SemanticType, compiler_types::LayoutId> {
        &self.aggregate_layout_by_semantic_type
    }

    /// flags 类型布局。
    pub(crate) fn flags_types(&self) -> &[compiler_types::FlagsLayout] {
        &self.flags_types
    }

    /// 单例实例计划。
    pub(crate) fn singleton_instances(&self) -> &[compiler_types::SingletonInstancePlan] {
        &self.singleton_instances
    }

    /// 程序级文本表示模式。
    pub(crate) fn program_text_representation(&self) -> Option<ProgramTextRepresentation> {
        self.program_text_representation
    }
}

/// 从已编译程序构建 lane 共享上下文：片段/根身份、导入唯一性、可达闭包、
/// 逐函数合同校验与类型投影、布局与 sum 表示表。
///
/// 该阶段对应旧公共计划构建路径的链接与函数降层合同校验；失败按
/// `TargetPlanError` 结构化返回，装配层保留原有的分区前缀文案。
pub(crate) fn build_lane_context<'a>(
    program: &'a CompiledValkyrieProgram,
    fragment_id: &compiler_target::Identifier,
    roots: &[ItemInstanceId],
) -> std::result::Result<LaneContext<'a>, TargetPlanError> {
    let canonical = program.mir_program();
    if !canonical.linked.fragments.contains_key(fragment_id) {
        return Err(TargetPlanError::UnknownFragment { fragment: fragment_id.to_string() });
    }
    for root in roots {
        if !canonical.mir.functions.contains_key(root) {
            return Err(TargetPlanError::UnknownRoot { instance: *root });
        }
    }
    let fragment = canonical
        .linked
        .fragments
        .get(fragment_id)
        .ok_or_else(|| unsound(AdapterStage::Link, None, None, format!("Canonical 片段 `{fragment_id}` 不存在")))?;
    // 导入声明唯一性：同一实例不得同时绑定函数体与导入，也不得绑定多个 ImportIndex。
    let mut declared_imports: BTreeMap<ItemInstanceId, (ImportIndex, &compiler_types::ImportRecord)> = BTreeMap::new();
    for (index, import) in &canonical.linked.imports {
        if canonical.mir.functions.contains_key(&import.callee) {
            return Err(unsound(
                AdapterStage::Link,
                Some(import.callee),
                None,
                format!("callable 实例 `{:?}` 同时绑定函数体与导入", import.callee),
            ));
        }
        if declared_imports.insert(import.callee, (*index, import)).is_some() {
            return Err(unsound(
                AdapterStage::Link,
                Some(import.callee),
                None,
                format!("callable 实例 `{:?}` 绑定多个 ImportIndex", import.callee),
            ));
        }
    }
    // lane 可达闭包：与旧 collect_reachable_functions 相同的遍历顺序——
    // 每轮取最小未访问实例，沿 `Invoke{Item}` 且「有函数体且非导入」的边扩展。
    let mut pending_roots: BTreeSet<ItemInstanceId> = fragment
        .export_names
        .keys()
        .chain(fragment.exported_operations.iter())
        .copied()
        .collect();
    if let Some(entry) = fragment.entry_operation {
        pending_roots.insert(entry);
    }
    let mut visited = BTreeSet::new();
    let mut reachable = Vec::new();
    // 闭包引用到的导入 callee：不进 `reachable`（它们没有函数体），但导入签名
    // 事实必须结算，与旧公共路径 `pending.extend(callees)` 后逐 callee 解析同源。
    let mut external_refs = BTreeSet::new();
    loop {
        let Some(instance) = pending_roots.iter().copied().find(|instance| !visited.contains(instance)) else {
            break;
        };
        visited.insert(instance);
        let Some(function) = canonical.mir.functions.get(&instance) else {
            continue;
        };
        for block in function.blocks.values() {
            for instruction in &block.instructions {
                match &instruction.operation {
                    ValkyrieMirOperation::Invoke { callee: ValkyrieMirCallee::Item(callee), .. } => {
                        if declared_imports.contains_key(callee) {
                            external_refs.insert(*callee);
                        }
                        else if canonical.mir.functions.contains_key(callee) {
                            pending_roots.insert(*callee);
                        }
                    }
                    // 闭包值（`LoadFunctionRef`）不经静态调用边，但代码指针同样是必须
                    // 链接的事实；旧公共路径经 `lower_operation` 把它登记进闭包，
                    // OVM 闭包载体要求 instance 属于本模块函数表。
                    ValkyrieMirOperation::LoadFunctionRef { instance: referenced, .. } => {
                        if declared_imports.contains_key(referenced) {
                            external_refs.insert(*referenced);
                        }
                        else if canonical.mir.functions.contains_key(referenced) {
                            pending_roots.insert(*referenced);
                        }
                    }
                    _ => {}
                }
            }
        }
        reachable.push(instance);
    }
    // 闭包内逐实例：导入解析签名，函数体校验合同并预解析类型视图。
    let mut imports = BTreeMap::new();
    let mut abi_names = BTreeMap::new();
    let mut views = BTreeMap::new();
    let resolve_import = |instance: &ItemInstanceId, index: &ImportIndex, import: &compiler_types::ImportRecord| {
        let parameter_types = import
            .parameter_types
            .iter()
            .map(|ty| lower_type(canonical, *ty).map_err(|reason| unsound(AdapterStage::Type, Some(*instance), None, reason)))
            .collect::<std::result::Result<Vec<_>, TargetPlanError>>()?;
        let return_type = lower_type(canonical, import.return_type)
            .map_err(|reason| unsound(AdapterStage::Type, Some(*instance), None, reason))?;
        Ok::<BackendImport, TargetPlanError>(BackendImport { index: *index, links: import.links.clone(), parameter_types, return_type })
    };
    for instance in &reachable {
        if let Some((index, import)) = declared_imports.get(instance) {
            imports.insert(*instance, resolve_import(instance, index, import)?);
            continue;
        }
        let name = canonical.linked.callable_names.get(instance).ok_or_else(|| {
            unsound(AdapterStage::Link, Some(*instance), None, format!("callable 实例 `{instance:?}` 缺少 ABI 名称"))
        })?;
        abi_names.insert(*instance, name.clone());
        let function = canonical.mir.functions.get(instance).ok_or_else(|| {
            unsound(AdapterStage::Link, Some(*instance), Some(name.to_string()), format!("callable `{name}` 缺少 canonical 函数体"))
        })?;
        // 可达调用的目标必须已有闭合实例。签名仍含 Generic 的声明模板没有被前端证明
        // 不可达，只是从前端单态化里漏掉；把它从闭包队列里滤掉等于把这处缺口伪装成
        // 编译成功，因此这里失败闭合，要求前端提供单态化/evidence 合同。
        if signature_contains_generic(canonical, function) {
            return Err(unsound(
                AdapterStage::Function,
                Some(*instance),
                Some(name.to_string()),
                format!(
                    "可达调用的目标缺少闭合实例: instance={instance:?} symbol={name} 的声明签名仍含 Generic 类型参数，\
                     backend 无法编码；需前端提供单态化/evidence 合同"
                ),
            ));
        }
        let view = build_function_view(program, canonical, function)
            .map_err(|reason| unsound(AdapterStage::Function, Some(*instance), Some(name.to_string()), reason))?;
        views.insert(*instance, view);
    }
    // 闭包引用到的导入 callee：签名事实与旧公共路径同源结算（外部调用 arity 与
    // 实参位宽只认这张冻结表，禁止消费点回落实参数量猜测）。
    for instance in &external_refs {
        if imports.contains_key(instance) {
            continue;
        }
        if let Some((index, import)) = declared_imports.get(instance) {
            imports.insert(*instance, resolve_import(instance, index, import)?);
        }
    }
    // 字段声明侧的身份：布局计划只记构造器拼写，SSA 记结算布局；字段合同
    // 必须以 canonical 为两侧冻结的同一布局身份比对，而不是比较拼写形态。
    let mut field_type_layouts = BTreeMap::new();
    for (field_id, record) in &canonical.linked.fields {
        if let Some(layout_id) = canonical.linked.aggregate_layout_by_type.get(&record.ty) {
            field_type_layouts.insert(*field_id, *layout_id);
        }
    }
    // 语义类型 → 布局身份折叠表：与旧 build_plan 相同的键归一规则。
    let mut aggregate_layout_by_semantic_type = BTreeMap::new();
    for (type_id, layout_id) in &canonical.linked.aggregate_layout_by_type {
        // lower_type 只对“可执行类型”有结果；函数签名等非值类型跳过即可，
        // 它们不会成为容器元素或导出位置。
        if let Ok(ty) = lower_type(canonical, *type_id) {
            aggregate_layout_by_semantic_type.insert(ty.clone(), *layout_id);
            // 同一实例类型有两种冻结形态：SSA/签名已结算为携带布局身份的
            // `Nominal`，而布局计划的字段事实仍以未结算的 `Apply` 记录。
            // 两者必须指向同一布局，否则字段合同会因形态差异误判为不同类型。
            // 键按 `contract_type_key` 归一（抹掉诊断拼写），两侧同一键即同一类型。
            aggregate_layout_by_semantic_type.insert(ty.contract_key(), *layout_id);
            if let Ok(unsettled) = lower_unsettled_contract_key(canonical, *type_id) {
                aggregate_layout_by_semantic_type.insert(unsettled, *layout_id);
            }
        }
        // 文本载体声明（`std.text::Utf8Text`/`Utf16Text`）在签名里折叠成编码基元，
        // 槽类型是 `SemanticType::Utf8`/`Utf16` 而不是名义拼写；托管布局必须同时以
        // 折叠后的编码身份为键登记，否则按编码身份取槽的 ABI 派生点查不到布局，
        // 会把托管槽误解释成 raw 串。键来源是 canonical 已冻结的
        // `text_encoding_types`（01 §5.4：文本身份只来自唯一已冻结编码身份），
        // 不按名字拼写回查。
        if let Some(encoding) = canonical.linked.text_encoding_types.get(type_id) {
            let folded = match encoding {
                CanonicalPrimitiveType::Utf8 => SemanticType::Utf8,
                CanonicalPrimitiveType::Utf16 => SemanticType::Utf16,
                other => {
                    return Err(unsound(
                        AdapterStage::Representation,
                        None,
                        None,
                        format!("文本载体 `TypeId` 登记了非文本编码身份: {other:?}"),
                    ));
                }
            };
            if let Some(existing) = aggregate_layout_by_semantic_type.get(&folded).copied() {
                if existing != *layout_id {
                    return Err(unsound(
                        AdapterStage::Representation,
                        None,
                        None,
                        format!("文本编码身份 `{folded:?}` 登记了冲突的托管布局: {existing:?} vs {layout_id:?}"),
                    ));
                }
            }
            aggregate_layout_by_semantic_type.insert(folded, *layout_id);
        }
    }
    // unite 没有物理布局身份，只能按已冻结的名义实例类型回查；因此用
    // `nominal_instances` 的声明 TypeId 逐一 `lower_type`，不按名字拼写匹配。
    let mut sum_layout_by_semantic_type = BTreeMap::new();
    for sum in &canonical.linked.sum_types {
        if !sum.is_unite {
            continue;
        }
        if let Some(record) = canonical.linked.nominal_instances.get(&sum.nominal) {
            if let Ok(ty) = lower_type(canonical, record.ty) {
                sum_layout_by_semantic_type.insert(ty, sum.clone());
            }
        }
    }
    let sum_reps = platform_sum_representations(canonical)
        .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    // 布局计划与公共路径同源：`from_prepared_input` 会把语义类型折叠表注入
    // `layout_by_semantic_type`，只拿到布局计划的 lane 也必须能结算闭合 `Apply`，
    // 不允许存在只在部分调用点可见的旁路索引。
    let mut aggregate_layouts = canonical.linked.aggregate_layouts.clone();
    aggregate_layouts.layout_by_semantic_type = aggregate_layout_by_semantic_type.clone();
    Ok(LaneContext {
        canonical,
        fragment,
        reachable,
        abi_names,
        imports,
        views,
        aggregate_layouts,
        aggregate_layout_by_nominal: canonical.linked.aggregate_layout_by_nominal.clone(),
        aggregate_layout_by_field: canonical.linked.aggregate_layout_by_field.clone(),
        field_type_layouts,
        aggregate_layout_by_type: canonical.linked.aggregate_layout_by_type.clone(),
        aggregate_layout_by_semantic_type,
        flags_types: canonical.linked.flags_types.clone(),
        singleton_instances: canonical.linked.singleton_instances.clone(),
        // 程序级文本表示模式只来自正式表示计划的显式事实（`RepresentationPlan`）；
        // 计划未声明时保持 `None`，后端按 managed 语义 fail-closed，禁止在这里
        // 按布局注册表是否含 `utf8` 反推模式。
        program_text_representation: program.representation().text_representation.map(|representation| match representation {
            PlannedTextRepresentation::Raw => ProgramTextRepresentation::Raw,
            PlannedTextRepresentation::Managed => ProgramTextRepresentation::Managed,
        }),
        sum_layout_by_semantic_type,
        sum_reps,
    })
}

/// 函数级合同校验 + 类型预解析：值表示合同、参数名冻结、sum variant 合同、
/// invoke 降低合同、终结符支持面，最后投影返回/形参/SSA 值的语义类型。
fn build_function_view(
    program: &CompiledValkyrieProgram,
    canonical: &ValkyrieMirProgram,
    function: &ValkyrieMirFunction,
) -> Result<FunctionView> {
    // SSA 值的表示合同：Specialized 或 Reified（且必须有引用实例布局）才可进入目标层。
    for (value, type_id) in &function.value_types {
        let identity = ValueIdentity::new(function.instance, *value);
        match program.representation().value_reps.get(&identity) {
            Some(ValueRepresentation::Specialized) => {}
            Some(ValueRepresentation::Reified) if has_reference_instance_layout(canonical, *type_id) => {}
            Some(representation) => {
                return Err(format!(
                    "值 `{identity:?}` 的表示 `{representation:?}` 尚无目标私有载体合同，拒绝按语义类型重新选择表示"
                ));
            }
            None => return Err(format!("值 `{identity:?}` 缺少 RepresentationPlan 载体合同")),
        }
    }
    // 入口参数：SSA 值合同与 canonical 冻结声明名。
    for (index, (value, _)) in function.parameters.iter().enumerate() {
        if !function.value_types.contains_key(value) {
            return Err("入口参数缺少 SSA 值合同".to_string());
        }
        if function.parameter_names.get(index).is_none() {
            return Err(format!("函数实例 `{:?}` 入口参数 {index} 缺少 canonical 冻结的声明参数名", function.instance));
        }
    }
    let invoke_lowerings = &program.representation().invoke_lowerings;
    for block in function.blocks.values() {
        for instruction in &block.instructions {
            // sum 操作必须携带链接期 variant 合同，payload 布局不能由 backend 补造。
            for (nominal, variant) in sum_operations(&instruction.operation) {
                let Some(sum) = canonical.linked.sum_types.iter().find(|sum| sum.nominal == nominal) else {
                    return Err(format!("sum `{nominal:?}` 缺少链接期 variant 合同"));
                };
                if !sum.variants.iter().any(|entry| entry.id == variant) {
                    return Err(format!("sum variant `{nominal:?}/{variant:?}` 缺少链接期 variant 合同"));
                }
            }
            // invoke 降低合同：表示计划必须为每条调用冻结消去方式。
            if let ValkyrieMirOperation::Invoke { callee, .. } = &instruction.operation {
                match (callee, invoke_lowerings.get(&instruction.id)) {
                    (ValkyrieMirCallee::Item(_), Some(InvokeLowering::Direct)) => {}
                    (ValkyrieMirCallee::Witness(_), Some(InvokeLowering::TypedWitness)) => {}
                    (ValkyrieMirCallee::Value(callee), Some(InvokeLowering::TypedReference)) => {
                        let Some(callee_type) = function.value_types.get(callee) else {
                            return Err(format!(
                                "调用指令 `{}` 的 callee `Value({callee:?})` 缺少 Semantic MIR 类型事实",
                                instruction.id.index()
                            ));
                        };
                        if !matches!(canonical.linked.types.get(callee_type).map(|row| &row.kind), Some(CanonicalTypeKind::Function { .. })) {
                            return Err(format!(
                                "调用指令 `{}` 的 callee `Value({callee:?})` 类型 `{callee_type:?}` 不是函数类型，\
                                 尚无目标私有调用合同，拒绝发射间接调用",
                                instruction.id.index()
                            ));
                        }
                    }
                    (_, None) => {
                        return Err(format!("调用指令 `{}` 缺少 RepresentationPlan 降低合同", instruction.id.index()));
                    }
                    (_, Some(lowering)) => {
                        return Err(format!(
                            "调用指令 `{}` 的表示 `{lowering:?}` 与 callee `{callee:?}` 尚无目标私有调用合同，拒绝改用普通调用",
                            instruction.id.index()
                        ));
                    }
                }
            }
        }
        // 终结符支持面：Raise 映射为 Trap，其余 effect/dispatch 在 lane 失败闭合。
        if !matches!(
            &block.terminator,
            ValkyrieMirTerminator::Return { .. }
                | ValkyrieMirTerminator::Jump { .. }
                | ValkyrieMirTerminator::Branch { .. }
                | ValkyrieMirTerminator::Unreachable
                | ValkyrieMirTerminator::PerformEffect { effect: ValkyrieMirEffectKind::Raise, .. }
        ) {
            return Err(format!("canonical effect terminator 尚无目标合同: {:?}", block.terminator));
        }
    }
    let symbol = canonical
        .linked
        .callable_names
        .get(&function.instance)
        .ok_or_else(|| format!("callable 实例 `{:?}` 缺少 ABI 名称", function.instance))?
        .to_string();
    let return_type = lower_type(canonical, function.return_type)?;
    let mut param_types = Vec::with_capacity(function.parameters.len());
    for (_, type_id) in &function.parameters {
        param_types.push(lower_type(canonical, *type_id)?);
    }
    let mut value_types = BTreeMap::new();
    for (value, type_id) in &function.value_types {
        value_types.insert(*value, lower_type(canonical, *type_id)?);
    }
    Ok(FunctionView {
        symbol,
        param_types,
        return_type,
        value_types,
        parameter_names: function.parameter_names.clone(),
    })
}

/// 提取一条 MIR 指令携带的 sum 实例/variant 对（链接期 variant 合同校验用）。
fn sum_operations(operation: &ValkyrieMirOperation) -> Vec<(NominalInstanceId, VariantId)> {
    match operation {
        ValkyrieMirOperation::SumNew { nominal, variant, .. } | ValkyrieMirOperation::SumVariantIs { nominal, variant, .. } => {
            vec![(*nominal, *variant)]
        }
        ValkyrieMirOperation::SumPayloadGet { nominal, variant, .. } => vec![(*nominal, *variant)],
        _ => Vec::new(),
    }
}

/// 入口符号：旧 prepare 的 `abi_name_for_instance(entry).parts().last()` 等价。
pub(crate) fn entry_symbol(ctx: &LaneContext, entry: &ItemInstanceId) -> Option<String> {
    ctx.abi_name(entry).and_then(|name| name.parts().last().map(|part| part.as_str().to_string()))
}

/// 该类型是否有 canonical 冻结的实例级 Reference 布局（GC struct 载体）。
pub(crate) fn has_reference_instance_layout(program: &ValkyrieMirProgram, type_id: compiler_types::TypeId) -> bool {
    let Some(layout_id) = program.linked.aggregate_layout_by_type.get(&type_id).copied()
    else {
        return false;
    };
    program
        .linked
        .aggregate_layouts
        .layouts
        .iter()
        .find(|layout| layout.id == layout_id)
        .is_some_and(|layout| layout.storage == compiler_types::StorageKind::Reference)
}

/// 把链接期 sum/variant 事实映射成 sum 表示合同。
///
/// variant 身份、discriminant 与 payload 类型来自链接程序。tag 的物理宽度不在
/// 这里声明——它是各 lane 自己的物理事实，由后端在布局处冻结；语言适配器不再
/// 按 backend family 预先取宽度。
pub(crate) fn platform_sum_representations(canonical: &ValkyrieMirProgram) -> Result<BTreeMap<NominalInstanceId, SumRepresentation>> {
    let mut reps = BTreeMap::new();
    for sum in &canonical.linked.sum_types {
        if !canonical.linked.nominal_instances.contains_key(&sum.nominal) {
            return Err(format!("sum 布局引用未知 nominal identity: {:?}", sum.nominal));
        }
        let mut variants = BTreeMap::new();
        for variant in &sum.variants {
            let Some(record) = canonical.linked.variants.get(&(sum.nominal, variant.id))
            else {
                return Err(format!("sum 布局引用未知 variant identity: {:?}/{:?}", sum.nominal, variant.id));
            };
            if record.payload_type.is_some() != variant.payload_type.is_some() {
                return Err(format!("sum variant payload 合同不一致: {:?}/{:?}", sum.nominal, variant.id));
            }
            let entry = SumVariantRepresentation { id: variant.id, tag: variant.tag, payload_type: record.payload_type };
            if variants.insert(variant.id, entry).is_some() {
                return Err(format!("sum variant identity 重复: {:?}/{:?}", sum.nominal, variant.id));
            }
        }
        if reps.insert(sum.nominal, SumRepresentation { nominal: sum.nominal, variants }).is_some() {
            return Err(format!("sum nominal identity 重复: {:?}", sum.nominal));
        }
    }
    Ok(reps)
}

/// 函数声明的签名（参数与返回类型）是否仍含未闭合的 Generic。
///
/// 签名含 Generic 说明该实例是声明模板而非单态化后的闭合实例；目标层无法编码这种
/// 签名，因此它只能作为失败原因，不能作为「可以跳过」的判据。
pub(crate) fn signature_contains_generic(program: &ValkyrieMirProgram, function: &ValkyrieMirFunction) -> bool {
    function.parameters.iter().any(|(_, ty)| type_contains_generic_deep(program, *ty))
        || type_contains_generic_deep(program, function.return_type)
}

/// 递归检查 canonical 类型（含嵌套）是否包含 Generic。
fn type_contains_generic_deep(program: &ValkyrieMirProgram, id: compiler_types::TypeId) -> bool {
    let Some(row) = program.linked.types.get(&id)
    else {
        return false;
    };
    match &row.kind {
        compiler_types::CanonicalTypeKind::Generic { .. } => true,
        compiler_types::CanonicalTypeKind::Nominal { arguments, .. } => arguments.iter().any(|a| type_contains_generic_deep(program, *a)),
        compiler_types::CanonicalTypeKind::Tuple(items)
        | compiler_types::CanonicalTypeKind::Union(items)
        | compiler_types::CanonicalTypeKind::Intersection(items) => items.iter().any(|a| type_contains_generic_deep(program, *a)),
        compiler_types::CanonicalTypeKind::LabeledTuple { fields } => fields.iter().any(|(_, ty)| type_contains_generic_deep(program, *ty)),
        compiler_types::CanonicalTypeKind::Array { element, .. } => type_contains_generic_deep(program, *element),
        compiler_types::CanonicalTypeKind::Nullable(element) => type_contains_generic_deep(program, *element),
        compiler_types::CanonicalTypeKind::Function { parameters, return_type } => {
            parameters.iter().any(|a| type_contains_generic_deep(program, *a)) || type_contains_generic_deep(program, *return_type)
        }
        _ => false,
    }
}

/// canonical 标量基元 → 目标语义载体。
///
/// 载体来自 frontend 冻结的身份（`[primitive("…")]` 或 canonical `Primitive`），
/// 不是拼写：没有稳定目标宽度的形态（`f128` 等）失败闭合。
fn semantic_of_canonical_primitive(kind: compiler_types::CanonicalPrimitiveType) -> Result<SemanticType> {
    use compiler_types::CanonicalPrimitiveType as P;
    Ok(match kind {
        P::Void => SemanticType::Bottom,
        P::Unit => SemanticType::Unit,
        P::Bool => SemanticType::Boolean,
        P::Integer { bits: 8, signed } => SemanticType::Integer8 { signed },
        P::Integer { bits: 16, signed } => SemanticType::Integer16 { signed },
        P::Integer { bits: 32, signed } => SemanticType::Integer32 { signed },
        P::Integer { bits: 64, signed } => SemanticType::Integer64 { signed },
        P::Integer { bits: 128, signed } => SemanticType::Integer128 { signed },
        P::Float { bits: 32 } => SemanticType::Float32,
        P::Float { bits: 64 } => SemanticType::Float64,
        P::Character => SemanticType::Character,
        P::Utf8 => SemanticType::Utf8,
        P::Utf16 => SemanticType::Utf16,
        unsupported => return Err(format!("primitive 尚无目标合同: {unsupported:?}")),
    })
}

/// 按 canonical 类型图重建布局计划使用的**未结算**类型形态（`Nominal` 基 + `Apply` 实参）。
pub(crate) fn lower_unsettled_contract_key(program: &ValkyrieMirProgram, id: compiler_types::TypeId) -> Result<SemanticType> {
    let kind = &program.linked.types.get(&id).ok_or_else(|| "canonical 类型事实缺失".to_string())?.kind;
    match kind {
        CanonicalTypeKind::Nominal { declaration, arguments } => {
            // 文本载体声明（`std.text::Utf8Text`/`Utf16Text`）在 unsettled 键处必须折叠成
            // 编码基元，与签名折叠、顶层折叠键登记同源（都消费 canonical 已冻结的
            // `text_encoding_types`，01 §5.4：文本身份只来自唯一已冻结编码身份）。
            // 这里若保留名义 `Nominal{layout}` 形态，语言布局计划的字段事实（已经文本
            // 编码桥折叠成 `Utf8`/`Utf16`）与本键就是同一类型的两种键，宿主序列投影的
            // 嵌套层查找必然 miss，投影失败闭合退化成 `InstanceHandle`。
            if let Some(encoding) = program.linked.text_encoding_types.get(&id) {
                return match encoding {
                    CanonicalPrimitiveType::Utf8 => Ok(SemanticType::Utf8),
                    CanonicalPrimitiveType::Utf16 => Ok(SemanticType::Utf16),
                    other => Err(format!("文本载体 `TypeId` 登记了非文本编码身份: {other:?}")),
                };
            }
            let base = match program.linked.aggregate_layout_by_type.get(declaration) {
                Some(layout) => SemanticType::Nominal { layout: *layout, spelling: compiler_types::Identifier::new("") },
                None => lower_type(program, *declaration)?.contract_key(),
            };
            if arguments.is_empty() {
                Ok(base)
            }
            else {
                Ok(SemanticType::Apply(
                    Box::new(base),
                    arguments.iter().map(|argument| lower_unsettled_contract_key(program, *argument)).collect::<Result<Vec<_>>>()?,
                ))
            }
        }
        CanonicalTypeKind::Array { element, length } => {
            let element = lower_unsettled_contract_key(program, *element)?;
            Ok(match length {
                None => SemanticType::Array(Box::new(element)),
                Some(length) => SemanticType::FixedArray {
                    element: Box::new(element),
                    length: usize::try_from(*length).map_err(|_| "固定数组长度溢出".to_string())?,
                },
            })
        }
        CanonicalTypeKind::Tuple(items) => {
            Ok(SemanticType::Tuple(items.iter().map(|item| lower_unsettled_contract_key(program, *item)).collect::<Result<Vec<_>>>()?))
        }
        CanonicalTypeKind::Nullable(element) => Ok(SemanticType::Nullable(Box::new(lower_unsettled_contract_key(program, *element)?))),
        CanonicalTypeKind::Union(items) => {
            Ok(SemanticType::Union(items.iter().map(|item| lower_unsettled_contract_key(program, *item)).collect::<Result<Vec<_>>>()?))
        }
        _ => Ok(lower_type(program, id)?.contract_key()),
    }
}

pub(crate) fn lower_type(program: &ValkyrieMirProgram, id: compiler_types::TypeId) -> Result<SemanticType> {
    let kind = &program.linked.types.get(&id).ok_or_else(|| format!("canonical 类型事实缺失"))?.kind;
    match kind {
        CanonicalTypeKind::Primitive(kind) => match kind {
            compiler_types::CanonicalPrimitiveType::Void => Ok(SemanticType::Bottom),
            compiler_types::CanonicalPrimitiveType::Unit => Ok(SemanticType::Unit),
            compiler_types::CanonicalPrimitiveType::Bool => Ok(SemanticType::Boolean),
            // 窄整数保留宽度与符号，不能在计划层提前抹成 i32：表示转换的扩展/截断
            // 配方由两端语义类型决定，抹平宽度后无法区分 `i32.and 0xFF` 与 `i32.extend8_s`。
            compiler_types::CanonicalPrimitiveType::Integer { bits: 8, signed } => Ok(SemanticType::Integer8 { signed: *signed }),
            compiler_types::CanonicalPrimitiveType::Integer { bits: 16, signed } => Ok(SemanticType::Integer16 { signed: *signed }),
            compiler_types::CanonicalPrimitiveType::Integer { bits: 32, signed } => Ok(SemanticType::Integer32 { signed: *signed }),
            compiler_types::CanonicalPrimitiveType::Integer { bits: 64, signed } => Ok(SemanticType::Integer64 { signed: *signed }),
            // 浮点各自保留独立载体：`Value` 有 `F64` 与 `F32` 两个变体，
            // `bits: 32` 不得落到 64 位槽（那会让「32 位类型」实际承载 64 位载荷）。
            compiler_types::CanonicalPrimitiveType::Float { bits: 32 } => Ok(SemanticType::Float32),
            compiler_types::CanonicalPrimitiveType::Float { bits: 64 } => Ok(SemanticType::Float64),
            compiler_types::CanonicalPrimitiveType::Character => Ok(SemanticType::Character),
            compiler_types::CanonicalPrimitiveType::Utf8 => Ok(SemanticType::Utf8),
            compiler_types::CanonicalPrimitiveType::Utf16 => Ok(SemanticType::Utf16),
            unsupported => Err(format!("primitive 尚无目标合同: {unsupported:?}")),
        },
        CanonicalTypeKind::Tuple(items) => Ok(SemanticType::Tuple(items.iter().map(|id| lower_type(program, *id)).collect::<Result<_>>()?)),
        // 唯一标签擦除点：标签只属于 canonical 身份，目标层按位置布局，`SemanticType`
        // 与 `LayoutKey` 保持无标签。类型身份不等但布局相同的元组在 ABI 上允许。
        CanonicalTypeKind::LabeledTuple { fields } => {
            Ok(SemanticType::Tuple(fields.iter().map(|(_, ty)| lower_type(program, *ty)).collect::<Result<_>>()?))
        }
        CanonicalTypeKind::Array { element, length: None } => Ok(SemanticType::Array(Box::new(lower_type(program, *element)?))),
        CanonicalTypeKind::Array { element, length: Some(length) } => Ok(SemanticType::FixedArray {
            element: Box::new(lower_type(program, *element)?),
            length: usize::try_from(*length).map_err(|_| format!("固定数组长度溢出"))?,
        }),
        CanonicalTypeKind::Nullable(element) => Ok(SemanticType::Nullable(Box::new(lower_type(program, *element)?))),
        CanonicalTypeKind::Nominal { declaration, arguments } => {
            if !arguments.is_empty() {
                // 实例级布局已由 canonical 冻结时，泛型实例必须在计划边界结算为携带
                // 布局身份的 `Nominal`：存储类、字段与载体由实例布局解析，后端不得把
                // “看起来像引用”的泛型实例一律擦成引用。未冻结实例布局的泛型形状保持
                // `Apply`（宿主句柄形态），由后端在值槽上按缺合同失败闭合。
                if let Some(layout) = program.linked.aggregate_layout_by_type.get(&id).copied() {
                    let spelling = program
                        .linked
                        .type_surface_spellings
                        .get(&id)
                        .or_else(|| program.linked.type_surface_spellings.get(declaration))
                        .cloned()
                        .unwrap_or_else(|| compiler_types::Identifier::new("Apply"));
                    return Ok(SemanticType::Nominal { layout, spelling });
                }
                return Ok(SemanticType::Apply(
                    Box::new(lower_type(program, *declaration)?),
                    arguments.iter().map(|argument| lower_type(program, *argument)).collect::<Result<Vec<_>>>()?,
                ));
            }
            // 文本载体身份只来自 canonical 冻结的编码身份，不按拼写（`Utf8Text` 等）
            // 回查；折叠后的 `Utf8`/`Utf16` 是后续托管布局与宿主边界的共同键。
            if let Some(encoding) = program.linked.text_encoding_types.get(&id).copied() {
                return match encoding {
                    CanonicalPrimitiveType::Utf8 => Ok(SemanticType::Utf8),
                    CanonicalPrimitiveType::Utf16 => Ok(SemanticType::Utf16),
                    other => Err(format!("文本载体 `TypeId` 登记了非文本编码身份: {other:?}")),
                };
            }
            // `[primitive("…")]` 自举身份直接绑定标量载体：载体是身份事实，不按
            // 声明拼写或短名回查。同名但没有该身份的普通 nominal 必须继续走布局，
            // 不得因为名字叫 `i64` 就被静默折叠成标量。
            if let Some(primitive) = program
                .primitive_identities
                .get(&id)
                .or_else(|| program.primitive_identities.get(declaration))
                .copied()
            {
                return semantic_of_canonical_primitive(primitive);
            }
            let spelling = program
                .linked
                .type_surface_spellings
                .get(if *declaration == id { &id } else { declaration })
                .or_else(|| program.linked.type_surface_spellings.get(&id));
            // 零实参 nominal 的布局可能登记在声明 `TypeId` 上（`Self` 别名与声明
            // 使用不同 `TypeId`），只能按声明身份回退，禁止按短名回查。
            let layout =
                program.linked.aggregate_layout_by_type.get(&id).or_else(|| program.linked.aggregate_layout_by_type.get(declaration)).copied();
            if let Some(name) = spelling {
                // 已冻结实例级布局的零实参 nominal 铸造为携带身份的 `Nominal`：
                // 存储类/GC 字段/ABI 由 `layout` 身份解析，拼写仅诊断显示。
                // 泛型声明本身没有物理布局，只作为 `Apply` 基的构造器拼写保留；
                // 闭合 nominal 有实例记录却无任何布局绑定时必须失败关闭，
                // 不得压回裸 `Named` 让下游按名字猜实现。
                if let Some(layout) = layout {
                    return Ok(SemanticType::Nominal { layout, spelling: name.clone() });
                }
                if is_generic_declaration(program, *declaration) {
                    return Ok(SemanticType::Named(name.clone()));
                }
                return Err(format!("nominal type `{name}` 缺少目标 layout 或 carrier 合同"));
            }
            if *declaration != id {
                return lower_type(program, *declaration);
            }
            // canonical 合成名义记录（多字段变体 payload）没有源面拼写：源码里不存在
            // 这个名字，只有变体声明身份。布局已冻结时目标身份由布局决定，拼写从布局
            // 限定名派生，仅作诊断显示；不得为它要求一条不存在的 surface spelling。
            if let Some(layout_id) = layout {
                let spelling = match program.linked.aggregate_layouts.layouts.iter().find(|layout| layout.id == layout_id) {
                    Some(layout) if !layout.namespace.is_empty() => {
                        format!("{}::{}", layout.namespace.replace('.', "::"), layout.name)
                    }
                    Some(layout) => layout.name.clone(),
                    None => format!("Nominal({id:?})"),
                };
                return Ok(SemanticType::Nominal { layout: layout_id, spelling: compiler_types::Identifier::new(&spelling) });
            }
            Err(format!(
                "canonical 类型尚无目标合同: Nominal {{ declaration: {declaration:?}, arguments: [], {} }}",
                nominal_contract_context(program, id, *declaration)
            ))
        }
        CanonicalTypeKind::Function { parameters, return_type } => Ok(SemanticType::Function(Box::new(FunctionType {
            params: parameters.iter().map(|parameter| lower_type(program, *parameter)).collect::<Result<Vec<_>>>()?,
            return_type: lower_type(program, *return_type)?,
        }))),
        CanonicalTypeKind::Union(items) => {
            Ok(SemanticType::Union(items.iter().map(|item| lower_type(program, *item)).collect::<Result<Vec<_>>>()?))
        }
        // `Intersection` 必须在进入平台表示计划前由 frontend 擦除（见
        // `frontend_contract::type_concretize` 的 `ConcretizeError`）。把它静默降成 `Union`
        // 会改变语义（`A & B` 不等于 `A | B`），属于错误成功路径，因此失败闭合。
        CanonicalTypeKind::Intersection(items) => {
            Err(format!("canonical 类型 Intersection 尚未在 frontend 擦除，不能进入平台表示计划: {items:?}"))
        }
        unsupported => {
            // Generic 类型只能出现在泛型函数的签名里——如果某个泛型函数被错误地留在了
            // 可达集并尝试编码，这里就会触发。打印引用 Generic 的闭合函数帮助定位。
            let canonical = program;
            let mut refs_debug = String::new();
            for (inst, func) in &program.mir.functions {
                // 跳过泛型声明函数
                let has_generic_sig = func.parameters.iter().any(|(_, tid)| {
                    canonical.linked.types.get(tid).is_some_and(|row| matches!(row.kind, compiler_types::CanonicalTypeKind::Generic { .. }))
                }) || canonical
                    .linked
                    .types
                    .get(&func.return_type)
                    .is_some_and(|row| matches!(row.kind, compiler_types::CanonicalTypeKind::Generic { .. }));
                if has_generic_sig {
                    continue;
                }
                // 递归检查 value_types 里有没有嵌套 Generic
                let mut found = false;
                for (_, tid) in &func.value_types {
                    if type_contains_generic_deep(canonical, *tid) {
                        found = true;
                        break;
                    }
                }
                if found {
                    let name = program.linked.callable_names.get(inst).cloned().unwrap_or_default();
                    refs_debug.push_str(&format!("function `{name}` instance {inst:?}; "));
                }
            }
            let mut type_debug = String::new();
            for (tid, row) in &program.linked.types {
                if matches!(row.kind, compiler_types::CanonicalTypeKind::Generic { .. }) {
                    type_debug.push_str(&format!("TypeId({:?})={:?}; ", tid, row.kind));
                }
            }
            return Err(format!(
                "canonical 类型尚无目标合同: {unsupported:?} (module={}, refs=[{}], generic_types=[{}])",
                program.mir.module_name, refs_debug, type_debug
            ));
        }
    }
}

/// 判定 `declaration` 是否为泛型声明：存在以该声明为 owner 的具实参 nominal 实例。
///
/// 泛型声明本身没有物理布局，只能作为 `Apply` 基的构造器拼写保留；闭合 nominal
/// 缺少布局是合同缺口，必须失败闭合，二者不能共用裸 `Named` 回落。
fn is_generic_declaration(program: &ValkyrieMirProgram, declaration: compiler_types::TypeId) -> bool {
    program.linked.types.values().any(|record| {
        matches!(
            &record.kind,
            CanonicalTypeKind::Nominal { declaration: owner, arguments } if *owner == declaration && !arguments.is_empty()
        )
    })
}

/// 失败闭合诊断的定位信息：声明拼写、名义实例与字段槽位。
///
/// 只打印 `TypeId` 无法区分「哪个声明/哪个字段」缺合同；这里优先用已冻结的
/// surface spelling，其次用名义实例的字段槽位名（布局已登记时），最后才退回
/// `FieldId`。
fn nominal_contract_context(program: &ValkyrieMirProgram, id: compiler_types::TypeId, declaration: compiler_types::TypeId) -> String {
    let mut parts = Vec::new();
    if let Some(name) = program.linked.type_surface_spellings.get(&declaration) {
        parts.push(format!("spelling={name}"));
    }
    if let Some((nominal, record)) = program.linked.nominal_instances.iter().find(|(_, record)| record.ty == id) {
        parts.push(format!("nominal={nominal:?}"));
        if !record.fields.is_empty() {
            let fields = record
                .fields
                .iter()
                .map(|field| {
                    program
                        .linked
                        .aggregate_layout_by_field
                        .get(field)
                        .and_then(|(layout_id, index)| {
                            program
                                .linked
                                .aggregate_layouts
                                .layouts
                                .iter()
                                .find(|layout| layout.id == *layout_id)
                                .and_then(|layout| layout.fields.get(*index as usize))
                                .map(|slot| slot.name.clone())
                        })
                        .unwrap_or_else(|| format!("FieldId({})", field.index()))
                })
                .collect::<Vec<_>>();
            parts.push(format!("fields=[{}]", fields.join(", ")));
        }
    }
    if parts.is_empty() { "无已登记拼写或布局".to_string() } else { parts.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::valkyrie_mir::ValkyrieMirBlockId;
    use compiler_emitter::target_operator::TargetOperandType;
    use crate::compiler::emitter::pipeline::operator::operand_carrier;
    use compiler_types::{ItemId, TypeRecord};

    fn type_id(index: u32) -> compiler_types::TypeId {
        compiler_types::TypeId::from_index(index).expect("测试类型身份")
    }

    fn program_with_types(rows: Vec<(compiler_types::TypeId, CanonicalTypeKind)>) -> ValkyrieMirProgram {
        let mut program = ValkyrieMirProgram::default();
        for (id, kind) in rows {
            program.linked.types.insert(id, TypeRecord { declaration: id, kind });
        }
        program
    }

    fn function_with_signature(
        parameters: Vec<compiler_types::TypeId>,
        return_type: compiler_types::TypeId,
    ) -> ValkyrieMirFunction {
        ValkyrieMirFunction {
            instance: ItemInstanceId::from_index(0).expect("测试实例身份"),
            parameters: parameters
                .into_iter()
                .enumerate()
                .map(|(index, ty)| (compiler_types::MirValueId::from_index(u32::try_from(index).expect("测试值下标")).expect("测试值身份"), ty))
                .collect(),
            parameter_names: Vec::new(),
            return_type,
            value_types: BTreeMap::new(),
            entry: ValkyrieMirBlockId(0),
            blocks: BTreeMap::new(),
        }
    }

    /// 签名含 Generic 的实例只能是失败原因，不能是「可以跳过」的判据。
    #[test]
    fn generic_signature_is_detected_in_parameter_and_nested_and_return() {
        let generic = CanonicalTypeKind::Generic { owner: ItemId::from_index(0).expect("测试声明身份"), ordinal: 0 };
        let program = program_with_types(vec![
            (type_id(0), generic.clone()),
            (type_id(1), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Integer { bits: 32, signed: true })),
            (type_id(2), CanonicalTypeKind::Tuple(vec![type_id(1), type_id(0)])),
        ]);
        assert!(signature_contains_generic(&program, &function_with_signature(vec![type_id(0)], type_id(1))), "直接泛型形参必须失败");
        assert!(signature_contains_generic(&program, &function_with_signature(vec![type_id(2)], type_id(1))), "嵌套泛型形参必须失败");
        assert!(signature_contains_generic(&program, &function_with_signature(vec![type_id(1)], type_id(0))), "泛型返回类型必须失败");
        assert!(!signature_contains_generic(&program, &function_with_signature(vec![type_id(1)], type_id(1))), "闭合标量签名不是泛型模板");
    }

    /// 窄整数与 `Character` 必须保留宽度与符号性：抹成 i32 后无法再区分掩码与符号扩展。
    #[test]
    fn narrow_integer_and_character_primitives_keep_their_identity() {
        let program = program_with_types(vec![
            (type_id(0), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Integer { bits: 8, signed: false })),
            (type_id(1), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Integer { bits: 16, signed: true })),
            (type_id(2), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Character)),
        ]);
        assert_eq!(lower_type(&program, type_id(0)).expect("u8"), SemanticType::Integer8 { signed: false });
        assert_eq!(lower_type(&program, type_id(1)).expect("i16"), SemanticType::Integer16 { signed: true });
        assert_eq!(lower_type(&program, type_id(2)).expect("Character"), SemanticType::Character);
    }

    /// `[primitive]` 自举身份直接绑定标量载体：即使该声明登记了聚合布局
    /// （std 原语声明通常是空壳结构），也必须按身份取标量，而不是按拼写再看布局。
    #[test]
    fn primitive_identity_binds_carrier_before_layout() {
        let declaration = type_id(0);
        let mut program = program_with_types(vec![(declaration, CanonicalTypeKind::Nominal { declaration, arguments: Vec::new() })]);
        program.linked.type_surface_spellings.insert(declaration, compiler_types::Identifier::new("std::primitive::i64"));
        program.primitive_identities.insert(declaration, CanonicalPrimitiveType::Integer { bits: 64, signed: true });
        program.linked.aggregate_layout_by_type.insert(declaration, 0);
        program.linked.aggregate_layouts.layouts.push(compiler_types::AggregateLayout {
            id: 0,
            name: "i64".into(),
            namespace: "std.primitive".into(),
            key: None,
            storage: compiler_types::StorageKind::Value,
            fields: Vec::new(),
        });
        assert_eq!(
            lower_type(&program, declaration).expect("原语身份绑定标量载体"),
            SemanticType::Integer64 { signed: true },
            "带布局的原语声明也必须按身份折叠成标量"
        );
    }

    /// 同名但没有原语身份的普通 nominal（例如另一个命名空间里真的叫 `i64` 的结构体）
    /// 不得按短名折叠成标量，必须保留自身布局身份。
    #[test]
    fn same_name_nominal_without_identity_keeps_layout() {
        let declaration = type_id(0);
        let mut program = program_with_types(vec![(declaration, CanonicalTypeKind::Nominal { declaration, arguments: Vec::new() })]);
        program.linked.type_surface_spellings.insert(declaration, compiler_types::Identifier::new("app::i64"));
        program.linked.aggregate_layout_by_type.insert(declaration, 0);
        program.linked.aggregate_layouts.layouts.push(compiler_types::AggregateLayout {
            id: 0,
            name: "i64".into(),
            namespace: "app".into(),
            key: None,
            storage: compiler_types::StorageKind::Value,
            fields: vec![compiler_types::FieldLayout { name: "value".into(), ty: SemanticType::Integer64 { signed: true } }],
        });
        assert_eq!(
            lower_type(&program, declaration).expect("同名 nominal 保留布局"),
            SemanticType::Nominal { layout: 0, spelling: compiler_types::Identifier::new("app::i64") },
            "同名但无原语身份的 nominal 不得折叠成标量"
        );
    }

    /// 零实参 nominal 的 `Self` 别名 `TypeId` 未持有布局时，必须按声明身份回退；
    /// 有实例记录但无任何布局绑定时必须失败关闭，不得压回裸 `Named`。
    #[test]
    fn zero_argument_nominal_resolves_layout_through_declaration_identity() {
        let alias = type_id(0);
        let declaration = type_id(1);
        let mut program = program_with_types(vec![
            (alias, CanonicalTypeKind::Nominal { declaration, arguments: Vec::new() }),
            (declaration, CanonicalTypeKind::Nominal { declaration, arguments: Vec::new() }),
        ]);
        program.linked.type_surface_spellings.insert(declaration, compiler_types::Identifier::new("Endpoint"));
        program.linked.aggregate_layout_by_type.insert(declaration, 0);
        program.linked.aggregate_layouts.layouts.push(compiler_types::AggregateLayout {
            id: 0,
            name: "Endpoint".into(),
            namespace: String::new(),
            key: None,
            storage: compiler_types::StorageKind::Value,
            fields: vec![compiler_types::FieldLayout { name: "code".into(), ty: SemanticType::Integer64 { signed: true } }],
        });
        program.linked.nominal_instances.insert(
            compiler_types::NominalInstanceId::from_index(0).expect("测试名义实例身份"),
            compiler_types::NominalInstanceRecord {
                declaration,
                ty: alias,
                semantics: compiler_types::NominalValueSemantics::Value,
                fields: Vec::new(),
            },
        );
        assert_eq!(
            lower_type(&program, alias).expect("声明身份布局"),
            SemanticType::Nominal { layout: 0, spelling: compiler_types::Identifier::new("Endpoint") }
        );
    }

    #[test]
    fn nominal_instance_without_any_layout_fails_closed() {
        let alias = type_id(0);
        let declaration = type_id(1);
        let mut program = program_with_types(vec![
            (alias, CanonicalTypeKind::Nominal { declaration, arguments: Vec::new() }),
            (declaration, CanonicalTypeKind::Nominal { declaration, arguments: Vec::new() }),
        ]);
        program.linked.type_surface_spellings.insert(declaration, compiler_types::Identifier::new("Endpoint"));
        program.linked.nominal_instances.insert(
            compiler_types::NominalInstanceId::from_index(0).expect("测试名义实例身份"),
            compiler_types::NominalInstanceRecord {
                declaration,
                ty: alias,
                semantics: compiler_types::NominalValueSemantics::Value,
                fields: Vec::new(),
            },
        );
        let error = lower_type(&program, alias).expect_err("无布局必须失败关闭");
        assert!(error.contains("缺少目标 layout"), "{error}");
    }

    #[test]
    fn generic_declaration_without_layout_keeps_constructor_spelling() {
        let declaration = type_id(0);
        let integer = type_id(1);
        let instance = type_id(2);
        let mut program = program_with_types(vec![
            (declaration, CanonicalTypeKind::Nominal { declaration, arguments: Vec::new() }),
            (integer, CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Integer { bits: 64, signed: true })),
            (instance, CanonicalTypeKind::Nominal { declaration, arguments: vec![integer] }),
        ]);
        program.linked.type_surface_spellings.insert(declaration, compiler_types::Identifier::new("ArrayList"));
        program.linked.nominal_instances.insert(
            compiler_types::NominalInstanceId::from_index(0).expect("测试名义实例身份"),
            compiler_types::NominalInstanceRecord {
                declaration,
                ty: declaration,
                semantics: compiler_types::NominalValueSemantics::Reference,
                fields: Vec::new(),
            },
        );
        assert_eq!(lower_type(&program, declaration).expect("泛型声明拼写"), SemanticType::Named(compiler_types::Identifier::new("ArrayList")));
    }

    /// 浮点宽度与目标载体一一对应：`Float{bits:32}` 必须登记为独立的 `SemanticType::Float32`
    /// 与 `TargetOperandType::F32`，不得静默并入 64 位载体。
    #[test]
    fn float_primitive_registers_width_matched_target_contract() {
        let program = program_with_types(vec![
            (type_id(0), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Float { bits: 64 })),
            (type_id(1), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Float { bits: 32 })),
        ]);
        assert_eq!(lower_type(&program, type_id(0)).expect("f64 必须有目标合同"), SemanticType::Float64);
        let float32 = lower_type(&program, type_id(1)).expect("f32 必须有目标合同");
        assert_eq!(float32, SemanticType::Float32);
        assert_eq!(operand_carrier(&float32), Some(TargetOperandType::F32));
        assert_ne!(operand_carrier(&float32), operand_carrier(&SemanticType::Float64), "f32 不得复用 f64 载体");
    }

    /// `Intersection` 必须在 frontend 擦除；平台 lowering 静默降成 `Union` 会改变语义，
    /// 属错误成功路径，因此必须结构化失败闭合。
    #[test]
    fn intersection_type_fails_closed_instead_of_widening_to_union() {
        let program = program_with_types(vec![
            (type_id(0), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Integer { bits: 32, signed: true })),
            (type_id(1), CanonicalTypeKind::Primitive(CanonicalPrimitiveType::Bool)),
            (type_id(2), CanonicalTypeKind::Intersection(vec![type_id(0), type_id(1)])),
        ]);
        let error = lower_type(&program, type_id(2)).expect_err("Intersection 不得被降成 Union");
        assert!(error.contains("Intersection"), "诊断必须指明未擦除的 Intersection，实际：{error}");
        assert!(error.contains("frontend"), "诊断必须指向 frontend 擦除责任，实际：{error}");
    }

    /// `Unsound` 必须携带可机械归因的阶段与来源身份，而不是只剩一句文本。
    #[test]
    fn unsound_error_carries_stage_and_source_identity() {
        let instance = ItemInstanceId::from_index(3).expect("测试实例身份");
        let error = TargetPlanError::unsound(AdapterStage::Type, Some(instance), Some("demo::run".to_string()), "投影失败".to_string());
        let TargetPlanError::Unsound { stage, instance: bound_instance, symbol, reason } = &error
        else {
            panic!("必须构造 Unsound");
        };
        assert_eq!(*stage, AdapterStage::Type);
        assert_eq!(*bound_instance, Some(instance));
        assert_eq!(symbol.as_deref(), Some("demo::run"));
        assert_eq!(reason, "投影失败");
        let text = error.to_string();
        assert!(text.contains("target-plan[type]"), "展示必须含阶段: {text}");
        assert!(text.contains("symbol=demo::run"), "展示必须含符号: {text}");
    }

    /// 文本载体实参在 unsettled 键处必须折叠成编码基元（`ArrayList<utf8>` 投影回归反例）：
    /// 语言布局计划的字段事实经文本编码桥折叠为 `Utf8`，若本键保留名义
    /// `Nominal{layout}` 形态，同一类型就有两种键，宿主序列投影的嵌套层查找必然
    /// miss，投影失败闭合退化成 `InstanceHandle`。折叠事实只来自 canonical 已冻结的
    /// `text_encoding_types`，与签名折叠、顶层折叠键同源。
    #[test]
    fn unsettled_contract_key_folds_text_carrier_arguments() {
        let utf8_text = type_id(10);
        let storage_declaration = type_id(11);
        let storage_of_utf8 = type_id(12);
        let mut program = program_with_types(vec![
            (utf8_text, CanonicalTypeKind::Nominal { declaration: utf8_text, arguments: Vec::new() }),
            (storage_declaration, CanonicalTypeKind::Nominal { declaration: storage_declaration, arguments: Vec::new() }),
            (storage_of_utf8, CanonicalTypeKind::Nominal { declaration: storage_declaration, arguments: vec![utf8_text] }),
        ]);
        // `Utf8Text` 已登记托管布局并冻结编码身份：键必须取折叠形态，不得取名义布局形态。
        program.linked.aggregate_layout_by_type.insert(utf8_text, 7);
        program.linked.text_encoding_types.insert(utf8_text, CanonicalPrimitiveType::Utf8);
        program.linked.type_surface_spellings.insert(storage_declaration, compiler_types::Identifier::new("std::collection::ArrayStorage"));
        let key = lower_unsettled_contract_key(&program, storage_of_utf8).expect("闭合实例必须能派生 unsettled 键");
        assert_eq!(
            key,
            SemanticType::Apply(
                Box::new(SemanticType::Named(compiler_types::Identifier::new("std::collection::ArrayStorage"))),
                vec![SemanticType::Utf8]
            ),
            "文本载体实参必须折叠成 `Utf8`，与语言布局字段事实同键: {key:?}"
        );
    }
}
