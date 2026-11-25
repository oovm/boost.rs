//! Wasm singleton 元数据与访问器编码。
//!
//! 只消费 [`super::input`] 结算出的私有事实；上游 `SingletonInstancePlan` 与
//! 物理布局解析已在 settle 处完成，本模块不再接触跨目标公共 IR。

use crate::{
    emitter_backend_wasi::WasmBinaryModule,
    lowering::backends::wasm::mir::{WasmBodyInstruction, WasmModuleInput, WasmSingletonFact, encode_wasm_body},
};
use data_binary_wasm::WasmOpcode;

use crate::lowering::{
    backends::wasm,
    features::semantic_mir_contract::SemanticMirContractError,
};

pub(crate) fn singleton_metadata_line(fact: &WasmSingletonFact) -> String {
    let mode = if fact.is_lazy { "lazy" } else { "static" };
    let ctor = fact.constructor_symbol.as_deref().unwrap_or("-");
    let fin = fact.finalizer_symbol.as_deref().unwrap_or("-");
    let unload = if fact.supports_unload { "unload" } else { "-" };
    format!("{}|{}|{}|{}|{}|{}|{}|{}", fact.namespace, fact.name, fact.instance_field, mode, fact.accessor_method, ctor, fin, unload,)
}

fn nyar_singleton_export_name(fact: &WasmSingletonFact) -> String {
    format!("{}__{}", fact.name, fact.accessor_method)
}

/// Builds the OVM export name for a lazy singleton's `unload` accessor.
pub(crate) fn nyar_singleton_unload_export_name(fact: &WasmSingletonFact) -> String {
    format!("{}__{}", fact.name, compiler_types::SINGLETON_UNLOAD_ACCESSOR)
}

pub(crate) fn nyar_singleton_accessor_export_name(fact: &WasmSingletonFact) -> String {
    nyar_singleton_export_name(fact)
}

/// Append singleton metadata sections for backends without dedicated singleton slots.
pub(crate) fn append_singleton_metadata_sections(module: &mut WasmBinaryModule, input: &WasmModuleInput) {
    for fact in &input.singleton_facts {
        let payload = singleton_metadata_line(fact);
        module.sections.insert(
            0,
            crate::emitter_backend_wasi::WasmSection {
                id: 0,
                name: Some(format!("nyar.singleton.{}", fact.name)),
                bytes: payload.into_bytes(),
            },
        );
    }
}

/// 为 WASM 模块注入 singleton INSTANCE 全局与 accessor 导出函数。
///
/// 对每个 singleton 事实生成：
/// - 一个可变 `i32` 全局，存储 INSTANCE 线性内存指针（与 heap global 共存；heap 占 index 0）。
/// - 一个导出函数 `{name}__{accessor}`：
///   - eager 模式：直接 `global.get` 返回全局。
///   - lazy 模式：判空后经 `cabi_realloc(0,0,align,size)` 真实 bump 分配，写回全局。
///
/// 尺寸与对齐只能来自 settle 结算的聚合物理布局；缺失时返回结构化拒绝，
/// 不得由 backend 以目标默认值补造表示事实。
pub(crate) fn augment_wasm_with_singleton_accessors(module: &mut WasmBinaryModule, input: &WasmModuleInput) -> Result<(), SemanticMirContractError> {
    if input.singleton_facts.is_empty() {
        return Ok(());
    }

    // 先完成全部布局校验，再改写模块；避免在拒绝时留下半成品 section。
    // 语言 MIR 只提供语义布局；Wasm singleton 的分配尺寸按其目标数据布局解析
    // （settle 已按 WASM32 解析完毕，缺失即上游没有注册 INSTANCE 布局）。
    let allocations = input
        .singleton_facts
        .iter()
        .map(|fact| match (fact.layout_size, fact.layout_align) {
            (Some(size), Some(align)) => Ok((size.max(8), align.max(1))),
            _ => Err(SemanticMirContractError {
                code: "SMIR010",
                function: fact.name.clone(),
                location: "singleton instance".to_string(),
                detail: format!(
                    "singleton `{}` in namespace `{}` has no aggregate layout; backend must not synthesize size or alignment",
                    fact.name, fact.namespace
                ),
            }),
        })
        .collect::<Result<Vec<_>, SemanticMirContractError>>()?;

    let realloc_index = ensure_wasm_cabi_realloc(module);

    let import_count = wasm::count_wasm_function_imports(module);
    let decl_count = wasm::count_wasm_function_decls(module);
    let first_new_function_index = import_count + decl_count;

    let accessor_type_index = wasm::count_wasm_types(module);
    let accessor_type = wasm::wasm_function_type(&[], &[0x7F]);
    wasm::append_wasm_types(module, &[accessor_type]);

    let singleton_count = input.singleton_facts.len() as u32;
    let global_base = wasm::append_wasm_i32_globals(module, &vec![0; singleton_count as usize]);

    let type_indices: Vec<u32> = vec![accessor_type_index; singleton_count as usize];
    wasm::append_wasm_function_decls(module, &type_indices);

    let exports: Vec<(String, u8, u32)> = input
        .singleton_facts
        .iter()
        .enumerate()
        .map(|(index, fact)| (nyar_singleton_export_name(fact), 0x00, first_new_function_index + index as u32))
        .collect();
    wasm::append_wasm_exports(module, &exports);

    let bodies: Vec<Vec<u8>> = input
        .singleton_facts
        .iter()
        .enumerate()
        .map(|(index, fact)| {
            let (size, align) = allocations[index];
            wasm_singleton_accessor_body(global_base + index as u32, fact.is_lazy, realloc_index, size, align)
        })
        .collect();
    wasm::append_wasm_code_bodies(module, &bodies);
    Ok(())
}

