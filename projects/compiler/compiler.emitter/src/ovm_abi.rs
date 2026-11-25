//! OVM target 的库导出 ABI 合同。
//!
//! 合同全部由语言侧 OVM lane 从 MIR/canonical 一次结算的**目标私有导出事实**
//! （[`OvmAbiExportFacts`]）与链接布局事实（[`OvmAbiPlanFacts`]）派生：结构层与
//! wasm 共用 [`crate::abi_structure`]，本模块只负责 OVM 物理投影（模块布局下标与
//! 无类型槽位），不复制结构推导，也不读取跨目标共享程序。
//!
//! 失败闭合是设计的一部分：
//! - 未注册布局、`i128`、`Utf16`、`TraitObject`、未知/不支持结构一律返回错误；
//! - 任何 `Unsupported` 都不会进入 JSON，也不会被降级成“先跑再说”的兜底；
//! - 参数/返回的物理载体完全由本 lane 的结构投影产出；OVM 不读取也不比较
//!   Wasm 的载体表，零参数 / `Void` 边界是合法的空合同。
//!
//! digest 覆盖规范化 JSON 里的 `schema_version` 与全部导出（按逻辑名排序），
//! 同一布局事实在任意导出位置派生同一 digest。

use std::collections::{BTreeMap, BTreeSet};

use compiler_types::{AggregateLayoutPlan, LayoutId, NominalInstanceId, SemanticType, SumTypeLayout, combined_content_hash};
use data_binary_odin::OdinLayout;
use serde::Serialize;

use crate::{
    ProgramTextRepresentation, Result,
    abi_structure::{AbiStructure, AggregateStructure, abi_structure_for_type_with_layouts},
};

// 合同类型定义在格式 crate，编译器与运行时共用同一份 schema；本模块只保留派生。
pub use data_binary_odin::abi::{
    OVM_ABI_SCHEMA_VERSION, OVM_ABI_SIDECAR_SUFFIX, OvmAbiContract, OvmAbiExport, OvmAbiField, OvmAbiParam, OvmAbiSidecar, OvmAbiType,
    OvmAbiVariant, OvmOptionBoxing, OvmTextRepresentation, ovm_abi_sidecar_file,
};

// 合同类型（含 `OvmAbiType`）由 `data_binary_odin::abi` 定义并在上方重导出。

// `OvmAbiField` / `OvmTextRepresentation` / `OvmAbiVariant` / `OvmOptionBoxing`
// 以及 `OvmAbiSidecar` 同样由 `data_binary_odin::abi` 定义。

/// 单个库导出的目标私有 ABI 事实，由语言侧 OVM lane 从 MIR 一次结算。
///
/// 这里只携带 ABI 派生需要的签名与槽布局事实；参数名来自 canonical 冻结的
/// `parameter_names`（入口块 SSA 形参的声明名），派生时校验数量、非空与唯一性，
/// 禁止按位置合成名义名。入口形参槽布局与 lowering 的入口槽表同源派生，
/// 禁止任何一方另写一份回落，否则合同与运行时实际布局会分叉。
pub struct OvmAbiExportFacts {
    /// 逻辑导出名（run-contract 表面）。
    pub logical_name: String,
    /// 源码声明名（`[export]` 冻结事实）。
    pub source_name: String,
    /// 形参语义类型，按声明顺序。
    pub param_types: Vec<SemanticType>,
    /// canonical 冻结的形参声明名，按位置。
    pub param_names: Vec<String>,
    /// 返回语义类型。
    pub return_type: SemanticType,
    /// 入口形参槽的冻结布局身份。
    ///
    /// 事实来源与 lowering 完全一致：
    /// 1. SSA 值类型（`TypeId`）→ `aggregate_layout_by_type`；
    /// 2. 折叠后的槽类型 → `aggregate_layout_by_semantic_type`。
    ///
    /// 两处都没有 = 目标计划没有为该槽冻结托管布局；`None` 只有在程序级表示计划
    /// 显式声明 raw 时才是合法 raw 宿主串事实，否则由 [`Projection::project_text`]
    /// 与 lowering 失败闭合。
    pub entry_parameter_layouts: Vec<Option<LayoutId>>,
    /// 返回槽的冻结布局身份。
    pub return_layout: Option<LayoutId>,
}

/// ABI 派生消费的链接级布局事实（与公共 IR 无关）。
pub struct OvmAbiPlanFacts<'a> {
    /// 聚合布局计划。
    pub aggregate_layouts: &'a AggregateLayoutPlan,
    /// 结构化类型 → 布局身份。
    pub aggregate_layout_by_semantic_type: &'a BTreeMap<SemanticType, LayoutId>,
    /// 语义类型 → sum 布局（unite 表示）。
    pub sum_layout_by_semantic_type: &'a BTreeMap<SemanticType, SumTypeLayout>,
    /// 程序级文本表示计划。
    pub program_text_representation: Option<ProgramTextRepresentation>,
}

/// 从语言侧结算的目标私有导出事实派生合同。
///
/// `exports` 是导出面的逐导出事实；`module_layouts` / `layout_indices`
/// 是 `.odin` 模块已登记的聚合布局表与 canonical `LayoutId` → 模块下标映射；
/// `sum_layout_indices` 是 lowering 已登记的 `NominalInstanceId` → sum 布局下标。
/// ABI 位置引用但模块尚未登记的布局会按冻结字段数追加到表尾，保证合同里
/// 的每个聚合下标都对应一份真实布局。
pub fn derive_contract(
    facts: &OvmAbiPlanFacts<'_>,
    exports: &[OvmAbiExportFacts],
    module_layouts: &mut Vec<OdinLayout>,
    layout_indices: &mut BTreeMap<LayoutId, i32>,
    sum_layout_indices: &BTreeMap<NominalInstanceId, i32>,
) -> Result<OvmAbiContract> {
    let mut ordered = Vec::with_capacity(exports.len());
    let mut seen = BTreeSet::new();
    for export in exports {
        if !seen.insert(export.logical_name.clone()) {
            return Err(format!("库导出名 `{}` 重复，无法派生 OVM ABI 合同", export.logical_name));
        }
        ordered.push(export);
    }
    // 先按逻辑名排序再投影：布局下标分配与 digest 都与导出登记顺序无关。
    ordered.sort_by(|left, right| left.logical_name.cmp(&right.logical_name));
    let mut option_box_indices = BTreeMap::new();
    let mut entries = Vec::with_capacity(ordered.len());
    for export in ordered {
        entries.push(derive_export(facts, export, module_layouts, layout_indices, sum_layout_indices, &mut option_box_indices)?);
    }
    let digest = contract_digest(&entries)?;
    let contract = OvmAbiContract { schema_version: OVM_ABI_SCHEMA_VERSION, lane: "ovm".to_string(), exports: entries, digest };
    // 合同必须与它所描述的 `.odin` 布局表一致；不一致 fail-closed。
    contract.verify_against_layouts(module_layouts)?;
    Ok(contract)
}

