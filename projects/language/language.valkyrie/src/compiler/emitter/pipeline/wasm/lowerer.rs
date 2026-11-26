//! Wasm lowering：直接消费 Valkyrie MIR，产出目标私有结算输入（[`WasmModuleInput`]）。
//!
//! 该降低器归语言 × 目标适配器所有：它读取语言自己的 SSA MIR 与冻结表示合同，
//! 投影为 `compiler_emitter` wasm lane 的目标私有词汇，不经过任何跨目标公共 IR。
//! 语言语义在这里一次结算完成：名义/泛型实例解析为布局引用（`WasmTypeFact::Layout`），
//! 缺闭合实例布局即结构化失败，不把源语言类型镜像交给后端回猜；指令与终结符的
//! 形状校验在 [`super::lane::build_function_view`] 已完成，这里只做身份映射与
//! 布局绑定。

use std::collections::BTreeMap;

use compiler_emitter::{
    ProgramTextRepresentation,
    emitter_backend_wasm::{
        LayoutPolicy, ResolvedAggregateLayout, WasmAbiVariant, WasmArrayInit, WasmBlock, WasmBlockKey, WasmCallableKey, WasmConstant,
        WasmFieldFact, WasmFieldKey, WasmFloat64Bits, WasmFunctionInput, WasmFunctionType, WasmImportFact, WasmImportLinkFact, WasmInstruction,
        WasmInstructionKey, WasmLayoutFact, WasmLayoutKeyFact, WasmModuleInput, WasmNominalKey, WasmOperation, WasmOperand,
        WasmProgramTextRepresentation, WasmSingletonFact, WasmStorageClass, WasmSumLayoutFact, WasmSumRep, WasmSumVariantFact,
        WasmSumVariantLayoutFact, WasmTerminator, WasmTypeFact, WasmValue, WasmValueKey, WasmValueOrigin, WasmVariantKey,
        resolve_physical_layouts, sum_identity_facts,
    },
};
use compiler_types::{
    CanonicalTypeKind, ExternalImportLink, ItemInstanceId, LayoutKey, SemanticType, StorageKind, SumTypeLayout,
    semantic_ids::{MirValueId, TypeId},
};

use crate::compiler::valkyrie_mir::{
    ValkyrieMirArrayInitialization, ValkyrieMirCallee, ValkyrieMirConstant, ValkyrieMirEffectKind, ValkyrieMirFunction, ValkyrieMirInstruction,
    ValkyrieMirOperand, ValkyrieMirOperation, ValkyrieMirTerminator,
};

use crate::compiler::emitter::pipeline::lane::LaneContext;

type Result<T> = std::result::Result<T, String>;

