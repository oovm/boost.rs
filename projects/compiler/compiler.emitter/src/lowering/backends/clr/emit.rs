//! CLR 目标私有 `MSIL` 载体原语与装配服务。
//!
//! 边界（God IR 拆除整改）：本 lane 的计划事实消费与逐操作 `MSIL` 发射已迁到
//! 语言侧 `language.valkyrie` 的 `pipeline/clr/`，直读 `ValkyrieMirFunction` 结算
//! 目标私有输入；本模块只保留目标私有载体原语（类型映射、数组/整数形状、
//! 布局/sum 载体定义、managed ABI 词汇表与 argv 分发入口合成）与装配服务
//! [`assemble_clr_module`]（模块非空自检并封装）。
//!
//! 身份校验语义不变：闭合 `Apply` 只查冻结的 `SemanticType → LayoutId` 表，
//! 缺映射、悬空或身份冲突一律结构化拒绝，禁止按名字回退猜布局。

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ManagedAbiContract, ManagedAbiEntry, Result, SumRepresentation,
    lowering::backends::argv_encoding::{ArgvKind, argv_encoding},
    target_operator::TargetOperandType,
    transitional::msil::{
        MsilField, MsilInstruction, MsilInstructionOperand, MsilMethodBody, MsilMethodRef, MsilMethodSignature, MsilModule, MsilOpcode, MsilType,
        MsilTypeDef,
    },
};
use compiler_types::{AggregateLayout, AggregateLayoutPlan, NominalInstanceId, SemanticType};

/// CLR 栈上标量载体类别：`int32` 覆盖 bool/char/窄整型，比较结果与布尔同槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClrValueKind {
    /// `int32` 载体。
    Int32,
    /// `int64` 载体。
    Int64,
    /// `float32` 载体。
    Float32,
    /// `float64` 载体。
    Float64,
    /// 引用载体（字符串等）。
    Reference,
}

impl ClrValueKind {
    /// 冻结 operator 配方的标量载体 → CLR 栈上类别。
    pub fn of_operand_type(operand_type: TargetOperandType) -> Self {
        match operand_type {
            TargetOperandType::I32 => Self::Int32,
            TargetOperandType::I64 => Self::Int64,
            TargetOperandType::F32 => Self::Float32,
            TargetOperandType::F64 => Self::Float64,
        }
    }

    /// `MSIL` 载体类型 → CLR 栈上类别；`void` 没有值槽。
    pub fn of_msil_type(ty: &MsilType) -> Result<Self> {
        Ok(match ty {
            MsilType::Void => return Err("`void` 不能作为局部变量或操作数载体".to_string()),
            MsilType::Bool
            | MsilType::Char
            | MsilType::Int8 { .. }
            | MsilType::Int16 { .. }
            | MsilType::Int32 { .. }
            | MsilType::IntPtr { .. } => Self::Int32,
            MsilType::Int64 { .. } => Self::Int64,
            MsilType::Float32 => Self::Float32,
            MsilType::Float64 => Self::Float64,
            MsilType::String | MsilType::Object | MsilType::SzArray(_) | MsilType::Named(_) => Self::Reference,
        })
    }
}

/// CLR lowering 结果：`MSIL` 模块与导出入口的 managed ABI 契约。
#[derive(Debug, Clone)]
pub struct ClrLoweredFragment {
    /// 片段 `MSIL` 模块。
    pub module: MsilModule,
    /// 导出入口 ABI 契约；没有可导出入口时为 `None`。
    pub managed_abi: Option<ManagedAbiContract>,
}

/// CLR 目标私有输入：已降低的 `MSIL` 模块与导出入口的 managed ABI 契约。
///
/// 计划事实（可达导出面、方法命名与签名、布局/sum 类型、入口投影桩、managed
/// ABI）由语言侧 `pipeline/clr/` 从 MIR 一次结算；装配服务
/// [`assemble_clr_module`] 只接受这个形状，不接触任何跨目标共享程序载荷。
#[derive(Debug, Clone)]
pub struct ClrProgramInput {
    module: MsilModule,
    managed_abi: Option<ManagedAbiContract>,
}

impl ClrProgramInput {
    /// 从已结算的模块与 ABI 契约构造目标私有输入。
    pub fn new(module: MsilModule, managed_abi: Option<ManagedAbiContract>) -> Self {
        Self { module, managed_abi }
    }

    /// 已降低的 `MSIL` 模块。
    pub fn module(&self) -> &MsilModule {
        &self.module
    }

    /// 导出入口的 managed ABI 契约。
    pub fn managed_abi(&self) -> Option<&ManagedAbiContract> {
        self.managed_abi.as_ref()
    }
}

/// CLR 目标私有输入 → 片段产物：只做模块非空自检并封装。
///
/// 签名里没有跨目标共享程序：`MSIL` 与 PE 容器由后端在同一份目标私有输入上完成。
pub fn assemble_clr_module(input: &ClrProgramInput) -> Result<ClrLoweredFragment> {
    let module = input.module();
    if module.assembly.name.trim().is_empty() {
        return Err("CLR 最小 lowering：目标私有输入的 assembly 名为空，拒绝装配".to_string());
    }
    if module.types.is_empty() && module.global_methods.is_empty() {
        return Err("CLR 最小 lowering：目标私有输入既无类型也无全局方法，拒绝装配空模块".to_string());
    }
    Ok(ClrLoweredFragment { module: input.module().clone(), managed_abi: input.managed_abi().cloned() })
}