/// 参数身份校验：数量、非空与唯一。
///
/// 与旧公共路径的 `canonical_parameter_identities` 同一合同：参数名缺失或重复
/// 即失败闭合，绝不合成 `arg{index}` 之类名义名。
fn validate_parameter_identities(logical_name: &str, param_types: &[SemanticType], param_names: &[String]) -> Result<()> {
    if param_names.len() != param_types.len() {
        return Err(format!(
            "库导出 `{logical_name}` 的入口参数身份数量与参数类型数量不一致: parameter_names={} param_types={}",
            param_names.len(),
            param_types.len()
        ));
    }
    let mut names = BTreeSet::new();
    for (index, name) in param_names.iter().enumerate() {
        if name.trim().is_empty() {
            return Err(format!("库导出 `{logical_name}` 参数 {index} 缺少 canonical 参数名"));
        }
        if !names.insert(name.as_str()) {
            return Err(format!("库导出 `{logical_name}` 参数 {index} 的参数名 `{name}` 重复"));
        }
    }
    Ok(())
}

/// `utf8` 槽的文本表示事实来源。
#[derive(Clone, Copy, Debug)]
enum TextSlot {
    /// 入口形参槽；按 `position` 查逐槽冻结事实。
    Parameter(usize),
    /// 返回槽；只认 `return_layout`。
    Return,
    /// 嵌套槽（字段 / 元素 / 载荷）；目标合同显式声明固定为 raw 宿主串，
    /// 与 lowering 侧 `freeze_text_representations` 的嵌套读取规则同源。
    Nested,
}

fn derive_export(
    facts: &OvmAbiPlanFacts<'_>,
    export: &OvmAbiExportFacts,
    module_layouts: &mut Vec<OdinLayout>,
    layout_indices: &mut BTreeMap<LayoutId, i32>,
    sum_layout_indices: &BTreeMap<NominalInstanceId, i32>,
    option_box_indices: &mut BTreeMap<String, i32>,
) -> Result<OvmAbiExport> {
    let logical_name = export.logical_name.as_str();
    validate_parameter_identities(logical_name, &export.param_types, &export.param_names)?;
    let mut projection = Projection {
        facts,
        return_layout: export.return_layout,
        entry_layouts: &export.entry_parameter_layouts,
        module_layouts,
        layout_indices,
        sum_layout_indices,
        option_box_indices,
    };
    let mut params = Vec::with_capacity(export.param_names.len());
    for (position, name) in export.param_names.iter().enumerate() {
        let ty = export.param_types.get(position).ok_or_else(|| format!("库导出 `{logical_name}` 参数 {position} 缺少类型事实"))?;
        let structure =
            abi_structure_for_type_with_layouts(ty, facts.aggregate_layouts, facts.aggregate_layout_by_semantic_type, facts.sum_layout_by_semantic_type);
        let projected = projection
            .project(&structure, TextSlot::Parameter(position))
            .map_err(|reason| format!("库导出 `{logical_name}` 参数 `{name}` 没有 OVM ABI 合同: {reason}"))?;
        params.push(OvmAbiParam { index: position, name: name.to_string(), carrier: carrier_label(&projected).to_string(), ty: projected });
    }
    let return_structure = abi_structure_for_type_with_layouts(
        &export.return_type,
        facts.aggregate_layouts,
        facts.aggregate_layout_by_semantic_type,
        facts.sum_layout_by_semantic_type,
    );
    let returns = projection
        .project(&return_structure, TextSlot::Return)
        .map_err(|reason| format!("库导出 `{logical_name}` 返回类型没有 OVM ABI 合同: {reason}"))?;
    Ok(OvmAbiExport {
        logical_name: logical_name.to_string(),
        source_name: export.source_name.clone(),
        params,
        returns_carrier: carrier_label(&returns).to_string(),
        returns,
    })
}

/// 结构 → OVM 物理投影。
///
/// `Unsupported` 与未注册布局都在这里结构化失败；投影成功的结果不含任何
/// “未知”分支，因此宿主不需要也不允许再猜。
struct Projection<'a> {
    facts: &'a OvmAbiPlanFacts<'a>,
    /// 返回槽冻结布局（与 lowering 同源）。
    return_layout: Option<LayoutId>,
    /// 入口参数逐槽冻结布局（与 lowering 同源）。
    entry_layouts: &'a [Option<LayoutId>],
    module_layouts: &'a mut Vec<OdinLayout>,
    layout_indices: &'a mut BTreeMap<LayoutId, i32>,
    sum_layout_indices: &'a BTreeMap<NominalInstanceId, i32>,
    /// 显式 Option box 布局的跨导出去重表（规范化 inner 身份 → 模块下标）。
    option_box_indices: &'a mut BTreeMap<String, i32>,
}