fn ensure_wasm_cabi_realloc(module: &mut WasmBinaryModule) -> u32 {
    if wasm::count_wasm_globals(module) == 0 {
        wasm::insert_wasm_section(module, wasm::cabi_heap_global_section(wasm::CABI_HEAP_DEFAULT_BASE));
    }
    if let Some(index) = find_wasm_export_func(module, "cabi_realloc") {
        return index;
    }
    let realloc_type = wasm::count_wasm_types(module);
    wasm::append_wasm_types(module, &[wasm::wasm_function_type(&[0x7F, 0x7F, 0x7F, 0x7F], &[0x7F])]);
    let func_index = wasm::count_wasm_function_imports(module) + wasm::count_wasm_function_decls(module);
    wasm::append_wasm_function_decls(module, &[realloc_type]);
    wasm::append_wasm_code_bodies(module, &[wasm::wasm_cabi_realloc_bump_body()]);
    wasm::append_wasm_exports(module, &[("cabi_realloc".to_string(), 0x00, func_index)]);
    func_index
}

fn find_wasm_export_func(module: &WasmBinaryModule, name: &str) -> Option<u32> {
    let section = module.sections.iter().find(|item| item.id == 7)?;
    let bytes = &section.bytes;
    let mut pos = 0;
    let count = wasm::decode_uleb128(bytes, &mut pos);
    for _ in 0..count {
        let name_len = wasm::decode_uleb128(bytes, &mut pos) as usize;
        let export_name = std::str::from_utf8(&bytes[pos..pos + name_len]).ok()?;
        pos += name_len;
        let kind = *bytes.get(pos)?;
        pos += 1;
        let index = wasm::decode_uleb128(bytes, &mut pos);
        if export_name == name && kind == 0x00 {
            return Some(index);
        }
    }
    None
}

/// 生成 WASM accessor 函数体字节。
///
/// eager：`global.get N; end`
/// lazy：判空后 `cabi_realloc(0,0,align,size)` 分配并写回。
fn wasm_singleton_accessor_body(global_index: u32, is_lazy: bool, realloc_index: u32, size: u32, align: u32) -> Vec<u8> {
    let mut lir = Vec::new();
    if is_lazy {
        lir.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::GlobalGet, operand: global_index });
        lir.push(WasmBodyInstruction::Opcode(WasmOpcode::I32Eqz));
        lir.push(WasmBodyInstruction::BlockType { opcode: WasmOpcode::If, block_type: 0x40 });
        lir.push(WasmBodyInstruction::I32Const(0));
        lir.push(WasmBodyInstruction::I32Const(0));
        lir.push(WasmBodyInstruction::I32Const(align.max(1) as i32));
        lir.push(WasmBodyInstruction::I32Const(size.max(8) as i32));
        lir.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Call, operand: realloc_index });
        lir.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::GlobalSet, operand: global_index });
        lir.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
    }
    lir.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::GlobalGet, operand: global_index });
    lir.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
    let mut body = vec![0x00];
    body.extend(encode_wasm_body(&lir));
    body
}