/// 单个 argv 分发目标：显式携带 `entries` 下标，禁止依赖并行 push 后的位置假设。
#[derive(Debug, Clone)]
pub struct ManagedDispatchTarget {
    /// 对应 [`ManagedAbiEntry`] 在入口列表中的下标。
    pub entry_index: usize,
    /// 被分发方法的冻结签名。
    pub signature: MsilMethodSignature,
    /// 形参的托管 ABI 逻辑类型名。
    pub parameter_types: Vec<String>,
    /// 返回值的托管 ABI 逻辑类型名。
    pub return_type: String,
}

/// CLI 分发入口名，同时作为 ABI 的 `launcher_class` 标识。
pub const MANAGED_ENTRY_METHOD: &str = "managed_entry";

/// PE 写入器为全局方法合成的类型名。
pub const GLOBAL_METHODS_TYPE: &str = "_GlobalMethods";

/// 宿主 `System.String` 类型名。
pub const SYSTEM_STRING: &str = "[mscorlib]System.String";
/// 宿主 `System.Int64` 类型名。
pub const SYSTEM_INT64: &str = "[mscorlib]System.Int64";
/// 宿主 `System.Int32` 类型名。
pub const SYSTEM_INT32: &str = "[mscorlib]System.Int32";
/// 宿主 `System.UInt64` 类型名。
pub const SYSTEM_UINT64: &str = "[mscorlib]System.UInt64";
/// 宿主 `System.UInt32` 类型名。
pub const SYSTEM_UINT32: &str = "[mscorlib]System.UInt32";
/// 宿主 `System.UInt16` 类型名。
pub const SYSTEM_UINT16: &str = "[mscorlib]System.UInt16";
/// 宿主 `System.Byte` 类型名。
pub const SYSTEM_BYTE: &str = "[mscorlib]System.Byte";
/// 宿主 `System.Single` 类型名。
pub const SYSTEM_SINGLE: &str = "[mscorlib]System.Single";
/// 宿主 `System.Double` 类型名。
pub const SYSTEM_DOUBLE: &str = "[mscorlib]System.Double";
/// 宿主 `System.Boolean` 类型名。
pub const SYSTEM_BOOLEAN: &str = "[mscorlib]System.Boolean";
/// 宿主 `System.Console` 类型名。
pub const SYSTEM_CONSOLE: &str = "[mscorlib]System.Console";

/// 托管 ABI 逻辑类型名：由冻结载体投影出的「入口事实」，供库宿主按名绑定。
///
/// 这里只回答「签名是什么」，与宿主能否经 argv 编组无关；数组、引用、unsigned
/// 都是合法入口事实。缺少载体合同（`Object`/`Named`/`IntPtr` 等）时结构化失败，
/// 不猜名字、不静默回退。
pub fn managed_abi_type_name(ty: &MsilType) -> Result<String> {
    Ok(match ty {
        MsilType::Void => "void",
        MsilType::Bool => "bool",
        // 本 lane 的 `msil_type` 把 V 的 `Character`（u32 码位）固定映射为
        // `Int32 { signed: false }`，实际载体词汇是 `u32`；此分支仅覆盖直接携带
        // 宿主 16 位字符的冻结载体。
        MsilType::Char => "char",
        MsilType::Int8 { signed: true } => "i8",
        MsilType::Int8 { signed: false } => "u8",
        MsilType::Int16 { signed: true } => "i16",
        MsilType::Int16 { signed: false } => "u16",
        MsilType::Int32 { signed: true } => "i32",
        MsilType::Int32 { signed: false } => "u32",
        MsilType::Int64 { signed: true } => "i64",
        MsilType::Int64 { signed: false } => "u64",
        MsilType::Float32 => "f32",
        MsilType::Float64 => "f64",
        MsilType::String => "string",
        MsilType::SzArray(item) => return Ok(format!("{}[]", managed_abi_type_name(item)?)),
        other => {
            return Err(format!(
                "CLR managed ABI：类型 `{other}` 缺少托管载体合同（`Object`/`Named`/`IntPtr` 等未冻结为可绑定逻辑类型），拒绝猜名字或回退"
            ));
        }
    }
    .to_string())
}

/// argv 可表达性：机械消费共享编码合同表。
///
/// 这是「包装能力」判定而非「入口事实」判定：被表拒绝的类型仍是合法库签名，
/// 只是不生成 argv launcher；两个 managed 后端共享同一张表，不各自维护名单。
pub fn managed_argv_expressible(managed_type: &str) -> bool {
    argv_encoding(managed_type).is_ok()
}