impl Projection<'_> {
    fn project(&mut self, structure: &AbiStructure, slot: TextSlot) -> Result<OvmAbiType> {
        Ok(match structure {
            AbiStructure::Unit => OvmAbiType::Unit,
            AbiStructure::I32 => OvmAbiType::I32,
            AbiStructure::I64 => OvmAbiType::I64,
            AbiStructure::F32 => OvmAbiType::F32,
            AbiStructure::F64 => OvmAbiType::F64,
            AbiStructure::Bool => OvmAbiType::Bool,
            AbiStructure::Char => OvmAbiType::Char,
            AbiStructure::Utf8 => self.project_text(slot)?,
            AbiStructure::Array { element, .. } => OvmAbiType::Array { element: Box::new(self.project(element, TextSlot::Nested)?) },
            AbiStructure::FixedArray { element, length } => {
                OvmAbiType::FixedArray { element: Box::new(self.project(element, TextSlot::Nested)?), length: *length }
            }
            AbiStructure::Option { inner } => {
                let inner = self.project(inner, TextSlot::Nested)?;
                // 内层也是 Option 时，`null` 同时表示外层 None 与内层 None：
                // 必须显式 box，否则 `Some(None)` 与外层 `None` 塌陷成同一物理值。
                let boxing = if matches!(inner, OvmAbiType::Option { .. }) {
                    OvmOptionBoxing::Boxed { layout: self.option_box_layout(&inner)?, tag_slot: 0, payload_slot: 1 }
                }
                else {
                    OvmOptionBoxing::Niche
                };
                OvmAbiType::Option { inner: Box::new(inner), boxing }
            }
            AbiStructure::Tuple { fields } => {
                OvmAbiType::Tuple { fields: fields.iter().map(|field| self.project(field, TextSlot::Nested)).collect::<Result<Vec<_>>>()? }
            }
            AbiStructure::Aggregate(aggregate) => self.project_aggregate(aggregate)?,
            AbiStructure::Sequence(sequence) => {
                // OVM lane 目前不消费宿主序列构造合同：声明级 `[host_projection]`
                // 授权不改变 OVM 的实例身份语义，这里按引用实例句柄维持既有投影，
                // 与同布局未投影时的行为逐字一致，绝不按字段重建实例。
                let aggregate = self
                    .facts
                    .aggregate_layouts
                    .layouts
                    .iter()
                    .find(|item| item.id == sequence.layout)
                    .ok_or_else(|| format!("layout `{}` 未注册，无法投影序列声明 `{}`", sequence.layout, sequence.name))?;
                OvmAbiType::InstanceHandle {
                    layout: layout_index(sequence.layout, aggregate.fields.len(), self.module_layouts, self.layout_indices)?,
                    name: sequence.name.clone(),
                    module_path: sequence.module_path.clone(),
                }
            }
            AbiStructure::InstanceHandle { layout, name, module_path } => {
                let aggregate = self
                    .facts
                    .aggregate_layouts
                    .layouts
                    .iter()
                    .find(|item| item.id == *layout)
                    .ok_or_else(|| format!("layout `{layout}` 未注册，无法投影引用实例 `{module_path}::{name}`"))?;
                OvmAbiType::InstanceHandle {
                    layout: layout_index(*layout, aggregate.fields.len(), self.module_layouts, self.layout_indices)?,
                    name: name.clone(),
                    module_path: module_path.clone(),
                }
            }
            AbiStructure::Unite { nominal, name, variants, payloads, .. } => {
                // 物理 sum 布局由 lowering 已登记的 nominal → 模块下标映射给出；
                // 缺失即失败闭合，绝不在合同里留“未知布局”。
                let layout = *self
                    .sum_layout_indices
                    .get(nominal)
                    .ok_or_else(|| format!("sum nominal `{nominal:?}` 没有已登记的 OVM 模块布局，拒绝按未知布局解释内存"))?;
                let variants = variants
                    .iter()
                    .zip(payloads.iter())
                    .map(|(variant, payload)| {
                        let payload = payload.as_ref().map(|payload| self.project(payload, TextSlot::Nested)).transpose()?.map(Box::new);
                        Ok(OvmAbiVariant { name: variant.name.clone(), tag: variant.tag, payload })
                    })
                    .collect::<Result<Vec<_>>>()?;
                OvmAbiType::Unite { layout, name: name.clone(), tag_slot: 0, payload_slot: 1, variants }
            }
            AbiStructure::Unsupported { reason } => return Err(reason.clone()),
        })
    }

    /// `utf8` 文本边界投影。
    ///
    /// 每个槽的表示都是显式事实：
    /// - 入口参数只认导出事实里的逐槽布局，且与 lowering 同源；该槽解析不到托管
    ///   布局时，只有程序级表示计划显式声明 raw 才是合法 raw 串，否则说明合同与
    ///   运行时布局分叉，直接失败闭合；
    /// - 返回槽只认 `return_layout`（lowering 要求 `utf8` 返回必须是托管对象）；
    /// - 嵌套槽没有逐槽事实，显式落到 raw 宿主串，绝不回查全局布局猜测。
    fn project_text(&mut self, slot: TextSlot) -> Result<OvmAbiType> {
        let layout_id = match slot {
            TextSlot::Parameter(position) => {
                let layout = self.entry_layouts.get(position).copied().flatten();
                if layout.is_none() && self.facts.program_text_representation != Some(ProgramTextRepresentation::Raw) {
                    return Err(format!(
                        "导出参数 #{position} 的 `utf8` 槽没有解析到目标计划已冻结的文本布局：\
                         程序级表示计划没有显式声明 raw 表示，缺登记不是「程序选择 raw」的证据，拒绝按 raw 串解释"
                    ));
                }
                layout
            }
            TextSlot::Return => Some(self.return_layout.ok_or_else(|| {
                "`utf8` 返回值没有目标计划冻结的 `return_layout`：OVM lowering 要求返回槽是托管文本对象，拒绝按 raw 串解释".to_string()
            })?),
            TextSlot::Nested => None,
        };
        let Some(layout_id) = layout_id
        else {
            return Ok(OvmAbiType::Text { representation: OvmTextRepresentation::Raw });
        };
        let layout = find_layout(self.facts.aggregate_layouts, layout_id)?;
        let is_byte_slice = matches!(
            layout.fields.as_slice(),
            [field]
                if matches!(
                    &field.ty,
                    SemanticType::Array(element) | SemanticType::FixedArray { element, .. }
                        if matches!(element.as_ref(), SemanticType::Integer8 { signed: false })
                )
        );
        if !is_byte_slice {
            let shape = layout.fields.iter().map(|field| format!("{}:{}", field.name, field.ty)).collect::<Vec<_>>().join(", ");
            return Err(format!(
                "OVM 文本载体 layout `{}`（id={:?}）缺少冻结的单一 `[u8]` 槽配方：实际 fields=[{shape}]",
                layout.name, layout.id
            ));
        }
        Ok(OvmAbiType::Text {
            representation: OvmTextRepresentation::Managed {
                layout: layout_index(layout_id, layout.fields.len(), self.module_layouts, self.layout_indices)?,
                field_count: layout.fields.len(),
                byte_slot: 0,
            },
        })
    }

    /// 为嵌套 Option 登记显式 2 槽 box 布局；同一 inner 形状跨导出复用同一下标。
    fn option_box_layout(&mut self, inner: &OvmAbiType) -> Result<i32> {
        let key = serde_json::to_string(inner).map_err(|error| format!("Option box 身份无法规范化序列化: {error}"))?;
        if let Some(index) = self.option_box_indices.get(&key) {
            return Ok(*index);
        }
        let index = i32::try_from(self.module_layouts.len()).map_err(|_| "OVM 模块布局表下标溢出，无法登记 Option box".to_string())?;
        self.module_layouts.push(OdinLayout { field_count: 2 });
        self.option_box_indices.insert(key, index);
        Ok(index)
    }

    fn project_aggregate(&mut self, aggregate: &AggregateStructure) -> Result<OvmAbiType> {
        let layout_id = aggregate
            .layout
            .ok_or_else(|| format!("聚合 `{}::{}` 没有冻结布局身份，OVM 拒绝按身份猜字段", aggregate.module_path, aggregate.name))?;
        let layout = find_layout(self.facts.aggregate_layouts, layout_id)?;
        if layout.storage.is_reference() {
            return Err(format!("聚合 `{}::{}` 是引用存储，必须投影为共享身份而不是字段快照", layout.namespace, layout.name));
        }
        let namespace = layout.namespace.clone();
        let layout_name = layout.name.clone();
        let mut fields = Vec::with_capacity(aggregate.fields.len());
        for field in &aggregate.fields {
            fields.push(OvmAbiField {
                name: field.name.clone(),
                ty: self
                    .project(&field.ty, TextSlot::Nested)
                    .map_err(|reason| format!("聚合 `{namespace}::{layout_name}` 字段 `{}` 没有 OVM ABI 合同: {reason}", field.name))?,
            });
        }
        if fields.len() != layout.fields.len() {
            return Err(format!(
                "聚合 `{namespace}::{layout_name}` 的合同字段数（{}）与冻结布局字段数（{}）不一致",
                fields.len(),
                layout.fields.len()
            ));
        }
        Ok(OvmAbiType::Aggregate {
            layout: layout_index(layout_id, layout.fields.len(), self.module_layouts, self.layout_indices)?,
            name: aggregate.name.clone(),
            module_path: aggregate.module_path.clone(),
            fields,
        })
    }
}