/// 从 lane 上下文结算 wasm 模块私有输入（mir/ 编码层消费的唯一形状）。
///
/// 实例顺序取可达闭包按身份升序（与旧公共路径 `instances()` 的 BTreeMap 序
/// 一致）；物理布局在这里一次性按 WASM32 解析（原先发生在 settle 处）。
pub(crate) fn build_module_input(ctx: &LaneContext, theory_bundle: &compiler_optimize::TheoryBundle) -> Result<WasmModuleInput> {
    let mut operations: Vec<ItemInstanceId> = ctx.reachable().to_vec();
    operations.sort();
    let instances = operations.iter().map(|instance| WasmCallableKey(instance.index())).collect();
    let mut functions = BTreeMap::new();
    for instance in &operations {
        functions.insert(WasmCallableKey(instance.index()), build_function(ctx, *instance)?);
    }

    let fragment = ctx.fragment();
    let entry_operation = fragment.entry_operation.map(|instance| WasmCallableKey(instance.index()));
    let export_names = fragment.export_names.iter().map(|(instance, name)| (WasmCallableKey(instance.index()), name.clone())).collect();
    let export_annotations = fragment.export_annotations.iter().map(|(instance, name)| (WasmCallableKey(instance.index()), name.clone())).collect();
    let exported_operations = fragment.exported_operations.iter().map(|instance| WasmCallableKey(instance.index())).collect();
    let mut abi_labels = BTreeMap::new();
    for instance in &operations {
        if let Some(name) = ctx.abi_name(instance) {
            abi_labels.insert(WasmCallableKey(instance.index()), name.to_string());
        }
    }
    let merged_theory = theory_bundle.merged();
    let theory_rules = merged_theory.rules.len();
    let theory_equations = merged_theory.equations.len();

    let imports = ctx
        .imports()
        .iter()
        .map(|(instance, import)| -> Result<_> {
            Ok((
                WasmCallableKey(instance.index()),
                WasmImportFact {
                    links: import.links.iter().map(settle_import_link).collect(),
                    parameter_types: import.parameter_types.iter().map(|ty| settle_type(ctx, ty)).collect::<Result<Vec<_>>>()?,
                    return_type: settle_type(ctx, &import.return_type)?,
                },
            ))
        })
        .collect::<Result<_>>()?;
    let external_import_links = fragment
        .external_imports
        .iter()
        .map(|(instance, links)| (WasmCallableKey(instance.index()), links.iter().map(settle_import_link).collect()))
        .collect();

    let resolved = resolve_physical_layouts(ctx.aggregate_layouts(), LayoutPolicy::WASM32)?;
    let layouts = resolved.layouts.iter().map(|layout| settle_layout(ctx, layout)).collect::<Result<Vec<_>>>()?;
    let layout_by_nominal_id = ctx.aggregate_layout_by_nominal().iter().map(|(nominal, id)| (WasmNominalKey(nominal.index()), *id)).collect();
    let layout_by_field_id =
        ctx.aggregate_layout_by_field().iter().map(|(field, (id, slot))| (WasmFieldKey(field.index()), (*id, *slot))).collect();
    // sum 实例按布局引用键控：类型事实里的 `Layout` 直接命中冻结 sum 合同，
    // 不再维护「类型拼写 → 布局」的第二套索引。
    let sum_layouts_by_type = ctx
        .sum_layout_by_semantic_type()
        .iter()
        .map(|(ty, sum)| -> Result<_> {
            let fact = settle_sum_layout(ctx, sum)?;
            match settle_type(ctx, ty)? {
                WasmTypeFact::Layout(layout) => Ok((layout, fact)),
                other => Err(format!("WASM 结算：sum 实例类型结算结果 `{other:?}` 不是布局引用")),
            }
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let sum_reps = ctx
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
                        .map(|(variant, contract)| {
                            (WasmVariantKey(variant.index()), WasmSumVariantFact { tag: contract.tag, has_payload: contract.payload_type.is_some() })
                        })
                        .collect(),
                },
            )
        })
        .collect();
    let program_text_representation = ctx.program_text_representation().map(|representation| match representation {
        ProgramTextRepresentation::Raw => WasmProgramTextRepresentation::Raw,
        ProgramTextRepresentation::Managed => WasmProgramTextRepresentation::Managed,
    });
    let singleton_facts = ctx
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
    let host_sequence_projections = ctx.aggregate_layouts().host_sequence_projections.iter().copied().collect();

    Ok(WasmModuleInput {
        functions,
        instances,
        entry_operation,
        export_names,
        export_annotations,
        exported_operations,
        abi_labels,
        module_name: ctx.linked().module_name.clone(),
        fragment_id: fragment.id.as_str().to_string(),
        required_capabilities: fragment.required_capabilities.iter().map(|capability| capability.as_str().to_string()).collect(),
        theory_rules,
        theory_equations,
        imports,
        external_import_links,
        layouts,
        layout_by_nominal_id,
        layout_by_field_id,
        sum_layouts_by_type,
        sum_reps,
        program_text_representation,
        singleton_facts,
        host_sequence_projections,
        // 冻结 ABI 合同与宿主导入 provider 由 `build_wasm_program_input` 在
        // 结算期补齐（需要完整 module input 与宿主边界事实）。
        export_abis: BTreeMap::new(),
        host_import_providers: Vec::new(),
    })
}