/// 合成 `int32(string[])` argv 分发入口：`args[0]` 是逻辑入口名，其余按声明类型解析。
///
/// 只消费过滤后的分发目标，每个目标经显式 `entry_index` 取回入口事实，
/// 因此入口列表与目标列表的过滤不会造成错位。
pub fn build_managed_entry_method(entries: &[ManagedAbiEntry], dispatch_targets: &[ManagedDispatchTarget]) -> Result<MsilMethodBody> {
    let mut instructions = Vec::new();
    for (index, target) in dispatch_targets.iter().enumerate() {
        let entry = entries
            .get(target.entry_index)
            .ok_or_else(|| format!("CLR managed ABI：分发目标下标 {} 越界（入口列表长度 {}）", target.entry_index, entries.len()))?;
        let next_label = format!("managed_entry_next_{index}");
        instructions.push(ldarg0());
        instructions.push(ldc_i4_0());
        instructions.push(opcode_instruction(MsilOpcode::LdelemRef));
        instructions.push(msil_instruction(MsilOpcode::Ldstr, Some(MsilInstructionOperand::StringLiteral(entry.logical_entry.clone()))));
        instructions.push(call_instruction(string_equality_reference()));
        instructions.push(branch_instruction(MsilOpcode::Brfalse, next_label.clone()));

        for (parameter_index, parameter_type) in target.parameter_types.iter().enumerate() {
            instructions.push(ldarg0());
            instructions
                .push(msil_instruction(MsilOpcode::LdcI4, Some(MsilInstructionOperand::Integer(i64::from(parameter_index as u32) + 1))));
            instructions.push(opcode_instruction(MsilOpcode::LdelemRef));
            instructions.push(call_instruction(argument_parse_reference(parameter_type)?));
        }
        instructions.push(call_instruction(MsilMethodRef { owner: None, name: entry.method.clone(), signature: target.signature.clone() }));
        if target.return_type != "void" {
            instructions.push(call_instruction(result_print_reference(&target.return_type)?));
        }
        instructions.push(ldc_i4_0());
        instructions.push(opcode_instruction(MsilOpcode::Ret));
        instructions.push(MsilInstruction { label: Some(next_label), opcode: MsilOpcode::Nop, operand: None });
    }

    instructions.push(msil_instruction(MsilOpcode::Ldstr, Some(MsilInstructionOperand::StringLiteral("unknown entry".to_string()))));
    instructions.push(call_instruction(console_write_line_reference("string")?));
    instructions.push(ldarg0());
    instructions.push(ldc_i4_0());
    instructions.push(opcode_instruction(MsilOpcode::LdelemRef));
    instructions.push(call_instruction(console_write_line_reference("string")?));
    instructions.push(msil_instruction(MsilOpcode::LdcI4, Some(MsilInstructionOperand::Integer(1))));
    instructions.push(opcode_instruction(MsilOpcode::Ret));

    let max_stack = dispatch_targets
        .iter()
        .map(|target| u16::try_from(target.parameter_types.len()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0)
        .saturating_add(2);
    Ok(MsilMethodBody {
        method: MsilMethodRef {
            owner: None,
            name: MANAGED_ENTRY_METHOD.to_string(),
            signature: MsilMethodSignature::new(MsilType::Int32 { signed: true }, vec![MsilType::sz_array(MsilType::String)]),
        },
        locals: Vec::new(),
        instructions,
        max_stack,
        is_entry_point: true,
        is_async: false,
    })
}

/// 构造无标签 `MSIL` 指令。
pub fn msil_instruction(opcode: MsilOpcode, operand: Option<MsilInstructionOperand>) -> MsilInstruction {
    MsilInstruction { label: None, opcode, operand }
}

/// 构造无操作数 `MSIL` 指令。
pub fn opcode_instruction(opcode: MsilOpcode) -> MsilInstruction {
    msil_instruction(opcode, None)
}

/// `ldarg.0` 指令。
pub fn ldarg0() -> MsilInstruction {
    opcode_instruction(MsilOpcode::Ldarg0)
}

/// `ldc.i4.0` 指令。
pub fn ldc_i4_0() -> MsilInstruction {
    opcode_instruction(MsilOpcode::LdcI4_0)
}

/// `call` 指令。
pub fn call_instruction(method: MsilMethodRef) -> MsilInstruction {
    msil_instruction(MsilOpcode::Call, Some(MsilInstructionOperand::Method(method)))
}

/// 分支指令。
pub fn branch_instruction(opcode: MsilOpcode, target: String) -> MsilInstruction {
    msil_instruction(opcode, Some(MsilInstructionOperand::BranchTarget(target)))
}

/// 宿主外部方法引用。
pub fn external_method(owner: &str, name: &str, return_type: MsilType, parameter_types: Vec<MsilType>) -> MsilMethodRef {
    MsilMethodRef { owner: Some(owner.to_string()), name: name.to_string(), signature: MsilMethodSignature::new(return_type, parameter_types) }
}

/// `System.String.op_Equality` 引用。
pub fn string_equality_reference() -> MsilMethodRef {
    external_method(SYSTEM_STRING, "op_Equality", MsilType::Bool, vec![MsilType::String, MsilType::String])
}

/// argv 解析调用：按共享合同表选择宿主 `Parse`。
///
/// 无符号整型必须落到宿主无符号解析器（`Byte.Parse` / `UInt16.Parse` /
/// `UInt32.Parse` / `UInt64.Parse`），范围与语义位宽一致；越界由宿主抛
/// `OverflowException` / `FormatException` 自然传播，不截断、不回绕。
/// `f32` 用 `System.Single.Parse` 得到真实 `float32`，不得升格成 `Double`。
pub fn argument_parse_reference(managed_type: &str) -> Result<MsilMethodRef> {
    let encoding = argv_encoding(managed_type)?;
    let (owner, return_type) = match encoding.kind {
        ArgvKind::Bool => (SYSTEM_BOOLEAN, MsilType::Bool),
        ArgvKind::Signed { bits: 32 } => (SYSTEM_INT32, MsilType::Int32 { signed: true }),
        ArgvKind::Signed { bits: 64 } => (SYSTEM_INT64, MsilType::Int64 { signed: true }),
        ArgvKind::Unsigned { bits: 8 } => (SYSTEM_BYTE, MsilType::Int8 { signed: false }),
        ArgvKind::Unsigned { bits: 16 } => (SYSTEM_UINT16, MsilType::Int16 { signed: false }),
        ArgvKind::Unsigned { bits: 32 } => (SYSTEM_UINT32, MsilType::Int32 { signed: false }),
        ArgvKind::Unsigned { bits: 64 } => (SYSTEM_UINT64, MsilType::Int64 { signed: false }),
        ArgvKind::Float { bits: 32 } => (SYSTEM_SINGLE, MsilType::Float32),
        ArgvKind::Float { bits: 64 } => (SYSTEM_DOUBLE, MsilType::Float64),
        ArgvKind::Void => return Err("CLR managed ABI launcher 形参不能是 `void`".to_string()),
        other => return Err(format!("CLR managed ABI launcher 不支持形参载体 `{managed_type}`（合同类别 {other:?}）")),
    };
    Ok(external_method(owner, "Parse", return_type, vec![MsilType::String]))
}

/// 结果输出调用：按共享合同表选择匹配的 `Console.WriteLine` 重载。
///
/// `u8`/`u16` 的值域落在 `int32` 内，用 `WriteLine(int32)` 无损；`u32`/`u64`
/// 必须是宿主无符号重载，否则 4294967295 会被打成 -1。
pub fn result_print_reference(managed_type: &str) -> Result<MsilMethodRef> {
    let encoding = argv_encoding(managed_type)?;
    let parameter = match encoding.kind {
        ArgvKind::Bool => MsilType::Bool,
        ArgvKind::Signed { bits: 32 } => MsilType::Int32 { signed: true },
        ArgvKind::Signed { bits: 64 } => MsilType::Int64 { signed: true },
        ArgvKind::Unsigned { bits: 8 | 16 } => MsilType::Int32 { signed: true },
        ArgvKind::Unsigned { bits: 32 } => MsilType::Int32 { signed: false },
        ArgvKind::Unsigned { bits: 64 } => MsilType::Int64 { signed: false },
        ArgvKind::Float { bits: 32 } => MsilType::Float32,
        ArgvKind::Float { bits: 64 } => MsilType::Float64,
        ArgvKind::Void => return Err("CLR managed ABI launcher：`void` 结果不得生成输出调用".to_string()),
        other => return Err(format!("CLR managed ABI launcher 不支持返回载体 `{managed_type}`（合同类别 {other:?}）")),
    };
    Ok(console_write_line_type(parameter))
}

/// `Console.WriteLine(string)` 引用。
pub fn console_write_line_reference(managed_type: &str) -> Result<MsilMethodRef> {
    match managed_type {
        "string" => Ok(console_write_line_type(MsilType::String)),
        other => Err(format!("CLR managed ABI launcher 不支持 `Console.WriteLine` 载体 `{other}`")),
    }
}

/// 指定参数载体的 `Console.WriteLine` 重载。
pub fn console_write_line_type(parameter_type: MsilType) -> MsilMethodRef {
    external_method(SYSTEM_CONSOLE, "WriteLine", MsilType::Void, vec![parameter_type])
}

/// 局部 / 形参位置的 CLR 载体：`Unit`/`Bottom` 没有值槽。
pub fn msil_parameter_type(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<MsilType> {
    let msil_type = msil_type(layouts, ty)?;
    if msil_type == MsilType::Void {
        return Err(format!("CLR 最小 lowering：`{ty}` 是 void 形态，不能作为方法形参或局部值"));
    }
    Ok(msil_type)
}

/// `Cast` 的整数表示：位宽 + 符号性（`Character` 是 32 位无符号码位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsilIntShape {
    /// 位宽。
    pub bits: u8,
    /// 是否带符号。
    pub signed: bool,
}

/// 语义类型的整数形状；非整数形态返回 `None`。
pub fn msil_int_shape(ty: &SemanticType) -> Option<MsilIntShape> {
    Some(match ty {
        SemanticType::Integer8 { signed } => MsilIntShape { bits: 8, signed: *signed },
        SemanticType::Integer16 { signed } => MsilIntShape { bits: 16, signed: *signed },
        SemanticType::Integer32 { signed } => MsilIntShape { bits: 32, signed: *signed },
        SemanticType::Integer64 { signed } => MsilIntShape { bits: 64, signed: *signed },
        SemanticType::Character => MsilIntShape { bits: 32, signed: false },
        _ => return None,
    })
}

/// 按位宽/符号性把求值栈上的整数规范化成目标表示。
pub fn msil_int_normalize_opcode(shape: MsilIntShape) -> MsilOpcode {
    match (shape.bits, shape.signed) {
        (8, true) => MsilOpcode::ConvI1,
        (16, true) => MsilOpcode::ConvI2,
        (32, true) => MsilOpcode::ConvI4,
        (64, true) => MsilOpcode::ConvI8,
        (8, false) => MsilOpcode::ConvU1,
        (16, false) => MsilOpcode::ConvU2,
        (32, false) => MsilOpcode::ConvU4,
        _ => MsilOpcode::ConvU8,
    }
}

/// 返回位置的 CLR 载体。
pub fn msil_return_type(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<MsilType> {
    msil_type(layouts, ty)
}

/// 布局类型定义的稳定名字：`compiler_target::layout::<sanitized(name)>$<layout id>`。
///
/// CLR 名字用 `::` 层级；块 4 生成 `MsilTypeDef` 时复用同一函数，绝不另写第二套规则。
/// 同一 layout id 稳定、不同布局不撞名。
pub fn clr_layout_type_name(layout: &AggregateLayout) -> String {
    format!("compiler_target::layout::{}${}", crate::lowering::sanitize_symbol(&layout.name), layout.id)
}

/// 布局字段在生成类型中的唯一名字；构造与字段访问必须共用这一条规则。
pub fn clr_layout_field_name(slot: usize) -> String {
    format!("field{slot}")
}

/// sum 生成类型的稳定名字：只由 nominal 身份派生，禁止按 variant 名/短名推断。
pub fn clr_sum_type_name(nominal: NominalInstanceId) -> String {
    format!("compiler_target::sum::${}", nominal.index())
}

/// sum 生成类型的 tag 字段名。
pub const CLR_SUM_TAG_FIELD: &str = "tag";
/// sum 生成类型的 payload 字段名。
pub const CLR_SUM_PAYLOAD_FIELD: &str = "payload";
/// 宿主 `System.Object` 类型名。
pub const SYSTEM_OBJECT: &str = "[mscorlib]System.Object";

/// 生成类型的实例默认构造器：`ldarg.0; call instance void [mscorlib]System.Object::.ctor(); ret`。
///
/// 构造函数体是 PE 写入器唯一要求的实例初始化路径；没有它，生成的类型无法加载。
pub fn clr_instance_constructor(type_name: &str) -> MsilMethodBody {
    MsilMethodBody {
        method: MsilMethodRef {
            owner: Some(type_name.to_string()),
            name: ".ctor".to_string(),
            signature: MsilMethodSignature::new_instance(MsilType::Void, Vec::new()),
        },
        locals: Vec::new(),
        instructions: vec![
            ldarg0(),
            call_instruction(MsilMethodRef {
                owner: Some(SYSTEM_OBJECT.to_string()),
                name: ".ctor".to_string(),
                signature: MsilMethodSignature::new_instance(MsilType::Void, Vec::new()),
            }),
            opcode_instruction(MsilOpcode::Ret),
        ],
        max_stack: 1,
        is_entry_point: false,
        is_async: false,
    }
}

/// 为所有需要堆载体的布局生成 `MsilTypeDef`。
///
/// 规则与 `msil_type` 的载体分类一致：引用存储或非零字段布局才需要类型定义；
/// 零字段值存储布局继续在载体层结构化拒绝。
pub fn build_layout_types(layouts: &AggregateLayoutPlan) -> Result<Vec<MsilTypeDef>> {
    // 第一遍：先为全部可达布局登记目标类型身份（类型名只由布局身份派生）。
    // 递归引用因此总能命中已注册的类型定义，不依赖布局遍历顺序。
    let mut types = Vec::new();
    let mut type_index_by_layout = BTreeMap::new();
    for layout in &layouts.layouts {
        if !(layout.storage.is_reference() || !layout.fields.is_empty()) {
            continue;
        }
        let type_name = clr_layout_type_name(layout);
        type_index_by_layout.insert(layout.id, types.len());
        types.push(MsilTypeDef {
            methods: vec![clr_instance_constructor(&type_name)],
            full_name: type_name,
            namespace: String::new(),
            fields: Vec::new(),
            is_value_type: false,
        });
    }
    // 第二遍：在同一份冻结计划上解析字段与引用（含闭合 `Apply` 的布局映射）。
    for layout in &layouts.layouts {
        let Some(&type_index) = type_index_by_layout.get(&layout.id)
        else {
            continue;
        };
        let mut fields = Vec::with_capacity(layout.fields.len());
        for (slot, field) in layout.fields.iter().enumerate() {
            fields.push(MsilField { name: clr_layout_field_name(slot), ty: msil_type(layouts, &field.ty)?, is_static: false });
        }
        types[type_index].fields = fields;
    }
    Ok(types)
}

/// 只为 MIR 中实际出现 sum 指令的 nominal 生成 tag/payload 类型定义。
///
/// nominal 清单由语言侧扫描可达 MIR 函数结算；缺冻结 `SumRepresentation`
/// 的 nominal 在这里失败闭合，不补造 tag/payload 形状。
pub fn build_sum_types(nominals: &BTreeSet<NominalInstanceId>, sum_representations: &BTreeMap<NominalInstanceId, SumRepresentation>) -> Result<Vec<MsilTypeDef>> {
    let mut types = Vec::with_capacity(nominals.len());
    for nominal in nominals {
        let Some(sum) = sum_representations.get(nominal)
        else {
            return Err(format!("CLR 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`"));
        };
        let _ = sum;
        let type_name = clr_sum_type_name(*nominal);
        types.push(MsilTypeDef {
            methods: vec![clr_instance_constructor(&type_name)],
            full_name: type_name,
            namespace: String::new(),
            fields: vec![
                MsilField { name: CLR_SUM_TAG_FIELD.to_string(), ty: MsilType::Int32 { signed: true }, is_static: false },
                MsilField { name: CLR_SUM_PAYLOAD_FIELD.to_string(), ty: MsilType::Object, is_static: false },
            ],
            is_value_type: false,
        });
    }
    Ok(types)
}

/// 生成类型全名必须模块内唯一，否则字段/构造 token 会静默漂移。
pub fn reject_duplicate_type_names(types: &[MsilTypeDef]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for type_def in types {
        if !seen.insert(type_def.qualified_name()) {
            return Err(format!("CLR 最小 lowering：生成类型全名 `{}` 重复，拒绝输出歧义模块", type_def.qualified_name()));
        }
    }
    Ok(())
}

/// 数组类型 `Array<T>` / `FixedArray { element, .. }` 的元素类型。
pub fn clr_array_element_type(array_type: &SemanticType) -> Result<&SemanticType> {
    match array_type {
        SemanticType::Array(element) => Ok(element),
        SemanticType::FixedArray { element, .. } => Ok(element),
        other => Err(format!("CLR 最小 lowering：`{other}` 不是数组形态，缺少元素类型合同")),
    }
}

/// `Int32` 元素走专用指令，引用载体走 `ldelem.ref`，其余统一 `ldelem.any + Type`。
pub fn clr_array_load_opcode(element: &MsilType) -> MsilOpcode {
    match element {
        MsilType::Int32 { .. } => MsilOpcode::LdelemI4,
        MsilType::String | MsilType::Object | MsilType::Named(_) | MsilType::SzArray(_) => MsilOpcode::LdelemRef,
        _ => MsilOpcode::LdelemAny,
    }
}

/// 与 [`clr_array_load_opcode`] 对称的元素存储 opcode。
pub fn clr_array_store_opcode(element: &MsilType) -> MsilOpcode {
    match element {
        MsilType::Int32 { .. } => MsilOpcode::StelemI4,
        MsilType::String | MsilType::Object | MsilType::Named(_) | MsilType::SzArray(_) => MsilOpcode::StelemRef,
        _ => MsilOpcode::StelemAny,
    }
}

/// 数组元素位置的 CLR 载体。
///
/// data-binary-pe 没有 `ldelem.i8`/`stelem.i8`，i64 元素由块 4 用
/// `ldelem.any`/`stelem.any` + `Type("int64")` 存取；载体本身仍是 `Int64`。
pub fn msil_array_element_type(layouts: &AggregateLayoutPlan, element: &SemanticType) -> Result<MsilType> {
    Ok(match element {
        SemanticType::Boolean => MsilType::Bool,
        SemanticType::Integer8 { signed } => MsilType::Int8 { signed: *signed },
        SemanticType::Integer16 { signed } => MsilType::Int16 { signed: *signed },
        SemanticType::Integer32 { signed } => MsilType::Int32 { signed: *signed },
        // V 的 `char` 是 u32 码位载体；CLR `Char` 只有 16 位，物理载体固定 32 位 int32。
        SemanticType::Character => MsilType::Int32 { signed: false },
        SemanticType::Integer64 { signed } => MsilType::Int64 { signed: *signed },
        SemanticType::Float32 => MsilType::Float32,
        SemanticType::Float64 => MsilType::Float64,
        SemanticType::Utf8 | SemanticType::Utf16 => MsilType::String,
        // 闭合泛型实例的布局身份只来自计划冻结的 `SemanticType → LayoutId` 索引；
        // 缺映射或映射悬空都回到计划生产者修，开放泛型仍在这里失败。
        SemanticType::Apply(..) => {
            let Some(layout) = layouts.frozen_layout(element) else {
                return Err(format!(
                    "CLR 最小 lowering：泛型实例 `{element}` 缺少冻结布局映射：计划未把该闭合实例登记到 `SemanticType → LayoutId`（开放泛型不得进入发射）"
                ));
            };
            if layout.storage.is_reference() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持值存储泛型实例 `{element}` 作为数组元素：缺少单槽 CLR 载体合同"));
            }
        }
        // 名义布局按冻结身份取类型定义名；值存储聚合没有单槽 CLR 载体。
        SemanticType::Nominal { layout, spelling } => {
            let Some(layout) = layouts.layouts.iter().find(|candidate| candidate.id == *layout)
            else {
                return Err(format!("CLR 最小 lowering：`{spelling}` 的布局身份 `{layout}` 未在计划中注册：缺少数组元素 CLR 载体合同"));
            };
            if layout.storage.is_reference() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持值存储聚合 `{spelling}` 作为数组元素：缺少单槽 CLR 载体合同"));
            }
        }
        SemanticType::Array(inner) | SemanticType::FixedArray { element: inner, .. } => {
            MsilType::sz_array(msil_array_element_type(layouts, inner)?)
        }
        // 可空元素只能复用引用载体（`null` 表示空）；标量需要 sum 表示。
        SemanticType::Nullable(inner) => {
            let ty = msil_array_element_type(layouts, inner)?;
            if matches!(ty, MsilType::String | MsilType::Object | MsilType::Named(_) | MsilType::SzArray(_)) {
                ty
            }
            else {
                return Err(format!("CLR 最小 lowering：可空标量 `{element}` 需要 sum 表示，不能擦成引用"));
            }
        }
        other => {
            return Err(format!("CLR 最小 lowering 不支持数组元素类型 `{other}`：缺少该形状的 CLR 载体合同"));
        }
    })
}

