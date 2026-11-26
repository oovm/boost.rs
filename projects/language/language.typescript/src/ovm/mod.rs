//! TS × OVM 第一投影适配器：残差 IR → OVM LIR → `.odin` 字节。
//!
//! 本层只做**纯结构映射**：每个目标无关残差操作（[`crate::residual`]）映射到
//! 对应的 OVM 头码或 `odin.host` 宿主导入，不含任何已知值求解、TS 语义裁决
//! 或 AST 遍历。语义决策全部在语言侧完成：静态可判定的在特化期结算进残差，
//! 动态不可判定的走内核未收录的 `ts.*` provider 符号——provider 缺席即装载
//! 失败（失败闭合）。

use compiler_emitter::emitter_backend_vm::{assemble_ovm_module, encode_odin_function, sanitize_symbol, BytecodeEmitter, OvmProgramInput};
use data_binary_odin::{
    encode_module, OdinConstant, OdinExport, OdinExportKind, OdinFunction, OdinHeadCode, OdinImport, OdinLayout, OdinModuleData, Odin_VERSION,
};
use language_typescript_ir::Program;

use crate::hir;
use crate::mir;
use crate::residual::{ResidualConst, ResidualOp, ResidualProgram};
use crate::specialize::{self, SpecializeFailure};

/// 语言 provider 必须注册的全部 `ts.*` 符号（内核未收录，缺席即装载失败）。
pub const TS_PROVIDER_SYMBOLS: &[&str] = &["ts.add", "ts.truthy", "ts.strictEqual", "ts.num_to_string", "ts.log"];

/// 第一投影产物。
#[derive(Debug, Clone)]
pub struct OvmArtifact {
    /// `.odin` 字节。
    pub bytes: Vec<u8>,
    /// 模块数据（取证入口）。
    pub module: OdinModuleData,
    /// 布局下标 → 冻结字段名（按名字排序）。宿主构造 guest 对象时使用。
    pub layout_fields: Vec<Vec<String>>,
}

/// 程序 → `.odin` 的第一投影管线：AST → HIR → MIR → 残差 → LIR。
pub fn specialize_to_odin(program: &Program, module_name: &str) -> Result<OvmArtifact, SpecializeFailure> {
    let hir = hir::lower(program);
    let mir = mir::lower(&hir);
    let residual = specialize::specialize(&mir)?;
    to_odin(&residual, module_name)
}

/// 残差程序 → `.odin` 产物：残差操作到 OVM LIR 的结构映射。
pub fn to_odin(residual: &ResidualProgram, module_name: &str) -> Result<OvmArtifact, SpecializeFailure> {
    let mut imports: Vec<OdinImport> = Vec::new();
    let mut constants: Vec<OdinConstant> = Vec::new();
    let mut code: Vec<u8> = Vec::new();
    let mut functions: Vec<OdinFunction> = Vec::new();
    let mut exports: Vec<OdinExport> = Vec::new();
    for (index, function) in residual.functions.iter().enumerate() {
        let constants_base = constants.len() as i32;
        let mut emitter = BytecodeEmitter::new(constants_base, &mut imports);
        for operation in &function.body {
            emit_operation(&mut emitter, operation);
        }
        let encoded = encode_odin_function(&emitter.instructions)
            .map_err(|error| SpecializeFailure::Unsupported { construct: "OVM LIR 编码".to_string(), detail: error })?;
        debug_assert_eq!(functions.len(), index);
        let code_offset = code.len() as i32;
        code.extend_from_slice(&encoded);
        constants.extend(std::mem::take(&mut emitter.constants));
        functions.push(OdinFunction {
            name: sanitize_symbol(&function.name),
            arity: function.arity as i32,
            local_count: function.slots.max(function.arity) as i32,
            code_offset,
            code_length: encoded.len() as i32,
        });
        if function.exported {
            exports.push(OdinExport { kind: OdinExportKind::Function, symbol_name: sanitize_symbol(&function.name), function_index: index as i32 });
        }
    }
    let layouts: Vec<OdinLayout> = residual.layouts.iter().map(|names| OdinLayout { field_count: names.len() as i32 }).collect();
    let module = OdinModuleData {
        version: Odin_VERSION,
        name: module_name.to_string(),
        constants,
        functions,
        imports,
        exports,
        witness_entries: Vec::new(),
        code_bytes: code,
        globals: Vec::new(),
        init_function_indices: Vec::new(),
        layouts,
    };
    let (module, _abi) = assemble_ovm_module(&OvmProgramInput::new(module, None))
        .map_err(|error| SpecializeFailure::Unsupported { construct: "OVM 装配".to_string(), detail: error })?;
    Ok(OvmArtifact { bytes: encode_module(&module), module, layout_fields: residual.layouts.clone() })
}