fn find_layout(plan: &AggregateLayoutPlan, layout_id: LayoutId) -> Result<&compiler_types::AggregateLayout> {
    plan.layouts.iter().find(|item| item.id == layout_id).ok_or_else(|| format!("layout `{layout_id}` 未注册，无法派生 OVM 物理投影"))
}

/// 把 canonical 布局身份解析为 `.odin` 模块布局下标；尚未登记的布局按冻结
/// 字段数追加到表尾，绝不复用其它布局的下标。
fn layout_index(
    layout_id: LayoutId,
    field_count: usize,
    module_layouts: &mut Vec<OdinLayout>,
    layout_indices: &mut BTreeMap<LayoutId, i32>,
) -> Result<i32> {
    if let Some(index) = layout_indices.get(&layout_id) {
        return Ok(*index);
    }
    let field_count = i32::try_from(field_count).map_err(|_| format!("layout `{layout_id}` 字段数 `{field_count}` 超出 OVM 槽位范围"))?;
    let index = i32::try_from(module_layouts.len()).map_err(|_| format!("OVM 模块布局表下标溢出，无法登记 layout `{layout_id}`"))?;
    module_layouts.push(OdinLayout { field_count });
    layout_indices.insert(layout_id, index);
    Ok(index)
}

/// 载体的 JSON 标签。
///
/// OVM 只输出本 lane 的投影标签：raw 宿主串标 `text`，托管文本对象标 `handle`。
/// 这里不读取、也不比较 Wasm 的载体表。
fn carrier_label(ty: &OvmAbiType) -> &'static str {
    match ty {
        OvmAbiType::Text { representation: OvmTextRepresentation::Raw } => "text",
        OvmAbiType::Text { representation: OvmTextRepresentation::Managed { .. } } => "handle",
        OvmAbiType::Unit => "void",
        OvmAbiType::I32 | OvmAbiType::Bool | OvmAbiType::Char => "i32",
        OvmAbiType::I64 => "i64",
        OvmAbiType::F32 => "f32",
        OvmAbiType::F64 => "f64",
        OvmAbiType::Array { .. }
        | OvmAbiType::FixedArray { .. }
        | OvmAbiType::Option { .. }
        | OvmAbiType::Tuple { .. }
        | OvmAbiType::Aggregate { .. }
        | OvmAbiType::InstanceHandle { .. }
        | OvmAbiType::Unite { .. } => "handle",
    }
}