/// 类型的 CLR 载体：标量取原生类型，数组取 `SzArray`，引用布局取类型定义名。
pub fn msil_type(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<MsilType> {
    Ok(match ty {
        SemanticType::Bottom | SemanticType::Unit => MsilType::Void,
        SemanticType::Boolean => MsilType::Bool,
        // V 的 `char` 是 u32 码位，载体固定 32 位；不得用宿主 16 位 `Char`。
        SemanticType::Character => MsilType::Int32 { signed: false },
        SemanticType::Integer8 { signed } => MsilType::Int8 { signed: *signed },
        SemanticType::Integer16 { signed } => MsilType::Int16 { signed: *signed },
        SemanticType::Integer32 { signed } => MsilType::Int32 { signed: *signed },
        SemanticType::Integer64 { signed } => MsilType::Int64 { signed: *signed },
        SemanticType::Float32 => MsilType::Float32,
        SemanticType::Float64 => MsilType::Float64,
        SemanticType::Utf8 | SemanticType::Utf16 => MsilType::String,
        // 栈/堆数组的载体是元素载体数组；`[T; N]` 的长度由 `ArrayFromElements` 表达。
        SemanticType::Array(element) => MsilType::sz_array(msil_array_element_type(layouts, element)?),
        SemanticType::FixedArray { element, .. } => MsilType::sz_array(msil_array_element_type(layouts, element)?),
        // 布局身份只经 `TupleNew`/`AggregateCopy` 的冻结布局事实进入后端，类型描述符
        // 没有该事实，禁止按形状猜布局。
        SemanticType::Tuple(..) => {
            return Err(format!(
                "CLR 最小 lowering：裸 tuple `{ty}` 没有布局身份：布局身份只经 `TupleNew`/`AggregateCopy` 的冻结布局事实进入后端，类型描述符没有该事实"
            ));
        }
        // 可空引用直接复用引用载体（`null` 表示空）；标量需要 sum 表示，不得静默装箱。
        SemanticType::Nullable(inner) => {
            let ty = msil_type(layouts, inner)?;
            if matches!(ty, MsilType::String | MsilType::Object | MsilType::Named(_) | MsilType::SzArray(_)) {
                ty
            }
            else if ty == MsilType::Void {
                return Err(format!("CLR 最小 lowering：可空 `{ty}` 的内层是 void，没有可复用的引用载体"));
            }
            else {
                return Err(format!("CLR 最小 lowering：可空标量 `{ty}` 需要 sum 表示，不得静默装箱"));
            }
        }
        // 闭合泛型实例的布局身份只来自计划冻结的 `SemanticType → LayoutId` 索引。
        // 与 `Nominal` 分支同一存储规则；缺映射、悬空或身份冲突回计划生产者修。
        SemanticType::Apply(..) => {
            let Some(layout) = layouts.frozen_layout(ty) else {
                return Err(format!(
                    "CLR 最小 lowering：泛型实例 `{ty}` 缺少冻结布局映射：计划未把该闭合实例登记到 `SemanticType → LayoutId`（开放泛型不得进入发射）"
                ));
            };
            if layout.storage.is_reference() || !layout.fields.is_empty() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持零字段值存储泛型实例 `{ty}`：缺少单槽 CLR 载体合同"));
            }
        }
        // 携带身份的名义类型：存储类按 `layout` 身份解析，引用/值存储都指向
        // 布局类型定义。CLR 没有内联结构体，逐字段拷贝由 `AggregateCopy` 承担，
        // 它是唯一允许的复制路径；零字段值布局与 wasm 标量语义一致，维持结构化失败。
        SemanticType::Nominal { layout, spelling } => {
            let Some(layout) = layouts.layouts.iter().find(|candidate| candidate.id == *layout)
            else {
                return Err(format!("CLR 最小 lowering：`{spelling}` 的布局身份 `{layout}` 未在计划中注册：缺少该形状的 CLR 载体合同"));
            };
            if layout.storage.is_reference() || !layout.fields.is_empty() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持零字段值存储聚合 `{spelling}`：缺少单槽 CLR 载体合同"));
            }
        }
        other => {
            return Err(format!("CLR 最小 lowering 不支持类型 `{other}`：缺少该形状的 CLR 载体合同"));
        }
    })
}