/// 单函数结算：符号、类型与布局身份全部取 lane 已冻结的事实；指令臂只做
/// 私有身份映射（`TypeId → WasmTypeFact`、`*Id → Wasm*Key`）。
fn build_function(ctx: &LaneContext, instance: ItemInstanceId) -> Result<WasmFunctionInput> {
    let view = ctx.view(&instance).ok_or_else(|| format!("WASM lowering：可达实例 `{instance:?}` 缺少函数视图"))?;
    let function = ctx.function(&instance).ok_or_else(|| format!("WASM lowering：可达实例 `{instance:?}` 缺少函数体"))?;
    let symbol = view.symbol.clone();
    let return_type = settle_type(ctx, &view.return_type)?;
    let param_types = view.param_types.iter().map(|ty| settle_type(ctx, ty)).collect::<Result<Vec<_>>>()?;
    let value_types = view.value_types.iter().map(|(value, ty)| Ok((WasmValueKey(value.index()), settle_type(ctx, ty)?))).collect::<Result<_>>()?;
    let value_layouts = function
        .value_types
        .iter()
        .filter_map(|(value, type_id)| ctx.aggregate_layout_by_type().get(type_id).copied().map(|layout| (WasmValueKey(value.index()), layout)))
        .collect();
    let return_layout = ctx.aggregate_layout_by_type().get(&function.return_type).copied();
    let entry = WasmBlockKey(function.entry.0);
    let mut values =
        function.value_types.keys().map(|value| WasmValue { id: WasmValueKey(value.index()), origin: WasmValueOrigin::Temporary }).collect::<Vec<_>>();
    for (index, (value, _)) in function.parameters.iter().enumerate() {
        let row = values.iter_mut().find(|row| row.id.0 == value.index()).ok_or_else(|| "入口参数缺少 SSA 值合同".to_string())?;
        // 形参声明名的唯一权威是 canonical 冻结的 `parameter_names`；缺失时不得
        // 合成 `arg{index}` 之类名义名，否则下游会把缺失事实当成合法 ABI 名。
        let name = function
            .parameter_names
            .get(index)
            .cloned()
            .ok_or_else(|| format!("函数实例 `{:?}` 入口参数 {index} 缺少 canonical 冻结的声明参数名", function.instance))?;
        row.origin = WasmValueOrigin::Parameter { index, name };
    }
    let blocks = function
        .blocks
        .values()
        .map(|block| -> Result<WasmBlock> {
            let instructions = block
                .instructions
                .iter()
                .map(|instruction| -> Result<WasmInstruction> {
                    Ok(WasmInstruction {
                        id: WasmInstructionKey(instruction.id.index()),
                        results: instruction.results.iter().map(|value| WasmValueKey(value.index())).collect(),
                        kind: lower_operation(ctx, function, instruction)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(WasmBlock {
                id: WasmBlockKey(block.id.0),
                label: format!("block_{}", block.id.0),
                parameters: block.parameters.iter().map(|(value, _)| WasmValueKey(value.index())).collect(),
                instructions,
                terminator: lower_terminator(&block.terminator)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let parameter_identities = values
        .iter()
        .filter_map(|value| match &value.origin {
            WasmValueOrigin::Parameter { index, name } => Some((*index, name.clone())),
            _ => None,
        })
        .collect();
    Ok(WasmFunctionInput { symbol, return_type, param_types, value_types, value_layouts, return_layout, entry, values, blocks, parameter_identities })
}

/// canonical 操作 → wasm 私有指令载荷：布局身份、tuple 形状与函数值合同
/// 在这里按链接事实校验（与旧公共路径 `lower_operation` 同文案），其余臂
/// 只做身份映射。
fn lower_operation(ctx: &LaneContext, function: &ValkyrieMirFunction, instruction: &ValkyrieMirInstruction) -> Result<WasmOperation> {
    let value = |id: MirValueId| WasmOperand::Value(WasmValueKey(id.index()));
    let layout_for_type = |type_id: TypeId, operation: &str| {
        ctx.aggregate_layout_by_type()
            .get(&type_id)
            .copied()
            .ok_or_else(|| format!("{operation} 缺少编译器绑定的布局身份: {type_id:?}"))
    };
    let value_type = |value: MirValueId| {
        function.value_types.get(&value).copied().ok_or_else(|| format!("聚合值 `{value:?}` 缺少 Semantic MIR 类型事实"))
    };
    Ok(match &instruction.operation {
        ValkyrieMirOperation::Invoke { callee: ValkyrieMirCallee::Item(instance), arguments } => {
            if !ctx.linked().item_instances.contains_key(instance) {
                return Err(format!("调用 identity `{instance:?}` 未解析"));
            }
            WasmOperation::Call { callee: WasmOperand::Item(WasmCallableKey(instance.index())), arguments: arguments.iter().map(|id| value(*id)).collect() }
        }
        // 具体 witness 必须在 canonical 实例化固定点消去为 `ValkyrieMirCallee::Item`；
        // 走到目标层仍保留 `Witness` 说明上游未闭合，必须结构化失败而不是按
        // `(method declaration, self 类型)` 在这里再选一次实现。
        ValkyrieMirOperation::Invoke { callee: ValkyrieMirCallee::Witness(witness), .. } => {
            return Err(format!(
                "canonical 未消去的 trait witness 分派到达目标层：trait={:?} method={:?} self_type={:?}；\
                 具体 witness 必须在实例化固定点冻结 `ItemInstanceId`",
                witness.trait_declaration, witness.method_declaration, witness.self_type
            ));
        }
        ValkyrieMirOperation::Invoke { callee: ValkyrieMirCallee::Value(callee), arguments } => {
            WasmOperation::Call { callee: value(*callee), arguments: arguments.iter().map(|id| value(*id)).collect() }
        }
        // 具名函数作值：实例身份由前端冻结，目标层只校验链接事实并保留身份。
        ValkyrieMirOperation::LoadFunctionRef { instance, environment, captured } => {
            if !ctx.linked().item_instances.contains_key(instance) {
                return Err(format!("函数值 identity `{instance:?}` 未解析"));
            }
            let environment = match environment {
                Some(ValkyrieMirOperand::Value(id)) => Some(WasmOperand::Value(WasmValueKey(id.index()))),
                Some(ValkyrieMirOperand::Constant(_)) => return Err("闭包捕获环境必须是已定义的 SSA 值".to_string()),
                None => None,
            };
            WasmOperation::LoadFunctionRef { instance: WasmCallableKey(instance.index()), environment, captured: captured.clone() }
        }
        ValkyrieMirOperation::Copy { source } => WasmOperation::Copy { source: value(*source) },
        ValkyrieMirOperation::StoreVar { name: _, value: stored, ty } => WasmOperation::StoreVar {
            value: value(*stored),
            ty: ty.map(|id| -> Result<WasmTypeFact> { settle_type(ctx, &ctx.lower_type(id)?) }).transpose()?,
        },
        ValkyrieMirOperation::AggregateCopy { source, destination } => {
            let source_type = value_type(*source)?;
            let destination_type = value_type(*destination)?;
            if source_type != destination_type {
                return Err(format!("AggregateCopy 源与目标类型不一致: {source_type:?} != {destination_type:?}"));
            }
            WasmOperation::AggregateCopy {
                layout_id: layout_for_type(source_type, "AggregateCopy")?,
                source: value(*source),
                dest: value(*destination),
            }
        }
        // 文本常量的编码身份由常量变体自身决定（`Utf8` / `Utf16`），不靠名字或源码回推。
        ValkyrieMirOperation::LoadConstant { constant } => {
            WasmOperation::LoadConstant { constant: settle_constant(constant), ty: constant_encoding_type(constant) }
        }
        ValkyrieMirOperation::ArrayGet { array, index } => WasmOperation::ArrayGet { array: value(*array), index: value(*index) },
        ValkyrieMirOperation::ArraySet { array, index, value: stored } => {
            WasmOperation::ArraySet { array: value(*array), index: value(*index), value: value(*stored) }
        }
        ValkyrieMirOperation::ArrayLength { array } => WasmOperation::ArrayLength { array: value(*array) },
        ValkyrieMirOperation::NullIsNull { value: operand } => WasmOperation::NullIsNull { value: value(*operand) },
        ValkyrieMirOperation::NullUnwrap { value: operand } => WasmOperation::NullUnwrap { value: value(*operand) },
        ValkyrieMirOperation::ArrayNew { array_type, length, initialization } => WasmOperation::ArrayNew {
            array_type: settle_type(ctx, &ctx.lower_type(*array_type)?)?,
            length: value(*length),
            initialization: match initialization {
                ValkyrieMirArrayInitialization::Default => WasmArrayInit::Default,
                ValkyrieMirArrayInitialization::Fill(fill) => WasmArrayInit::Fill(value(*fill)),
            },
        },
        ValkyrieMirOperation::ArrayFromElements { array_type, elements } => {
            // 只有 FixedArray 才有聚合布局；动态数组没有对象 body 布局，
            // 不得用 dummy 身份污染私有输入。
            let layout_id = match ctx.linked().types.get(array_type).map(|row| &row.kind) {
                Some(CanonicalTypeKind::Array { length: Some(_), .. }) => Some(layout_for_type(*array_type, "ArrayFromElements")?),
                _ => None,
            };
            WasmOperation::ArrayFromElements {
                layout_id,
                array_type: settle_type(ctx, &ctx.lower_type(*array_type)?)?,
                elements: elements.iter().map(|id| value(*id)).collect(),
            }
        }
        ValkyrieMirOperation::TupleNew { fields, element_types } => {
            let result = instruction.results.first().copied().ok_or_else(|| "TupleNew 缺少结果值".to_string())?;
            let result_type = value_type(result)?;
            // 标签只属于 canonical 身份；目标布局按位置，带标签元组与位置元组共享同一构造合同。
            if !matches!(
                ctx.linked().types.get(&result_type).map(|row| &row.kind),
                Some(CanonicalTypeKind::Tuple(_) | CanonicalTypeKind::LabeledTuple { .. })
            ) {
                return Err(format!("TupleNew 结果不是 tuple 类型: {result_type:?}"));
            }
            if fields.len() != element_types.len() {
                return Err("TupleNew 元素合同长度不一致".to_string());
            }
            WasmOperation::TupleNew { layout_id: layout_for_type(result_type, "TupleNew")?, fields: fields.iter().map(|id| value(*id)).collect() }
        }
        ValkyrieMirOperation::TupleGet { object, tuple_type, index } => {
            let arity = match ctx.linked().types.get(tuple_type).map(|row| &row.kind) {
                Some(CanonicalTypeKind::Tuple(members)) => members.len(),
                Some(CanonicalTypeKind::LabeledTuple { fields }) => fields.len(),
                _ => return Err(format!("TupleGet 对象不是元组类型: {tuple_type:?}")),
            };
            if (*index as usize) >= arity {
                return Err(format!("TupleGet 位置索引 {index} 越界（元组长度 {arity}）"));
            }
            WasmOperation::TupleGet { object: value(*object), layout_id: layout_for_type(*tuple_type, "TupleGet")?, index: *index }
        }
        ValkyrieMirOperation::StructNew { nominal, fields } => WasmOperation::StructNew {
            nominal: WasmNominalKey(nominal.index()),
            fields: fields.iter().map(|(field, id)| (WasmFieldKey(field.index()), value(*id))).collect(),
        },
        ValkyrieMirOperation::FieldGet { object, field } => WasmOperation::FieldGet { object: value(*object), field: WasmFieldKey(field.index()) },
        ValkyrieMirOperation::FieldSet { object, field, value: stored } => WasmOperation::FieldSet {
            object: value(*object),
            field: WasmFieldKey(field.index()),
            value: value(*stored),
        },
        ValkyrieMirOperation::SumNew { nominal, variant, payload } => WasmOperation::SumNew {
            nominal: WasmNominalKey(nominal.index()),
            variant: WasmVariantKey(variant.index()),
            payload: payload.map(value),
        },
        ValkyrieMirOperation::SumPayloadGet { nominal, variant, object } => WasmOperation::SumPayloadGet {
            nominal: WasmNominalKey(nominal.index()),
            variant: WasmVariantKey(variant.index()),
            object: value(*object),
        },
        ValkyrieMirOperation::SumVariantIs { nominal, variant, object } => WasmOperation::SumVariantIs {
            nominal: WasmNominalKey(nominal.index()),
            variant: WasmVariantKey(variant.index()),
            object: value(*object),
        },
        // 语言层显式 `as`：目标类型是后端必须编码的事实，不能丢成 Copy。
        ValkyrieMirOperation::Cast { source, ty } => {
            WasmOperation::Cast { source: value(*source), ty: settle_type(ctx, &ctx.lower_type(*ty)?)? }
        }
        unsupported => return Err(format!("canonical 操作尚无目标私有合同: {unsupported:?}")),
    })
}

/// canonical 终结符 → wasm 私有终结符：`Raise` 是唯一进入合同的 effect
/// （lane 已在函数视图处拒绝其余形状）。
fn lower_terminator(terminator: &ValkyrieMirTerminator) -> Result<WasmTerminator> {
    let value = |id: MirValueId| WasmOperand::Value(WasmValueKey(id.index()));
    Ok(match terminator {
        ValkyrieMirTerminator::Return { value: result } => WasmTerminator::Return { value: result.as_ref().map(|id| value(*id)) },
        ValkyrieMirTerminator::Jump { target, arguments } => WasmTerminator::Jump {
            target: WasmBlockKey(target.0),
            arguments: arguments.iter().map(|id| value(*id)).collect(),
        },
        ValkyrieMirTerminator::Branch { condition, then_target, else_target } => WasmTerminator::Branch {
            condition: value(*condition),
            then_target: WasmBlockKey(then_target.0),
            else_target: WasmBlockKey(else_target.0),
        },
        ValkyrieMirTerminator::Unreachable => WasmTerminator::Unreachable,
        ValkyrieMirTerminator::PerformEffect { effect: ValkyrieMirEffectKind::Raise, payload, resume_target } => WasmTerminator::Raise {
            payload: payload.as_ref().map(|id| value(*id)),
            resume_target: WasmBlockKey(resume_target.0),
        },
        unsupported => {
            return Err(format!("canonical effect terminator 尚无目标合同: {unsupported:?}"));
        }
    })
}

/// 文本常量的编码身份由常量变体自身决定；整数/浮点/空值没有在这里声称的编码身份。
fn constant_encoding_type(constant: &ValkyrieMirConstant) -> Option<WasmTypeFact> {
    match constant {
        ValkyrieMirConstant::Utf8(_) => Some(WasmTypeFact::Utf8),
        ValkyrieMirConstant::Utf16(_) => Some(WasmTypeFact::Utf16),
        _ => None,
    }
}

fn settle_constant(constant: &ValkyrieMirConstant) -> WasmConstant {
    match constant {
        ValkyrieMirConstant::Int(value) => WasmConstant::Int(*value),
        // 位模式原样重建：`-0.0` 与 NaN payload 不在结算层被规范化。
        ValkyrieMirConstant::Float64(bits) => WasmConstant::Float64(WasmFloat64Bits(f64::from_bits(*bits))),
        ValkyrieMirConstant::Bool(value) => WasmConstant::Bool(*value),
        ValkyrieMirConstant::Utf8(value) => WasmConstant::Utf8(value.clone()),
        ValkyrieMirConstant::Utf16(value) => WasmConstant::Utf16(value.clone()),
        ValkyrieMirConstant::Unit => WasmConstant::Unit,
        ValkyrieMirConstant::Null => WasmConstant::Null,
    }
}

/// canonical 语义类型 → wasm 目标类型事实（D5 架构门）。
///
/// 语言语义在这里一次结算为目标词汇：名义实例与闭合泛型实例都解析为
/// canonical 冻结的**布局引用**（`WasmTypeFact::Layout`）；缺闭合实例布局
/// 时结构化失败并报出确切缺项，绝不把源语言拼写、泛型应用、witness 表或
/// 匿名 union 镜像进目标层让后端回猜。
pub(crate) fn settle_type(ctx: &LaneContext, ty: &SemanticType) -> Result<WasmTypeFact> {
    match ty {
        SemanticType::Bottom => Ok(WasmTypeFact::Bottom),
        SemanticType::Unit => Ok(WasmTypeFact::Unit),
        SemanticType::Boolean => Ok(WasmTypeFact::Boolean),
        SemanticType::Integer8 { signed } => Ok(WasmTypeFact::Integer8 { signed: *signed }),
        SemanticType::Integer16 { signed } => Ok(WasmTypeFact::Integer16 { signed: *signed }),
        SemanticType::Integer32 { signed } => Ok(WasmTypeFact::Integer32 { signed: *signed }),
        SemanticType::Integer64 { signed } => Ok(WasmTypeFact::Integer64 { signed: *signed }),
        SemanticType::Integer128 { signed } => Ok(WasmTypeFact::Integer128 { signed: *signed }),
        SemanticType::Float32 => Ok(WasmTypeFact::Float32),
        SemanticType::Float64 => Ok(WasmTypeFact::Float64),
        SemanticType::Character => Ok(WasmTypeFact::Character),
        SemanticType::Utf8 => Ok(WasmTypeFact::Utf8),
        SemanticType::Utf16 => Ok(WasmTypeFact::Utf16),
        SemanticType::Named(name) => Err(format!(
            "WASM 结算：裸名义拼写 `{name}` 没有冻结布局身份；名义类型必须由 canonical 实例化结算为布局引用"
        )),
        SemanticType::Nominal { layout, .. } => Ok(WasmTypeFact::Layout(*layout)),
        SemanticType::Apply(..) => ctx
            .aggregate_layout_by_semantic_type()
            .get(ty)
            .copied()
            .map(WasmTypeFact::Layout)
            .ok_or_else(|| {
                format!("WASM 结算：闭合泛型实例 `{ty:?}` 缺少 canonical 冻结的布局身份；目标层不得按拼写回猜布局")
            }),
        SemanticType::Function(function) => Ok(WasmTypeFact::Function(Box::new(WasmFunctionType {
            params: function.params.iter().map(|param| settle_type(ctx, param)).collect::<Result<Vec<_>>>()?,
            return_type: settle_type(ctx, &function.return_type)?,
        }))),
        SemanticType::Tuple(items) => Ok(WasmTypeFact::Tuple(items.iter().map(|item| settle_type(ctx, item)).collect::<Result<Vec<_>>>()?)),
        SemanticType::Array(element) => Ok(WasmTypeFact::Array(Box::new(settle_type(ctx, element)?))),
        SemanticType::FixedArray { element, length } => Ok(WasmTypeFact::FixedArray { element: Box::new(settle_type(ctx, element)?), length: *length }),
        SemanticType::TraitObject(object) => Err(format!(
            "WASM 结算：witness 表类型 `dyn {}` 没有目标编码合同；trait 分派必须在 canonical 消为具体实例",
            object.trait_path
        )),
        SemanticType::Nullable(element) => Ok(WasmTypeFact::Nullable(Box::new(settle_type(ctx, element)?))),
        SemanticType::Union(arms) => Err(format!(
            "WASM 结算：匿名 union `{}` 没有目标编码合同；语言语义必须结算为显式布局引用",
            arms.iter().map(|arm| format!("{arm:?}")).collect::<Vec<_>>().join(" | ")
        )),
    }
}

fn settle_layout(ctx: &LaneContext, layout: &ResolvedAggregateLayout) -> Result<WasmLayoutFact> {
    Ok(WasmLayoutFact {
        id: layout.id,
        name: layout.name.clone(),
        namespace: layout.namespace.clone(),
        key: layout.key.as_ref().map(|key| settle_layout_key(ctx, key)).transpose()?,
        storage: match layout.storage {
            StorageKind::Value => WasmStorageClass::Value,
            StorageKind::Reference => WasmStorageClass::Reference,
        },
        size: layout.size,
        align: layout.align,
        fields: layout
            .fields
            .iter()
            .map(|field| -> Result<WasmFieldFact> {
                Ok(WasmFieldFact {
                    name: field.name.clone(),
                    ty: settle_type(ctx, &field.ty)?,
                    offset: field.offset,
                    size: field.size,
                    align: field.align,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn settle_layout_key(ctx: &LaneContext, key: &LayoutKey) -> Result<WasmLayoutKeyFact> {
    match key {
        LayoutKey::Tuple(items) => Ok(WasmLayoutKeyFact::Tuple(items.iter().map(|item| settle_type(ctx, item)).collect::<Result<Vec<_>>>()?)),
        LayoutKey::FixedArray { element, length } => {
            Ok(WasmLayoutKeyFact::FixedArray { element: Box::new(settle_type(ctx, element)?), length: *length })
        }
    }
}

fn settle_import_link(link: &ExternalImportLink) -> WasmImportLinkFact {
    WasmImportLinkFact { boundary: link.boundary.clone(), platform_tag: link.platform_tag.clone(), locator_segments: link.locator_segments.clone() }
}

fn settle_sum_layout(ctx: &LaneContext, sum: &SumTypeLayout) -> Result<WasmSumLayoutFact> {
    let variants = sum
        .variants
        .iter()
        .map(|variant| -> Result<WasmSumVariantLayoutFact> {
            Ok(WasmSumVariantLayoutFact {
                id: WasmVariantKey(variant.id.index()),
                name: variant.name.clone(),
                tag: variant.tag,
                payload_type: variant.payload_type.as_ref().map(|payload| settle_type(ctx, payload)).transpose()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let abi_variants = variants.iter().map(|variant| WasmAbiVariant { name: variant.name.clone(), tag: variant.tag }).collect::<Vec<_>>();
    Ok(WasmSumLayoutFact {
        nominal: WasmNominalKey(sum.nominal.index()),
        name: sum.name.clone(),
        is_unite: sum.is_unite,
        variants,
        identity: sum_identity_facts(&sum.name, sum.is_unite, &abi_variants),
    })
}