/// 合同 digest：覆盖 schema 版本与按逻辑名排序的导出事实。
fn contract_digest(exports: &[OvmAbiExport]) -> Result<String> {
    #[derive(Serialize)]
    struct DigestInput<'a> {
        schema_version: u32,
        lane: &'a str,
        exports: &'a [OvmAbiExport],
    }
    let canonical = serde_json::to_string(&DigestInput { schema_version: OVM_ABI_SCHEMA_VERSION, lane: "ovm", exports })
        .map_err(|error| format!("OVM ABI 合同无法规范化序列化: {error}"))?;
    Ok(format!("sha256-{}", combined_content_hash(&[&canonical])))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use compiler_types::{AggregateLayout, FieldLayout, NominalInstanceId, StorageKind, SumTypeLayout, SumVariantLayout, VariantId};

    use super::*;

    fn named_type(s: &str) -> SemanticType {
        crate::test_canonical_primitive(s)
    }

    /// 测试用的导出事实构造器：默认无入口槽布局、无返回布局。
    fn export(logical_name: &str, params: &[(&str, SemanticType)], return_type: SemanticType) -> OvmAbiExportFacts {
        OvmAbiExportFacts {
            logical_name: logical_name.to_string(),
            source_name: logical_name.to_string(),
            param_types: params.iter().map(|(_, ty)| ty.clone()).collect(),
            param_names: params.iter().map(|(name, _)| (*name).to_string()).collect(),
            return_type,
            entry_parameter_layouts: vec![None; params.len()],
            return_layout: None,
        }
    }

    #[derive(Default)]
    struct TestFacts {
        aggregate_layouts: AggregateLayoutPlan,
        by_semantic: BTreeMap<SemanticType, LayoutId>,
        sums: BTreeMap<SemanticType, SumTypeLayout>,
        program_text: Option<ProgramTextRepresentation>,
    }

    impl TestFacts {
        fn register_type_layout(&mut self, ty: SemanticType, layout: AggregateLayout) {
            self.aggregate_layouts.layouts.push(layout.clone());
            self.by_semantic.insert(ty, layout.id);
        }

        fn register_sum_layout(&mut self, ty: SemanticType, sum: SumTypeLayout) {
            self.sums.insert(ty, sum);
        }

        fn plan_facts(&self) -> OvmAbiPlanFacts<'_> {
            OvmAbiPlanFacts {
                aggregate_layouts: &self.aggregate_layouts,
                aggregate_layout_by_semantic_type: &self.by_semantic,
                sum_layout_by_semantic_type: &self.sums,
                program_text_representation: self.program_text,
            }
        }
    }

    fn derive_with_layouts(facts: &TestFacts, exports: &[OvmAbiExportFacts]) -> Result<(OvmAbiContract, Vec<OdinLayout>)> {
        let mut module_layouts = Vec::new();
        let mut layout_indices = BTreeMap::new();
        // 生产 lowering 先登记 unite 的 `[tag, payload]` 布局；测试里按 nominal
        // 一一补上 2 槽布局，再交给合同派生，保证 `verify_against_layouts` 有真布局可查。
        let mut sum_layout_indices = BTreeMap::new();
        for sum in facts.sums.values() {
            if sum_layout_indices.contains_key(&sum.nominal) {
                continue;
            }
            let index = i32::try_from(module_layouts.len()).expect("测试布局下标");
            module_layouts.push(OdinLayout { field_count: 2 });
            sum_layout_indices.insert(sum.nominal, index);
        }
        let contract = derive_contract(&facts.plan_facts(), exports, &mut module_layouts, &mut layout_indices, &sum_layout_indices)?;
        Ok((contract, module_layouts))
    }

    fn derive(facts: &TestFacts, exports: &[OvmAbiExportFacts]) -> Result<OvmAbiContract> {
        derive_with_layouts(facts, exports).map(|(contract, _)| contract)
    }

    fn layout(id: LayoutId, name: &str, storage: StorageKind, fields: Vec<(&str, SemanticType)>) -> AggregateLayout {
        AggregateLayout {
            id,
            name: name.to_string(),
            namespace: "demo".to_string(),
            key: None,
            storage,
            fields: fields.into_iter().map(|(name, ty)| FieldLayout { name: name.to_string(), ty }).collect(),
        }
    }

    fn export_of(contract: &OvmAbiContract) -> &OvmAbiExport {
        contract.exports.first().expect("至少一个导出")
    }

    fn export_of_mut(contract: &mut OvmAbiContract) -> &mut OvmAbiExport {
        contract.exports.first_mut().expect("至少一个导出")
    }

    #[test]
    fn heap_array_of_i64_projects_elementwise() {
        let ty = SemanticType::Array(Box::new(named_type("i64")));
        let facts = TestFacts::default();
        let contract = derive(&facts, &[export("take", &[("values", ty.clone())], ty)]).expect("`[i64]` 必须有 OVM 合同");
        let export = export_of(&contract);
        assert_eq!(export.params[0].ty, OvmAbiType::Array { element: Box::new(OvmAbiType::I64) });
        assert_eq!(export.params[0].carrier, "handle");
        assert_eq!(export.returns, OvmAbiType::Array { element: Box::new(OvmAbiType::I64) });
        assert!(contract.digest.starts_with("sha256-"), "digest: {}", contract.digest);
    }

    #[test]
    fn named_struct_projects_layout_slots() {
        let ty = SemanticType::Nominal { layout: 7, spelling: compiler_target::Identifier::new("demo::Point") };
        let mut facts = TestFacts::default();
        facts.register_type_layout(ty.clone(), layout(7, "Point", StorageKind::Value, vec![("x", named_type("i64")), ("y", named_type("i64"))]));
        let contract = derive(&facts, &[export("point", &[("point", ty.clone())], ty)]).expect("具名聚合必须有 OVM 合同");
        let OvmAbiType::Aggregate { layout: index, name, module_path, fields } = &export_of(&contract).params[0].ty
        else {
            panic!("具名 struct 必须投影为 Aggregate")
        };
        assert_eq!(*index, 0, "首个 ABI 布局登记为下标 0");
        assert_eq!(name, "Point");
        assert_eq!(module_path, "demo");
        assert_eq!(
            fields.iter().map(|field| (field.name.as_str(), field.ty.clone())).collect::<Vec<_>>(),
            vec![("x", OvmAbiType::I64), ("y", OvmAbiType::I64)]
        );
    }

    #[test]
    fn nested_aggregate_projects_recursively() {
        let inner_ty = SemanticType::Nominal { layout: 8, spelling: compiler_target::Identifier::new("demo::Inner") };
        let outer_ty = SemanticType::Nominal { layout: 7, spelling: compiler_target::Identifier::new("demo::Outer") };
        let mut facts = TestFacts::default();
        facts.register_type_layout(
            inner_ty.clone(),
            layout(8, "Inner", StorageKind::Value, vec![("value", named_type("i64")), ("tag", named_type("i32"))]),
        );
        facts.register_type_layout(outer_ty.clone(), layout(7, "Outer", StorageKind::Value, vec![("inner", inner_ty), ("count", named_type("i32"))]));
        let contract = derive(&facts, &[export("outer", &[("value", outer_ty.clone())], outer_ty)]).expect("嵌套聚合必须有 OVM 合同");
        let OvmAbiType::Aggregate { fields, .. } = &export_of(&contract).params[0].ty
        else {
            panic!("外层必须是 Aggregate")
        };
        let OvmAbiType::Aggregate { layout: inner_index, fields: inner_fields, .. } = &fields[0].ty
        else {
            panic!("嵌套字段必须是 Aggregate")
        };
        assert_eq!(*inner_index, 0);
        assert_eq!(inner_fields.len(), 2);
        assert_eq!(fields[1].ty, OvmAbiType::I32);
    }

    #[test]
    fn generic_reference_instance_keeps_identity_despite_array_and_integer_fields() {
        let list = SemanticType::Apply(Box::new(SemanticType::Named(compiler_target::Identifier::new("std::collections::ArrayList"))), vec![named_type("i64")]);
        let mut facts = TestFacts::default();
        facts.register_type_layout(
            list.clone(),
            layout(
                9,
                "ArrayList",
                StorageKind::Reference,
                vec![("_items", SemanticType::Array(Box::new(named_type("i64")))), ("_length", named_type("i32"))],
            ),
        );
        let contract = derive(&facts, &[export("list", &[("values", list.clone())], list)]).expect("泛型实例必须有 OVM 合同");
        let OvmAbiType::InstanceHandle { layout: index, name, .. } = &export_of(&contract).params[0].ty
        else {
            panic!("引用存储实例必须保留 InstanceHandle 身份")
        };
        assert_eq!(*index, 0);
        assert_eq!(name, "ArrayList");
        assert_eq!(export_of(&contract).returns, export_of(&contract).params[0].ty);
    }

    #[test]
    fn raw_utf8_parameter_uses_host_text_carrier() {
        let ty = named_type("utf8");
        let mut facts = TestFacts::default();
        // raw 槽的合法性来自程序级表示计划的显式声明，不是布局表为空的反推。
        facts.program_text = Some(ProgramTextRepresentation::Raw);
        let contract = derive(&facts, &[export("greet", &[("name", ty)], named_type("unit"))]).expect("`utf8` 必须有 OVM 合同");
        let export = export_of(&contract);
        assert_eq!(export.params[0].ty, OvmAbiType::Text { representation: OvmTextRepresentation::Raw });
        assert_eq!(export.params[0].carrier, "text");
        assert_eq!(export.returns, OvmAbiType::Unit);
        assert_eq!(export.returns_carrier, "void");
    }

    /// 反例：布局事实为空且程序级表示计划未显式声明 raw——缺登记不是「程序选择
    /// raw」的证据，合同派生必须失败闭合，不得把 missing 吸收成 raw 串。
    #[test]
    fn utf8_parameter_without_explicit_raw_fact_fails_closed() {
        let ty = named_type("utf8");
        let facts = TestFacts::default();
        let error = derive(&facts, &[export("greet", &[("name", ty)], named_type("unit"))]).expect_err("布局事实为空不得反推 raw，缺程序级显式事实必须失败闭合");
        assert!(error.contains("拒绝按 raw 串解释"), "错误必须点名禁止 raw 吸收，实际: {error}");
        assert!(error.contains("没有显式声明 raw"), "错误必须点名缺失的程序级显式声明，实际: {error}");
    }

    #[test]
    fn utf8_return_without_frozen_layout_fails_closed() {
        let ty = named_type("utf8");
        let mut facts = TestFacts::default();
        // 形参 raw 槽由程序级显式声明授权，让断言聚焦返回槽自己的失败闭合。
        facts.program_text = Some(ProgramTextRepresentation::Raw);
        let error = derive(&facts, &[export("greet", &[("name", ty.clone())], ty)]).expect_err("`utf8` 返回槽没有 return_layout 必须失败闭合");
        assert!(error.contains("return_layout"), "got: {error}");
    }

    #[test]
    fn raw_and_managed_text_slots_coexist_in_one_contract() {
        let text_ty = named_type("utf8");
        let text_layout = layout(11, "Utf8Text", StorageKind::Reference, vec![("_repr", SemanticType::Array(Box::new(named_type("u8"))))]);
        let mut facts = TestFacts::default();
        // 只登记布局表、不登记 `aggregate_layout_by_semantic_type`：托管表示只来自
        // 该槽自己的入口布局事实；raw 槽由程序级显式 raw 声明授权，
        // 不是布局表为空的反推。
        facts.program_text = Some(ProgramTextRepresentation::Raw);
        facts.aggregate_layouts.layouts.push(text_layout);
        let mut raw = export("raw", &[("name", text_ty.clone())], named_type("unit"));
        let mut managed = export("managed", &[("name", text_ty.clone())], named_type("unit"));
        managed.entry_parameter_layouts = vec![Some(11u32)];
        let contract = derive(&facts, &[raw, managed]).expect("同一合同两种文本表示都必须按各自事实解释");
        let raw = contract.export("raw").expect("raw 导出");
        let managed = contract.export("managed").expect("managed 导出");
        assert_eq!(raw.params[0].ty, OvmAbiType::Text { representation: OvmTextRepresentation::Raw });
        assert_eq!(raw.params[0].carrier, "text");
        assert_eq!(
            managed.params[0].ty,
            OvmAbiType::Text { representation: OvmTextRepresentation::Managed { layout: 0, field_count: 1, byte_slot: 0 } }
        );
        assert_eq!(managed.params[0].carrier, "handle");
    }

    #[test]
    fn contract_layout_mismatch_fails_closed() {
        let ty = named_type("utf8");
        let mut facts = TestFacts::default();
        facts.register_type_layout(
            ty.clone(),
            layout(11, "Utf8Text", StorageKind::Reference, vec![("_repr", SemanticType::Array(Box::new(named_type("u8"))))]),
        );
        let mut export = export("greet", &[("name", ty)], named_type("unit"));
        export.entry_parameter_layouts = vec![Some(11u32)];
        let (contract, _) = derive_with_layouts(&facts, &[export]).expect("合同");
        // 模拟 `.odin` 布局表与合同声明不一致：托管文本槽声明 1 槽，产物却是 2 槽。
        let error = contract.verify_against_layouts(&[OdinLayout { field_count: 2 }]).expect_err("合同与运行时布局不一致必须失败闭合");
        assert!(error.contains("不一致"), "got: {error}");
    }

    #[test]
    fn sidecar_hash_covers_whole_artifact() {
        let facts = TestFacts::default();
        let contract = derive(&facts, &[export("first", &[("left", named_type("i32"))], named_type("i32"))]).expect("合同");
        let bytes = b"odin-artifact-bytes";
        let sidecar = OvmAbiSidecar::new(contract, "app.odin".to_string(), bytes);
        assert!(sidecar.artifact_sha256.starts_with("sha256-"), "哈希必须带算法前缀: {}", sidecar.artifact_sha256);
        sidecar.verify_against(bytes).expect("同一份产物字节必须通过");
        let mut tampered = bytes.to_vec();
        tampered.push(0);
        assert!(sidecar.verify_against(&tampered).is_err(), "产物字节改动后必须失败闭合");
    }

    #[test]
    fn option_uses_niche_when_inner_is_not_option() {
        let ty = SemanticType::Nullable(Box::new(named_type("i64")));
        let facts = TestFacts::default();
        let contract = derive(&facts, &[export("maybe", &[("value", ty.clone())], ty)]).expect("`i64?` 必须有 OVM 合同");
        assert_eq!(export_of(&contract).params[0].ty, OvmAbiType::Option { inner: Box::new(OvmAbiType::I64), boxing: OvmOptionBoxing::Niche });
    }

    #[test]
    fn nested_option_projects_explicit_box_and_three_shapes_are_distinct() {
        let ty = SemanticType::Nullable(Box::new(SemanticType::Nullable(Box::new(named_type("i64")))));
        let facts = TestFacts::default();
        let (contract, _) = derive_with_layouts(&facts, &[export("maybe", &[("value", ty.clone())], ty)]).expect("`i64??` 必须有 OVM 合同");
        let returns = &export_of(&contract).returns;
        let OvmAbiType::Option { inner, boxing } = returns
        else {
            panic!("外层必须是 Option")
        };
        let OvmOptionBoxing::Boxed { layout, tag_slot, payload_slot } = *boxing
        else {
            panic!("嵌套 Option 必须显式 box，禁止 Niche 塌陷")
        };
        assert_eq!((layout, tag_slot, payload_slot), (0, 0, 1), "box 的冻结配方");
        assert_eq!(**inner, OvmAbiType::Option { inner: Box::new(OvmAbiType::I64), boxing: OvmOptionBoxing::Niche });
        // 三种形态按合同编码：`None` / `Some(None)` / `Some(Some(x))` 必须两两可区分。
        let shapes = encode_option_shapes(returns);
        assert_ne!(shapes[0], shapes[1], "`None` 与 `Some(None)` 不得塌陷");
        assert_ne!(shapes[1], shapes[2], "`Some(None)` 与 `Some(Some(x))` 不得塌陷");
        assert_ne!(shapes[0], shapes[2], "`None` 与 `Some(Some(x))` 不得塌陷");
    }

    #[test]
    fn collapsed_nested_option_fails_verification() {
        let ty = SemanticType::Nullable(Box::new(SemanticType::Nullable(Box::new(named_type("i64")))));
        let facts = TestFacts::default();
        let (mut contract, layouts) = derive_with_layouts(&facts, &[export("maybe", &[("value", ty.clone())], ty)]).expect("合同");
        // 人为把外层退回 `Niche`：这正是 `Some(None)` 与 `None` 物理同形的塌陷合同。
        let OvmAbiType::Option { boxing, .. } = &mut export_of_mut(&mut contract).returns
        else {
            panic!("外层必须是 Option")
        };
        *boxing = OvmOptionBoxing::Niche;
        let error = contract.verify_against_layouts(&layouts).expect_err("塌陷合同必须失败闭合");
        assert!(error.contains("塌陷"), "got: {error}");
    }

    /// 按合同编码 `None` / `Some(None)` / `Some(Some(x))`；只看合同，不看运行期类型。
    fn encode_option_shapes(ty: &OvmAbiType) -> [String; 3] {
        let OvmAbiType::Option { boxing, .. } = ty
        else {
            panic!("必须是 Option")
        };
        match boxing {
            OvmOptionBoxing::Niche => ["null".to_string(), "null".to_string(), "7".to_string()],
            OvmOptionBoxing::Boxed { layout, tag_slot, payload_slot } => [
                "null".to_string(),
                format!("object(layout={layout},slot{tag_slot}=1,slot{payload_slot}=null)"),
                format!("object(layout={layout},slot{tag_slot}=1,slot{payload_slot}=7)"),
            ],
        }
    }

    #[test]
    fn managed_utf8_parameter_uses_frozen_text_layout() {
        let ty = named_type("utf8");
        let mut facts = TestFacts::default();
        facts.register_type_layout(
            ty.clone(),
            layout(11, "Utf8Text", StorageKind::Reference, vec![("_repr", SemanticType::Array(Box::new(named_type("u8"))))]),
        );
        // 入口槽布局事实由语言侧结算（value_layouts → 语义表回落的同源派生）。
        let mut export = export("greet", &[("name", ty)], named_type("unit"));
        export.entry_parameter_layouts = vec![Some(11u32)];
        let contract = derive(&facts, &[export]).expect("托管文本必须有 OVM 合同");
        let export = export_of(&contract);
        assert_eq!(
            export.params[0].ty,
            OvmAbiType::Text { representation: OvmTextRepresentation::Managed { layout: 0, field_count: 1, byte_slot: 0 } }
        );
        assert_eq!(export.params[0].carrier, "handle");
    }

    #[test]
    fn text_layout_without_single_byte_array_slot_fails_closed() {
        let ty = named_type("utf8");
        let mut facts = TestFacts::default();
        facts.register_type_layout(ty.clone(), layout(11, "Utf8Text", StorageKind::Reference, vec![("_repr", named_type("i64"))]));
        let mut export = export("greet", &[("name", ty)], named_type("unit"));
        export.entry_parameter_layouts = vec![Some(11u32)];
        let error = derive(&facts, &[export]).expect_err("非 `[u8]` 文本布局必须失败");
        assert!(error.contains("OVM 文本载体"), "got: {error}");
    }

    #[test]
    fn same_layout_facts_derive_same_digest() {
        let ty = SemanticType::Nominal { layout: 7, spelling: compiler_target::Identifier::new("demo::Point") };
        let build = || {
            let mut facts = TestFacts::default();
            facts.register_type_layout(
                ty.clone(),
                layout(7, "Point", StorageKind::Value, vec![("x", named_type("i64")), ("y", named_type("i64"))]),
            );
            derive(&facts, &[export("point", &[("point", ty.clone())], ty.clone())]).expect("合同")
        };
        let first = build();
        let second = build();
        assert_eq!(first.digest, second.digest, "同一布局事实必须派生同一 digest");
        assert_eq!(first.exports, second.exports);
    }

    #[test]
    fn unknown_layout_fails_closed() {
        let ty = SemanticType::Nominal { layout: 99, spelling: compiler_target::Identifier::new("demo::Ghost") };
        let facts = TestFacts::default();
        let error = derive(&facts, &[export("ghost", &[("ghost", ty.clone())], ty)]).expect_err("未注册布局必须失败");
        assert!(error.contains("未注册"), "got: {error}");
    }

    #[test]
    fn unsupported_scalar_export_fails_before_contract() {
        let facts = TestFacts::default();
        for ty in [
            named_type("i128"),
            SemanticType::Utf16,
            SemanticType::TraitObject(compiler_types::WitnessObject { trait_path: compiler_target::Identifier::new("demo::Shape"), type_arguments: Vec::new() }),
        ] {
            let error = derive(&facts, &[export("bad", &[("value", named_type("i64"))], ty)]).expect_err("不支持的导出类型必须失败");
            assert!(!error.is_empty(), "错误必须携带具体原因");
        }
    }

    #[test]
    fn empty_boundary_exports_get_a_contract_with_stable_digest() {
        // OVM 的导出面来自声明导出名 / 程序入口，不在 wasm 冻结缓存里。
        // 零参数、`Void` 返回或完全无可携带内容都必须拿到**空的但存在**的合同，
        // 不能把“没有需要携带的东西”误判成“缺少载体合同”。
        let facts = TestFacts::default();
        let cases: [(&str, Vec<(&str, SemanticType)>, SemanticType, usize, &str); 3] = [
            ("zero_void", Vec::new(), named_type("unit"), 0, "void"),
            ("zero_i64", Vec::new(), named_type("i64"), 0, "i64"),
            ("arg_void", vec![("value", named_type("i64"))], named_type("unit"), 1, "void"),
        ];
        for (name, params, returns, param_count, returns_carrier) in cases {
            let build = || derive_with_layouts(&facts, &[export(name, &params, returns.clone())]).expect("空边界必须有合同");
            let (contract, layouts) = build();
            let export = export_of(&contract);
            assert!(layouts.is_empty(), "空边界不得凭空登记布局: {layouts:?}");
            assert_eq!(export.params.len(), param_count, "{name} 的参数槽数必须与声明一致");
            assert_eq!(export.returns_carrier, returns_carrier, "{name} 的返回载体");
            assert!(contract.digest.starts_with("sha256-"), "{name} digest: {}", contract.digest);
            let (again, _) = build();
            assert_eq!(contract.digest, again.digest, "{name} 相同边界事实必须派生稳定 digest");
        }
    }

    /// 不可携带的边界类型必须由本 lane 的结构投影直接失败，不依赖任何外部
    /// 平台的预冻结载体表。
    #[test]
    fn unsupported_export_types_fail_derive() {
        let facts = TestFacts::default();
        for ty in [
            named_type("i128"),
            SemanticType::Utf16,
            SemanticType::TraitObject(compiler_types::WitnessObject { trait_path: compiler_target::Identifier::new("demo::Shape"), type_arguments: Vec::new() }),
        ] {
            let error = derive(&facts, &[export("bad", &[("value", named_type("i64"))], ty)]).expect_err("不可携带类型必须结构化失败");
            assert!(error.contains("没有 OVM ABI 合同"), "got: {error}");
        }
    }

    #[test]
    fn unsupported_aggregate_field_fails_closed() {
        let ty = SemanticType::Nominal { layout: 8, spelling: compiler_target::Identifier::new("demo::Heavy") };
        let mut facts = TestFacts::default();
        facts.register_type_layout(
            ty.clone(),
            layout(8, "Heavy", StorageKind::Value, vec![("big", named_type("i128")), ("name", named_type("utf8"))]),
        );
        let error = derive(&facts, &[export("heavy", &[("heavy", ty.clone())], ty)]).expect_err("聚合字段不支持必须失败");
        assert!(error.contains("Heavy"), "got: {error}");
        assert!(error.contains("没有 OVM ABI 合同"), "got: {error}");
    }

    #[test]
    fn digest_changes_with_layout_shape() {
        let ty = SemanticType::Nominal { layout: 7, spelling: compiler_target::Identifier::new("demo::Point") };
        let build = |fields: Vec<(&str, SemanticType)>| {
            let mut facts = TestFacts::default();
            facts.register_type_layout(ty.clone(), layout(7, "Point", StorageKind::Value, fields));
            derive(&facts, &[export("point", &[("point", ty.clone())], ty.clone())]).expect("合同").digest
        };
        let narrow = build(vec![("x", named_type("i64"))]);
        let wide = build(vec![("x", named_type("i64")), ("y", named_type("i64"))]);
        assert_ne!(narrow, wide, "布局字段形状必须进入 digest");
    }

    #[test]
    fn unite_projects_variant_payloads_with_frozen_slots() {
        let sum_ty = SemanticType::Nominal { layout: 42, spelling: compiler_target::Identifier::new("demo::Maybe") };
        let mut facts = TestFacts::default();
        facts.register_sum_layout(
            sum_ty.clone(),
            SumTypeLayout {
                nominal: NominalInstanceId::from_index(0).expect("nominal"),
                name: "Maybe".to_string(),
                is_unite: true,
                variants: vec![
                    SumVariantLayout { id: VariantId::from_index(0).expect("variant"), name: "None".to_string(), tag: 0, payload_type: None },
                    SumVariantLayout {
                        id: VariantId::from_index(1).expect("variant"),
                        name: "Some".to_string(),
                        tag: 1,
                        payload_type: Some(named_type("i64")),
                    },
                ],
            },
        );
        let (contract, layouts) = derive_with_layouts(&facts, &[export("maybe", &[("value", sum_ty.clone())], sum_ty)]).expect("unite 必须派生 OVM 合同");
        let OvmAbiType::Unite { layout, name, tag_slot, payload_slot, variants } = &export_of(&contract).params[0].ty
        else {
            panic!("unite 必须投影为 Unite 合同")
        };
        assert_eq!(*layout, 0, "unite 布局下标来自 lowering 登记表");
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].field_count, 2, "unite 物理布局是 [tag, payload]");
        assert_eq!(name, "Maybe");
        assert_eq!((*tag_slot, *payload_slot), (0, 1), "tag / payload 槽位是冻结配方");
        assert_eq!(
            variants.iter().map(|variant| (variant.name.as_str(), variant.tag, variant.payload.clone())).collect::<Vec<_>>(),
            vec![("None", 0, None), ("Some", 1, Some(Box::new(OvmAbiType::I64)))],
            "带载荷变体必须携带自己的 payload 结构，零载荷变体不得伪造载荷"
        );
    }

    /// 参数身份合同：数量不一致、空名、重名都必须失败闭合，不得合成名义名。
    #[test]
    fn parameter_identity_violations_fail_closed() {
        let facts = TestFacts::default();
        let mut count_mismatch = export("bad", &[("value", named_type("i64"))], named_type("i64"));
        count_mismatch.param_names.push("extra".to_string());
        let error = derive(&facts, &[count_mismatch]).expect_err("参数身份数量不一致必须失败");
        assert!(error.contains("不一致"), "got: {error}");
        let empty = export("bad", &[("", named_type("i64"))], named_type("i64"));
        let error = derive(&facts, &[empty]).expect_err("空参数名必须失败");
        assert!(error.contains("缺少 canonical 参数名"), "got: {error}");
        let duplicate = export("bad", &[("value", named_type("i64")), ("value", named_type("i64"))], named_type("i64"));
        let error = derive(&facts, &[duplicate]).expect_err("重复参数名必须失败");
        assert!(error.contains("重复"), "got: {error}");
    }
}