#[cfg(test)]
mod apply_layout_tests {
    use super::*;
    use compiler_target::Identifier;
    use compiler_types::{FieldLayout, StorageKind};

    fn closed_apply() -> SemanticType {
        SemanticType::Apply(
            Box::new(SemanticType::Named(Identifier::new("std::collection::ArrayList"))),
            vec![SemanticType::Integer64 { signed: true }],
        )
    }

    fn plan_with_closed_apply(register: bool) -> AggregateLayoutPlan {
        let apply = closed_apply();
        let outer = AggregateLayout {
            id: 0,
            name: "Holder".to_string(),
            namespace: String::new(),
            key: None,
            storage: StorageKind::Reference,
            fields: vec![FieldLayout { name: "items".to_string(), ty: apply.clone() }],
        };
        let inner = AggregateLayout {
            id: 1,
            name: "ArrayList".to_string(),
            namespace: String::new(),
            key: None,
            storage: StorageKind::Reference,
            fields: vec![FieldLayout { name: "len".to_string(), ty: SemanticType::Integer64 { signed: true } }],
        };
        let mut plan = AggregateLayoutPlan { layouts: vec![outer, inner], ..Default::default() };
        if register {
            plan.layout_by_semantic_type.insert(apply, 1);
        }
        plan
    }