/// 残差操作 → OVM LIR 映射表。
fn emit_operation(emitter: &mut BytecodeEmitter<'_>, operation: &ResidualOp) {
    match operation {
        ResidualOp::Const(constant) => match constant {
            ResidualConst::Undefined | ResidualConst::Null => emitter.emit_const_null(),
            ResidualConst::Bool(flag) => emitter.emit_const_bool(*flag),
            ResidualConst::Number(number) => emitter.emit_const_f64(number.to_bits()),
            ResidualConst::String(text) => {
                let index = emitter.intern_string(text);
                emitter.emit_imm1(OdinHeadCode::Constant, index);
            }
        },
        ResidualOp::Load(slot) => emitter.emit_imm1(OdinHeadCode::LoadLocal, *slot as i32),
        ResidualOp::Store(slot) => emitter.emit_imm1(OdinHeadCode::StoreLocal, *slot as i32),
        ResidualOp::Pop => emitter.emit_plain(OdinHeadCode::Pop),
        ResidualOp::Dup => emitter.emit_plain(OdinHeadCode::Dup),
        ResidualOp::NumAdd => emitter.emit_call_import("f64_add", 2),
        ResidualOp::NumSub => emitter.emit_call_import("f64_sub", 2),
        ResidualOp::NumMul => emitter.emit_call_import("f64_mul", 2),
        ResidualOp::NumDiv => emitter.emit_call_import("f64_div", 2),
        ResidualOp::NumNeg => emitter.emit_call_import("f64_neg", 1),
        ResidualOp::StrConcat => emitter.emit_call_import("utf8.concat", 2),
        ResidualOp::NumToText => emitter.emit_call_import("ts.num_to_string", 1),
        ResidualOp::DynamicAdd => emitter.emit_call_import("ts.add", 2),
        ResidualOp::Truthy => emitter.emit_call_import("ts.truthy", 1),
        ResidualOp::BoolNot => emit_bool_flip(emitter),
        ResidualOp::NumLt => emitter.emit_call_import("f64_lt", 2),
        ResidualOp::NumLe => emitter.emit_call_import("f64_le", 2),
        ResidualOp::NumGt => emitter.emit_call_import("f64_gt", 2),
        ResidualOp::NumGe => emitter.emit_call_import("f64_ge", 2),
        ResidualOp::StrEq => emitter.emit_call_import("utf8.eq", 2),
        ResidualOp::NumEq => emitter.emit_call_import("f64_eq", 2),
        ResidualOp::DynamicStrictEq => emitter.emit_call_import("ts.strictEqual", 2),
        ResidualOp::Call(index) => emitter.emit_imm1(OdinHeadCode::CallStatic, *index as i32),
        ResidualOp::ObjectNew(layout) => emitter.emit_imm1(OdinHeadCode::ObjectNew, *layout as i32),
        // OVM 对象自带布局身份，字段槽按冻结名序定位；残差布局下标供校验与其它目标使用。
        ResidualOp::FieldSet(_, slot) => emitter.emit_imm1(OdinHeadCode::FieldSet, *slot as i32),
        ResidualOp::FieldGet(_, slot) => emitter.emit_imm1(OdinHeadCode::FieldGet, *slot as i32),
        ResidualOp::HostLog { arity } => emitter.emit_call_import("ts.log", *arity as i32),
        ResidualOp::Label(label) => emitter.emit_label(*label),
        ResidualOp::JumpIfFalse(label) => emitter.emit_jump_to_label(OdinHeadCode::JumpIfFalse, *label),
        ResidualOp::JumpIfTrue(label) => emitter.emit_jump_to_label(OdinHeadCode::JumpIfTrue, *label),
        ResidualOp::Jump(label) => emitter.emit_jump_to_label(OdinHeadCode::Jump, *label),
        ResidualOp::Return => emitter.emit_plain(OdinHeadCode::Return),
    }
}

/// 栈顶布尔取反（无 `Not` 头码，用条件跳转构造）。
fn emit_bool_flip(emitter: &mut BytecodeEmitter<'_>) {
    let true_label = emitter.alloc_label();
    let end_label = emitter.alloc_label();
    emitter.emit_jump_to_label(OdinHeadCode::JumpIfTrue, true_label);
    emitter.emit_const_bool(true);
    emitter.emit_jump_to_label(OdinHeadCode::Jump, end_label);
    emitter.emit_label(true_label);
    emitter.emit_const_bool(false);
    emitter.emit_label(end_label);
}