    #[test]
    fn closed_apply_resolves_to_frozen_type_definition() {
        let plan = plan_with_closed_apply(true);
        let ty = msil_type(&plan, &closed_apply()).expect("闭合 Apply 必须由冻结索引结算");
        assert_eq!(ty, MsilType::Named(clr_layout_type_name(&plan.layouts[1])));
    }

    #[test]
    fn layout_types_register_identity_before_resolving_fields() {
        let plan = plan_with_closed_apply(true);
        let types = build_layout_types(&plan).expect("字段引用必须命中已登记类型定义");
        assert_eq!(types.len(), 2);
        assert_eq!(types[0].fields[0].ty, MsilType::Named(clr_layout_type_name(&plan.layouts[1])));
    }

    #[test]
    fn missing_apply_mapping_fails_closed() {
        let plan = plan_with_closed_apply(false);
        let error = msil_type(&plan, &closed_apply()).expect_err("缺少映射必须失败");
        assert!(error.contains("缺少冻结布局映射"), "got: {error}");
    }

    #[test]
    fn open_generic_apply_fails_closed() {
        let open = SemanticType::Apply(
            Box::new(SemanticType::Named(Identifier::new("ArrayList"))),
            vec![SemanticType::Named(Identifier::new("T"))],
        );
        let error = msil_type(&AggregateLayoutPlan::default(), &open).expect_err("开放泛型不得发射");
        assert!(error.contains("缺少冻结布局映射"), "got: {error}");
    }
}

#[cfg(test)]
mod managed_abi_vocabulary_tests {
    use super::*;

    #[test]
    fn abi_vocabulary_keeps_frozen_carrier_names() {
        let array = MsilType::sz_array(MsilType::Int64 { signed: true });
        assert_eq!(managed_abi_type_name(&array).expect("数组是合法 ABI 事实"), "i64[]");
        assert_eq!(managed_abi_type_name(&MsilType::String).expect("string 载体"), "string");
        assert_eq!(managed_abi_type_name(&MsilType::Int32 { signed: false }).expect("无符号 int32 载体"), "u32");
        assert_eq!(managed_abi_type_name(&MsilType::Float32).expect("float32 载体"), "f32");
    }

    #[test]
    fn abi_vocabulary_rejects_carriers_without_contract() {
        for ty in [MsilType::Object, MsilType::Named("Some.Type".to_string()), MsilType::IntPtr { signed: true }] {
            let error = managed_abi_type_name(&ty).expect_err("缺少托管载体合同必须结构化失败");
            assert!(error.contains("缺少托管载体合同"), "got: {error}");
        }
    }

    #[test]
    fn argv_subset_rejects_non_argv_carriers() {
        let array = managed_abi_type_name(&MsilType::sz_array(MsilType::Int64 { signed: true })).expect("数组 ABI 事实");
        assert!(!managed_argv_expressible(&array));
        assert!(!managed_argv_expressible("string"));
        assert!(!managed_argv_expressible("char"));
        assert!(!managed_argv_expressible("i8"));
        assert!(!managed_argv_expressible("i16"));
    }

    #[test]
    fn argv_subset_admits_lossless_scalars() {
        for ty in ["void", "bool", "i32", "i64", "f32", "f64", "u8", "u16", "u32", "u64"] {
            assert!(managed_argv_expressible(ty), "`{ty}` 应可 argv 表达");
        }
    }

    /// f32/u8/u64 分发入口必须调用真实宿主解析器与无损输出重载。
    #[test]
    fn entry_method_uses_shared_contract_host_parsers() {
        let signature = MsilMethodSignature::new(
            MsilType::Float32,
            vec![MsilType::Float32, MsilType::Int8 { signed: false }, MsilType::Int64 { signed: false }],
        );
        let entry = ManagedAbiEntry {
            logical_entry: "score".to_string(),
            source_name: "score".to_string(),
            owner: GLOBAL_METHODS_TYPE.to_string(),
            method: "score".to_string(),
            signature: signature.to_string(),
            parameter_types: vec!["f32".to_string(), "u8".to_string(), "u64".to_string()],
            parameter_names: vec!["x".to_string(), "y".to_string(), "z".to_string()],
            return_type: "f32".to_string(),
            launcher_class: Some(MANAGED_ENTRY_METHOD.to_string()),
        };
        let target = ManagedDispatchTarget {
            entry_index: 0,
            signature,
            parameter_types: entry.parameter_types.clone(),
            return_type: entry.return_type.clone(),
        };
        let body = build_managed_entry_method(&[entry], &[target]).expect("f32/u8/u64 入口必须生成分发调用");
        let calls = body
            .instructions
            .iter()
            .filter_map(|instruction| match &instruction.operand {
                Some(MsilInstructionOperand::Method(reference)) => Some(reference),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            calls.iter().any(|reference| reference.owner.as_deref() == Some(SYSTEM_SINGLE)
                && reference.name == "Parse"
                && reference.signature.return_type == MsilType::Float32),
            "f32 必须走 `System.Single.Parse`: {calls:?}"
        );
        assert!(
            calls.iter().any(|reference| reference.owner.as_deref() == Some(SYSTEM_BYTE)
                && reference.name == "Parse"
                && reference.signature.return_type == MsilType::Int8 { signed: false }),
            "u8 必须走宿主无符号 `System.Byte.Parse`: {calls:?}"
        );
        assert!(
            calls.iter().any(|reference| reference.owner.as_deref() == Some(SYSTEM_UINT64)
                && reference.name == "Parse"
                && reference.signature.return_type == MsilType::Int64 { signed: false }),
            "u64 必须走 `System.UInt64.Parse`: {calls:?}"
        );
        assert!(
            calls.iter().any(|reference| reference.owner.as_deref() == Some(SYSTEM_CONSOLE)
                && reference.name == "WriteLine"
                && reference.signature.parameter_types == vec![MsilType::Float32]),
            "f32 返回值必须用 `WriteLine(float32)`: {calls:?}"
        );
    }
}
