//! x86-64 正式 lane 降低：计划事实 → 文本 LIR → 唯一程序装配器 → 容器。
//!
//! 本文件是 **x86-64 ISA 后端**，由 [`super::lower_fragment_to_machine_image`] 按
//! 规范目标四元组分派进入；这里不判断「native」形态，只实现四元组确定的编码、
//! 调用约定与容器：
//!
//! - `x86_64-pc-windows-msvc`：Win64 调用约定（`rcx/rdx/r8/r9` + 32 字节 shadow
//!   space）与 `PE32+`；
//! - `x86_64-unknown-linux-gnu`（及 `linux` + `systemv`）：SysV64 调用约定
//!   （`rdi/rsi/rdx/rcx/r8/r9`、无 shadow space）与 `ELF64 ET_EXEC`，入口桩以
//!   `exit(2)` 结束进程。
//!
//! exe、共享库与宿主扩展复用同一条执行链：降低器把冻结的计划事实转成
//! [`X64ProgramFunction`]（文本 LIR），[`super::x64_program`] 的唯一程序
//! 装配器负责帧序言、符号布局、`rel32` 回填与导入/数据装配，字节图元只在
//! [`super::x64_lir`] 一处实现。本文件不再维护第二套函数级编码器。
//!
//! 只消费计划已冻结的事实：整数/布尔常量、SSA 局部槽、冻结 x86-64 operator、
//! 块标签与条件分支、同片段内部调用以及 `Return`。局部值统一占 8 字节栈槽
//! （`[rbp - 8 * (slot + 1)]`）；块参数与实参周转使用局部槽之后的临时帧槽。
//! 未冻结或超出边界的形状一律结构化失败。

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    FragmentSubmission, Result,
    backend_plan_views::{
        ExecutableBlock as MirBlock, ExecutableBlockRef as MirBlockRef, ExecutableFunction as MirFunction,
        ExecutableInstruction as MirInstruction, ExecutableValueRef as MirValueRef, SemanticType,
    },
    contracts::{ArrayInitialization, Constant, InstructionKind, Operand, Terminator},
    emitter_backend_machine::{MachineImageKind, classify_machine_image},
    lowering::shared::executable::{collect_reachable_blocks, slots::ExecutableSlotPlan},
    lowering::shared::physical_layout::{LayoutPolicy, ResolvedLayoutPlan, resolve_physical_layouts},
    target_operator::{TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe},
    transitional::binary::pe::{NativePeImage, NativePeWriter},
};
use compiler_target::abstractions::BinaryArch;
use compiler_types::{ItemInstanceId, StorageKind};

use data_binary_pe::elf::{NativeElfImage, NativeElfWriter};

use super::{
    X64CallConv, X64FloatCast, X64LoweredProgram, X64Platform, X64ProgramFunction, X64TextInstruction, array_runtime,
    array_runtime::{ArrayAllocationSlots, ArrayAllocator}, encode_x64_program, x64_lir,
};

/// 已冻结的 x86-64 operator 编码。
///
/// 编码按 **ISA** 冻结，不按「native」命名：`native` / `node` / `python` 只是同一
/// 四元组上的产物与宿主形态。算术/位/移位码要求 `operand_type = I64` 且
/// `result_type = I64`；比较码要求 `operand_type = I64` 且 `result_type = I32`
/// （`bool` 载体用 `I32`，比较结果按 `setcc` + `movzx` 归一为 0/1）。任何载体不符
/// 都由 backend 结构化拒绝，不做隐式截窄或符号扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86_64OperatorCode {
    /// `add rax, rcx`：64 位整数加法。
    I64Add,
    /// `sub rax, rcx`：64 位整数减法。
    I64Subtract,
    /// `imul rax, rcx`：64 位整数乘法。
    I64Multiply,
    /// `neg rax`：64 位整数取负（一元）。
    I64Negate,
    /// `and rax, rcx`：64 位按位与。
    I64BitAnd,
    /// `or rax, rcx`：64 位按位或。
    I64BitOr,
    /// `xor rax, rcx`：64 位按位异或。
    I64BitXor,
    /// `shl rax, cl`：64 位左移（位移量取 `cl`，x86 按 6 位取模）。
    I64ShiftLeft,
    /// `sar rax, cl`：64 位算术右移（有符号）。
    I64ShiftRight,
    /// `cmp` + `sete`：64 位相等比较，结果 0/1。
    I64CompareEqual,
    /// `cmp` + `setne`：64 位不等比较，结果 0/1。
    I64CompareNotEqual,
    /// `cmp` + `setl`：64 位有符号小于比较，结果 0/1。
    I64CompareLessThan,
    /// `cmp` + `setle`：64 位有符号小于等于比较，结果 0/1。
    I64CompareLessOrEqual,
    /// `cmp` + `setg`：64 位有符号大于比较，结果 0/1。
    I64CompareGreaterThan,
    /// `cmp` + `setge`：64 位有符号大于等于比较，结果 0/1。
    I64CompareGreaterOrEqual,
    /// `add eax, ecx`：32 位整数加法（结果零扩展到 64 位槽）。
    I32Add,
    /// `sub eax, ecx`：32 位整数减法。
    I32Subtract,
    /// `imul eax, ecx`：32 位整数乘法。
    I32Multiply,
    /// `neg eax`：32 位整数取负（一元）。
    I32Negate,
    /// `and eax, ecx`：32 位按位与。
    I32BitAnd,
    /// `or eax, ecx`：32 位按位或。
    I32BitOr,
    /// `xor eax, ecx`：32 位按位异或。
    I32BitXor,
    /// `shl eax, cl`：32 位左移（位移量按 5 位取模）。
    I32ShiftLeft,
    /// `sar eax, cl`：32 位算术右移（有符号）。
    I32ShiftRight,
    /// `cmp` + `sete`：32 位相等比较，结果 0/1。
    I32CompareEqual,
    /// `cmp` + `setne`：32 位不等比较，结果 0/1。
    I32CompareNotEqual,
    /// `cmp` + `setl`：32 位有符号小于比较，结果 0/1。
    I32CompareLessThan,
    /// `cmp` + `setle`：32 位有符号小于等于比较，结果 0/1。
    I32CompareLessOrEqual,
    /// `cmp` + `setg`：32 位有符号大于比较，结果 0/1。
    I32CompareGreaterThan,
    /// `cmp` + `setge`：32 位有符号大于等于比较，结果 0/1。
    I32CompareGreaterOrEqual,
    /// `cqo` + `idiv rcx`：64 位有符号除法（商留在 `rax`）。
    I64Divide,
    /// `cqo` + `idiv rcx` + `mov rax, rdx`：64 位有符号取余。
    I64Remainder,
    /// `cdq` + `idiv ecx`：32 位有符号除法（商零扩展到 64 位槽）。
    I32Divide,
    /// `cdq` + `idiv ecx` + `mov eax, edx`：32 位有符号取余。
    I32Remainder,
    /// `xor edx, edx` + `div rcx`：64 位无符号除法。
    I64DivideUnsigned,
    /// `xor edx, edx` + `div rcx` + `mov rax, rdx`：64 位无符号取余。
    I64RemainderUnsigned,
    /// `xor edx, edx` + `div ecx`：32 位无符号除法。
    I32DivideUnsigned,
    /// `xor edx, edx` + `div ecx` + `mov eax, edx`：32 位无符号取余。
    I32RemainderUnsigned,
    /// `shr rax, cl`：64 位逻辑右移（无符号）。
    I64ShiftRightUnsigned,
    /// `shr eax, cl`：32 位逻辑右移（无符号）。
    I32ShiftRightUnsigned,
    /// `cmp` + `setb`：64 位无符号小于比较，结果 0/1。
    I64CompareLessThanUnsigned,
    /// `cmp` + `setb`：32 位无符号小于比较，结果 0/1。
    I32CompareLessThanUnsigned,
    /// `addsd xmm0, xmm1`：64 位浮点加法。
    F64Add,
    /// `subsd xmm0, xmm1`：64 位浮点减法。
    F64Subtract,
    /// `mulsd xmm0, xmm1`：64 位浮点乘法。
    F64Multiply,
    /// `divsd xmm0, xmm1`：64 位浮点除法。
    F64Divide,
    /// 64 位浮点取负：翻转符号位，保留 `-0.0` 与 NaN 载荷。
    F64Negate,
    /// `ucomisd` + 组合条件：64 位浮点相等（无序为假）。
    F64CompareEqual,
    /// `ucomisd` + 组合条件：64 位浮点不等（无序为真）。
    F64CompareNotEqual,
    /// `ucomisd` + 组合条件：64 位浮点小于（无序为假）。
    F64CompareLessThan,
    /// `ucomisd` + 组合条件：64 位浮点小于等于（无序为假）。
    F64CompareLessOrEqual,
    /// `ucomisd` + 组合条件：64 位浮点大于（无序为假）。
    F64CompareGreaterThan,
    /// `ucomisd` + 组合条件：64 位浮点大于等于（无序为假）。
    F64CompareGreaterOrEqual,
    /// `addss xmm0, xmm1`：32 位浮点加法。
    F32Add,
    /// `subss xmm0, xmm1`：32 位浮点减法。
    F32Subtract,
    /// `mulss xmm0, xmm1`：32 位浮点乘法。
    F32Multiply,
    /// `divss xmm0, xmm1`：32 位浮点除法。
    F32Divide,
    /// 32 位浮点取负：翻转符号位，保留 `-0.0` 与 NaN 载荷。
    F32Negate,
    /// `ucomiss` + 组合条件：32 位浮点相等（无序为假）。
    F32CompareEqual,
    /// `ucomiss` + 组合条件：32 位浮点不等（无序为真）。
    F32CompareNotEqual,
    /// `ucomiss` + 组合条件：32 位浮点小于（无序为假）。
    F32CompareLessThan,
    /// `ucomiss` + 组合条件：32 位浮点小于等于（无序为假）。
    F32CompareLessOrEqual,
    /// `ucomiss` + 组合条件：32 位浮点大于（无序为假）。
    F32CompareGreaterThan,
    /// `ucomiss` + 组合条件：32 位浮点大于等于（无序为假）。
    F32CompareGreaterOrEqual,
}

impl X86_64OperatorCode {
    /// 该编码是否为比较码（结果为 0/1，载体 `I32`）。
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::I64CompareEqual
                | Self::I64CompareNotEqual
                | Self::I64CompareLessThan
                | Self::I64CompareLessOrEqual
                | Self::I64CompareGreaterThan
                | Self::I64CompareGreaterOrEqual
                | Self::I32CompareEqual
                | Self::I32CompareNotEqual
                | Self::I32CompareLessThan
                | Self::I32CompareLessOrEqual
                | Self::I32CompareGreaterThan
                | Self::I32CompareGreaterOrEqual
                | Self::I64CompareLessThanUnsigned
                | Self::I32CompareLessThanUnsigned
                | Self::F64CompareEqual
                | Self::F64CompareNotEqual
                | Self::F64CompareLessThan
                | Self::F64CompareLessOrEqual
                | Self::F64CompareGreaterThan
                | Self::F64CompareGreaterOrEqual
                | Self::F32CompareEqual
                | Self::F32CompareNotEqual
                | Self::F32CompareLessThan
                | Self::F32CompareLessOrEqual
                | Self::F32CompareGreaterThan
                | Self::F32CompareGreaterOrEqual
        )
    }

    /// 该编码是否为浮点编码（操作数在 `xmm0`/`xmm1`）。
    pub fn is_float(self) -> bool {
        matches!(
            self,
            Self::F64Add
                | Self::F64Subtract
                | Self::F64Multiply
                | Self::F64Divide
                | Self::F64Negate
                | Self::F64CompareEqual
                | Self::F64CompareNotEqual
                | Self::F64CompareLessThan
                | Self::F64CompareLessOrEqual
                | Self::F64CompareGreaterThan
                | Self::F64CompareGreaterOrEqual
                | Self::F32Add
                | Self::F32Subtract
                | Self::F32Multiply
                | Self::F32Divide
                | Self::F32Negate
                | Self::F32CompareEqual
                | Self::F32CompareNotEqual
                | Self::F32CompareLessThan
                | Self::F32CompareLessOrEqual
                | Self::F32CompareGreaterThan
                | Self::F32CompareGreaterOrEqual
        )
    }

    /// 该编码是否为单精度浮点（`f32`，`movss`/`addss` 族）。
    pub fn is_single_precision(self) -> bool {
        matches!(
            self,
            Self::F32Add
                | Self::F32Subtract
                | Self::F32Multiply
                | Self::F32Divide
                | Self::F32Negate
                | Self::F32CompareEqual
                | Self::F32CompareNotEqual
                | Self::F32CompareLessThan
                | Self::F32CompareLessOrEqual
                | Self::F32CompareGreaterThan
                | Self::F32CompareGreaterOrEqual
        )
    }

    /// 该编码的操作数载体。
    pub fn operand_type(self) -> TargetOperandType {
        match self {
            Self::F64Add
            | Self::F64Subtract
            | Self::F64Multiply
            | Self::F64Divide
            | Self::F64Negate
            | Self::F64CompareEqual
            | Self::F64CompareNotEqual
            | Self::F64CompareLessThan
            | Self::F64CompareLessOrEqual
            | Self::F64CompareGreaterThan
            | Self::F64CompareGreaterOrEqual => TargetOperandType::F64,
            Self::F32Add
            | Self::F32Subtract
            | Self::F32Multiply
            | Self::F32Divide
            | Self::F32Negate
            | Self::F32CompareEqual
            | Self::F32CompareNotEqual
            | Self::F32CompareLessThan
            | Self::F32CompareLessOrEqual
            | Self::F32CompareGreaterThan
            | Self::F32CompareGreaterOrEqual => TargetOperandType::F32,
            Self::I32Add
            | Self::I32Subtract
            | Self::I32Multiply
            | Self::I32Negate
            | Self::I32BitAnd
            | Self::I32BitOr
            | Self::I32BitXor
            | Self::I32ShiftLeft
            | Self::I32ShiftRight
            | Self::I32ShiftRightUnsigned
            | Self::I32Divide
            | Self::I32Remainder
            | Self::I32DivideUnsigned
            | Self::I32RemainderUnsigned
            | Self::I32CompareEqual
            | Self::I32CompareNotEqual
            | Self::I32CompareLessThan
            | Self::I32CompareLessOrEqual
            | Self::I32CompareGreaterThan
            | Self::I32CompareGreaterOrEqual
            | Self::I32CompareLessThanUnsigned => TargetOperandType::I32,
            _ => TargetOperandType::I64,
        }
    }

    /// 该编码的结果载体：比较码固定 `I32`（bool），其余与操作数同宽。
    pub fn result_type(self) -> TargetOperandType {
        if self.is_comparison() { TargetOperandType::I32 } else { self.operand_type() }
    }

    /// 该编码冻结的实参个数（只有取负是一元）。
    pub fn arity(self) -> usize {
        match self {
            Self::I64Negate | Self::I32Negate | Self::F64Negate | Self::F32Negate => 1,
            _ => 2,
        }
    }
}

/// x86-64 数组 operator 的冻结操作。
///
/// 数组的物理表示、分配/释放实现、边界检查位置与失败出口全部由
/// [`super::array_runtime`] 冻结（`[0]` 元素个数、`[8]` 分配字节数、元素从 `+16`
/// 起按 8 字节槽排列）；集合语义（初始化前沿、有效窗口）仍留在 Valkyrie 标准库。
/// 这里只编码已经解析并绑定的四类机械操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum X64ArrayOperation {
    /// `array.new<T>(capacity) -> [T]`：分配并零初始化容量槽位。
    New,
    /// `array.length<T>([T]) -> usize`：读取头部元素个数。
    Length,
    /// `array.get<T>([T], index) -> T`：按 0 基下标读取一个 8 字节槽。
    Get,
    /// `array.set<T>([T], index, T) -> unit`：按 0 基下标写入一个 8 字节槽。
    Set,
}

/// 调用实参的寄存器类别与序位：整数与浮点各自独立编号。
#[derive(Debug, Clone, Copy)]
enum FloatPlacement {
    /// 整数寄存器序列第 `index` 个。
    Integer { index: usize },
    /// 浮点 `xmm` 序列第 `index` 个；`single` 决定 `movss`/`movsd`。
    Float { index: usize, single: bool },
}

/// 已冻结的 x86-64 数组 operator 计划。
///
/// 物理布局、分配/释放实现与失败出口由 [`super::array_runtime`] 唯一冻结；本计划
/// 只固定操作身份与调用点 arity，不复制第二套数组表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct X64ArrayOperatorPlan {
    /// 冻结的实参个数；编码前必须与调用实参个数一致。
    pub arity: usize,
    /// 冻结的数组操作身份。
    pub operation: X64ArrayOperation,
}

/// 已冻结的 x86-64 控制台 operator 身份。
///
/// `console.write(handle, message)` 把 `Utf8Text` 对象按冻结的数组合同 gather 成
/// 连续 UTF-8 字节后写出：Win64 走 `GetStdHandle` 伪句柄（-11 标准输出 / -12 标准
/// 错误）+ `WriteFile`，写出前设 `SetConsoleOutputCP(65001)`；SysV64/Linux 把
/// `handle` 直接当文件描述符走 `write(2)`。两条路径都只写 UTF-8 原始字节，
/// 重定向管道与控制台句柄共用同一写出通道。写出失败不伪造错误通道（unit 返回，
/// 与多数运行时的尽力写出一致）；staging 副本在写出后立即释放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum X64ConsoleOperation {
    /// `console.write(handle: i64, message: utf8) -> unit`。
    Write,
}

/// 可执行文件容器：决定入口接收方式与最终字节格式。
/// 容器选择来自规范目标的 `os`，不由 `arch` 推断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    /// Windows `PE32+`：loader 通过 `call` 进入入口，`rax` 作为退出码。
    Pe,
    /// Linux `ELF64 ET_EXEC`：内核直接 `jmp` 到入口，过程末尾必须以 `exit(2)` 结束。
    Elf,
}

impl ContainerKind {
    fn name(self) -> &'static str {
        match self {
            Self::Pe => "PE",
            Self::Elf => "ELF",
        }
    }
}

/// x86-64 程序装配服务的目标私有输入。
///
/// 内部只保存已降低的文本 LIR 程序、入口符号与容器选择；装配服务（编码 +
/// 容器 + 自检）只接受这个形状，不再接触跨目标的 `FragmentSubmission` /
/// `ExecutableFunction` / `InstructionKind`。
pub struct X64ProgramInput {
    abi: X64CallConv,
    container: ContainerKind,
    functions: Vec<X64ProgramFunction>,
    entry_name: String,
}

impl X64ProgramInput {
    /// 目标平台身份：由容器一次冻结，不从 `arch` 推断 `os`/`abi`。
    pub fn platform(&self) -> X64Platform {
        match self.container {
            ContainerKind::Pe => X64Platform::windows_x64(),
            ContainerKind::Elf => X64Platform::linux_x64(),
        }
    }
}

/// `x86_64-pc-windows-msvc` 的目标私有输入（Win64 + PE）。
pub(crate) fn build_x86_64_windows_program_input(submission: &FragmentSubmission) -> Result<X64ProgramInput> {
    build_x86_64_program_input(submission, X64CallConv::Win64, ContainerKind::Pe)
}

/// `x86_64-unknown-linux-gnu` 的目标私有输入（SysV64 + ELF）。
pub(crate) fn build_x86_64_linux_program_input(submission: &FragmentSubmission) -> Result<X64ProgramInput> {
    build_x86_64_program_input(submission, X64CallConv::SystemV64, ContainerKind::Elf)
}

/// 计划事实 → 目标私有 LIR 程序（不编码、不写容器）。
///
/// 这是语言语义最后一次以计划形状出现的地方；返回值之后的装配服务只认
/// [`X64ProgramInput`]。
pub(crate) fn build_x86_64_program_input(
    submission: &FragmentSubmission,
    abi: X64CallConv,
    container: ContainerKind,
) -> Result<X64ProgramInput> {
    let plan = &submission.backend_plan;
    let reachable = collect_reachable_functions(submission);
    if reachable.is_empty() {
        return Err(format!("x86_64 lowering 没有可达函数：片段既无导出也无入口操作，拒绝凭空生成 {}", container.name()));
    }

    let entry = plan
        .entry_operation()
        .ok_or_else(|| format!("x86_64 lowering 缺少入口操作：{} 入口点必须来自计划冻结的 `entry_operation`", container.name()))?;
    let entry_view = plan
        .get_function(&entry)
        .ok_or_else(|| format!("x86_64 lowering：入口操作 `{entry:?}` 没有函数体，不能作为 {} 入口", container.name()))?;
    validate_entry(&entry_view.function, container)?;

    let names = exe_symbol_names(&reachable);
    // 容器与调用约定由规范目标一次冻结；数组运行时按同一平台身份选择分配/释放实现。
    let platform = match container {
        ContainerKind::Pe => X64Platform::windows_x64(),
        ContainerKind::Elf => X64Platform::linux_x64(),
    };
    if platform.abi() != abi {
        return Err(format!(
            "x86_64 lowering：容器 {} 的冻结 ABI `{}` 与调用约定 `{}` 不一致",
            container.name(),
            platform.abi().name(),
            abi.name()
        ));
    }
    let mut functions = Vec::with_capacity(reachable.len());
    // 物理布局按规范平台的引用/文本槽策略一次解析；对象字段偏移只来自这里。
    let layouts = resolve_physical_layouts(plan.aggregate_layouts(), LayoutPolicy::X64)
        .map_err(|reason| format!("x86_64 lowering：物理布局解析失败：{reason}"))?;
    for instance in &reachable {
        let view = plan.get_function(instance).ok_or_else(|| format!("x86_64 lowering：可达操作 `{instance:?}` 缺少函数体"))?;
        functions.push(lower_function_to_text_program(instance, submission, &view.function, &reachable, &names, platform, &layouts)?);
    }

    let entry_name = names.get(&entry).expect("入口实例在可达函数符号表内").clone();
    Ok(X64ProgramInput { abi, container, functions, entry_name })
}

/// 目标私有 LIR 程序 → 容器镜像。
///
/// 这是本 lane 唯一的编码与容器装配入口：签名里没有 `FragmentSubmission` /
/// `ExecutableFunction`，复用唯一程序装配器与 PE/ELF 容器写出。
pub(crate) fn assemble_x86_64_program_image(input: &X64ProgramInput) -> Result<X64LoweredProgram> {
    let container = input.container;
    // 唯一程序装配器：帧、符号布局、`rel32` 回填与导入/数据装配不再各写一遍。
    let image = encode_x64_program(&input.functions, input.abi)?;
    let entry_function = u32::try_from(
        image
            .symbols
            .iter()
            .find(|(name, _)| *name == input.entry_name)
            .map(|(_, offset)| *offset as usize)
            .ok_or_else(|| "x86_64 lowering：入口函数没有生成代码".to_string())?,
    )
    .map_err(|_| "x86_64 lowering：入口偏移超出 `u32`".to_string())?;

    let (executable, entry_offset, text_len) = match container {
        ContainerKind::Pe => {
            let text_len = image.text.len();
            let executable = NativePeWriter::write_executable(&NativePeImage {
                text: image.text,
                rdata: Vec::new(),
                idata: image.idata,
                imports: image.imports,
                exports: Vec::new(),
                text_rel32_fixups: image.text_rel32_fixups,
                is_dll: false,
                entry_point: entry_function,
            })
            .map_err(|error| format!("x86_64 `PE` 编码失败：{error}"))?;
            (executable, entry_function, text_len)
        }
        ContainerKind::Elf => {
            let (text, entry_point) = append_elf_entry_exit_stub(image.text, entry_function as usize)?;
            let text_len = text.len();
            let executable = NativeElfWriter::write_executable(&NativeElfImage { text, rodata: Vec::new(), entry_point })
                .map_err(|error| format!("x86_64 `ELF` 编码失败：{error}"))?;
            (executable, entry_point, text_len)
        }
    };

    // 自检：产物必须是 AMD64 可执行镜像，且容器与规范目标一致。
    let facts = classify_machine_image(&executable).map_err(|error| format!("x86_64 产物自检失败：{error}"))?;
    let (expected_kind, expected_arch) = match container {
        ContainerKind::Pe => (MachineImageKind::Pe, BinaryArch::X64),
        ContainerKind::Elf => (MachineImageKind::Elf, BinaryArch::X64),
    };
    if facts.kind != expected_kind || facts.arch != expected_arch || facts.shared {
        return Err(format!("x86_64 产物自检失败：期望 AMD64 可执行 {}，实际 {facts:?}", container.name()));
    }

    Ok(X64LoweredProgram { executable, entry_offset, text_len })
}

/// exe 镜像内的符号名：实例身份是唯一权威，名字不参与语义，也不跨产物复用。
fn exe_symbol_names(reachable: &[ItemInstanceId]) -> BTreeMap<ItemInstanceId, String> {
    reachable.iter().map(|instance| (*instance, format!("__nyar_fn_{}", instance.index()))).collect()
}

/// 在 `.text` 末尾追加 ELF 进程入口桩，返回新文本与入口偏移。
///
/// 内核直接跳到 `e_entry`，因此入口不能像 PE 那样 `ret` 回 loader：桩先 `call`
/// 语言入口函数，把 `rax`（i32 退出码）移到 `rdi`，再执行 `exit(2)` 系统调用。
/// SysV64 要求调用点 `rsp` 16 字节对齐；进程入口正好满足，所以 `call` 后目标函数
/// 看到的是标准 `rsp ≡ 8 (mod 16)`，与 Win64 loader 调用一致。
fn append_elf_entry_exit_stub(mut text: Vec<u8>, entry_function: usize) -> Result<(Vec<u8>, u32)> {
    let entry_point = u32::try_from(text.len()).map_err(|_| "x86_64 ELF 入口桩偏移超出 `u32`".to_string())?;
    // `call rel32` 到语言入口函数。
    let field = x64_lir::emit_call_placeholder(&mut text);
    x64_lir::patch_rel32(&mut text, field, entry_function, || "x86_64 ELF 入口桩调用位移超出 `rel32`".to_string())?;
    // `mov rdi, rax`；`mov eax, 60`（SYS_exit）；`syscall`。
    x64_lir::emit_mov_rdi_from_rax(&mut text);
    x64_lir::emit_syscall_exit(&mut text);
    Ok((text, entry_point))
}

/// 入口函数必须是空形参、返回整数/布尔/字符或 unit。
///
/// 两个容器都以 `rax` 作为进程退出码（PE loader 读取返回值，ELF 入口桩把它传给
/// `exit`）；loader 不提供有意义的形参，因此入口只接受可直接作为退出码的标量返回。
fn validate_entry(function: &MirFunction, container: ContainerKind) -> Result<()> {
    let entry_block =
        function.blocks.get(function.entry.0 as usize).ok_or_else(|| format!("x86_64 lowering：入口函数 `{}` 缺少入口块", function.symbol))?;
    if !entry_block.parameters.is_empty() || !function.param_types.is_empty() {
        return Err(format!(
            "x86_64 lowering：{} 入口函数 `{}` 必须无参数（loader 不传递形参），实际 {} 个",
            container.name(),
            function.symbol,
            function.param_types.len()
        ));
    }
    if !is_exit_code_type(&function.return_type) {
        return Err(format!(
            "x86_64 lowering：{} 入口函数 `{}` 的返回类型 `{}` 不能作为进程退出码（只接受整数/布尔/字符/unit）",
            container.name(),
            function.symbol,
            function.return_type
        ));
    }
    Ok(())
}

/// 收集可达函数（导出面 + 入口 + 同片段调用闭包），按实例身份升序稳定输出。
pub(super) fn collect_reachable_functions(submission: &FragmentSubmission) -> Vec<ItemInstanceId> {
    let plan = &submission.backend_plan;
    let mut roots = BTreeSet::new();
    roots.extend(plan.export_names().keys().copied());
    roots.extend(plan.exported_operations().iter().copied());
    if let Some(entry) = plan.entry_operation() {
        roots.insert(entry);
    }

    let mut visited = BTreeSet::new();
    let mut ordered = Vec::new();
    loop {
        let Some(instance) = roots.iter().copied().find(|instance| !visited.contains(instance))
        else {
            break;
        };
        visited.insert(instance);
        let Some(view) = plan.get_function(&instance)
        else {
            continue;
        };
        for block in &view.function.blocks {
            for instruction in &block.instructions {
                let InstructionKind::Call { callee: Operand::Item(callee), .. } = &instruction.kind
                else {
                    continue;
                };
                if plan.get_function(callee).is_some() && !plan.imports().contains_key(callee) {
                    roots.insert(*callee);
                }
            }
        }
        ordered.push(instance);
    }
    ordered
}

/// 单函数降低：计划事实 → 文本 LIR 函数（不产生任何字节）。
///
/// exe 与宿主扩展复用这条降低：产物形态只改符号命名与容器，不改函数级语义。
pub(super) fn lower_function_to_text_program(
    instance: &ItemInstanceId,
    submission: &FragmentSubmission,
    function: &MirFunction,
    reachable: &[ItemInstanceId],
    names: &BTreeMap<ItemInstanceId, String>,
    platform: X64Platform,
    layouts: &ResolvedLayoutPlan,
) -> Result<X64ProgramFunction> {
    let slots = ExecutableSlotPlan::plan_semantic(function);
    let local_count = u16::try_from(slots.local_types.len()).map_err(|_| "x86_64 lowering：局部槽数量超出 `u16`".to_string())?;
    // 数组失败出口使用块标签之后的保留标签，避免与 MIR 块编号冲突。
    let failure_label = function
        .blocks
        .iter()
        .map(|block| block.id.0)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "x86_64 lowering：块标签编号已耗尽，无法分配数组失败出口".to_string())?;
    let mut lowerer = FunctionLowerer {
        submission,
        function,
        slots,
        reachable,
        names,
        abi: platform.abi(),
        allocator: ArrayAllocator::for_platform(platform),
        layouts,
        local_count,
        temps_used: 0,
        instructions: Vec::new(),
        failure_label,
        next_label: failure_label + 1,
        uses_failure_exit: false,
    };
    lowerer.store_entry_parameters()?;

    let mut order = vec![function.entry];
    for block in collect_reachable_blocks(function) {
        if block != function.entry {
            order.push(block);
        }
    }
    for block_id in order {
        let Some(block) = function.blocks.get(block_id.0 as usize)
        else {
            return Err(format!("x86_64 lowering：函数 `{}` 引用了不存在的块 {:?}", function.symbol, block_id));
        };
        lowerer.lower_block(block)?;
    }

    if lowerer.uses_failure_exit {
        // 定义好的数组失败出口：显式陷入，不伪造返回、不把空指针留到后续故障。
        for instruction in array_runtime::array_failure_exit(lowerer.failure_label) {
            lowerer.push(instruction);
        }
    }

    let temps = u16::try_from(lowerer.temps_used).map_err(|_| "x86_64 lowering：临时槽数量超出 `u16`".to_string())?;
    let slot_count = local_count.checked_add(temps).ok_or_else(|| "x86_64 lowering：帧槽数量溢出".to_string())?;
    Ok(X64ProgramFunction {
        name: names.get(instance).expect("可达操作有符号名").clone(), slot_count, instructions: lowerer.instructions
    })
}

/// 单函数降低器：把计划事实转成文本 LIR，不产生任何字节。
struct FunctionLowerer<'a> {
    submission: &'a FragmentSubmission,
    function: &'a MirFunction,
    slots: ExecutableSlotPlan,
    reachable: &'a [ItemInstanceId],
    /// 实例 → 镜像符号名。
    names: &'a BTreeMap<ItemInstanceId, String>,
    /// 由规范目标 `os`/`abi` 冻结的调用约定。
    abi: X64CallConv,
    /// 由同一平台身份冻结的数组分配/释放实现。
    allocator: ArrayAllocator,
    /// 目标已解析的物理布局：对象尺寸与字段偏移只来自这里。
    layouts: &'a ResolvedLayoutPlan,
    /// 局部槽数量；临时槽从该编号起分配。
    local_count: u16,
    /// 本函数同时需要的最大临时槽数。
    temps_used: usize,
    instructions: Vec<X64TextInstruction>,
    /// 数组失败出口标签（块标签之后的一个保留编号）。
    failure_label: u32,
    /// 动态标签分配游标（失败出口之后起，供展开循环等按调用点取唯一标签）。
    next_label: u32,
    /// 是否已经发射过数组失败分支（决定是否需要失败出口）。
    uses_failure_exit: bool,
}

impl FunctionLowerer<'_> {
    fn push(&mut self, instruction: X64TextInstruction) {
        self.instructions.push(instruction);
    }

    /// 预留 `count` 个临时帧槽（局部槽之后起分配，独立的周转序列之间复用），
    /// 返回首临时槽编号。
    fn reserve_temps(&mut self, count: usize) -> u16 {
        self.temps_used = self.temps_used.max(count);
        self.local_count
    }

    /// 第 `index` 个临时槽的帧槽编号。
    fn temp_slot(&self, base: u16, index: usize) -> Result<u16> {
        base.checked_add(u16::try_from(index).map_err(|_| "x86_64 lowering：临时槽序号溢出".to_string())?)
            .ok_or_else(|| "x86_64 lowering：临时槽溢出帧槽编号".to_string())
    }

    /// 分配一个函数内唯一标签（失败出口之后起，同函数多次展开调用互不冲突）。
    fn fresh_label(&mut self) -> u32 {
        let label = self.next_label;
        self.next_label += 1;
        label
    }

    fn store_entry_parameters(&mut self) -> Result<()> {
        let entry_block = self
            .function
            .blocks
            .get(self.function.entry.0 as usize)
            .ok_or_else(|| format!("x86_64 lowering：函数 `{}` 缺少入口块", self.function.symbol))?;
        if entry_block.parameters.len() != self.function.param_types.len() {
            return Err(format!(
                "x86_64 lowering：函数 `{}` 入口块形参 {} 个与冻结签名 {} 个不一致",
                self.function.symbol,
                entry_block.parameters.len(),
                self.function.param_types.len()
            ));
        }
        let register_limit = if self.abi.shares_register_files() {
            self.abi.integer_argument_limit().max(self.abi.float_argument_limit())
        } else {
            self.abi.integer_argument_limit() + self.abi.float_argument_limit()
        };
        if entry_block.parameters.len() > register_limit {
            return Err(format!(
                "x86_64 lowering：函数 `{}` 有 {} 个形参，超过 {} 寄存器传参上限（整数 {} + 浮点 {}；栈上传参未实现）",
                self.function.symbol,
                entry_block.parameters.len(),
                self.abi.name(),
                self.abi.integer_argument_limit(),
                self.abi.float_argument_limit()
            ));
        }
        // Win64 按形参位置同时消耗整数与浮点寄存器序位（第 i 个形参用 `rcx+i`
        // 或 `xmm0+i`）；SysV64 两个寄存器文件各自独立编号。混合签名不得按类别另算序位。
        let shared_files = self.abi.shares_register_files();
        let mut integer_index = 0u8;
        let mut float_index = 0u8;
        for (index, parameter) in entry_block.parameters.iter().enumerate() {
            let local = self
                .slots
                .block_param_locals
                .get(&(self.function.entry, index))
                .copied()
                .ok_or_else(|| format!("x86_64 lowering：函数 `{}` 入口形参 {index} 缺少局部槽", self.function.symbol))?;
            let ty = self
                .function
                .value_types
                .get(parameter)
                .or_else(|| self.function.param_types.get(index))
                .cloned();
            if let Some(ty) = &ty
                && !self.is_local_carrier(ty)
            {
                return Err(format!("x86_64 lowering：函数 `{}` 的形参类型 `{ty}` 没有 x86_64 槽载体", self.function.symbol));
            }
            if matches!(ty, Some(SemanticType::Float32 | SemanticType::Float64)) {
                let single = matches!(ty, Some(SemanticType::Float32));
                let register_index = if shared_files {
                    u8::try_from(index).map_err(|_| "x86_64 lowering：入口形参序位溢出 u8".to_string())?
                } else {
                    float_index
                };
                if usize::from(register_index) >= self.abi.float_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：函数 `{}` 的浮点形参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        self.function.symbol,
                        self.abi.name(),
                        self.abi.float_argument_limit()
                    ));
                }
                self.push(X64TextInstruction::LoadParamXmm { index: register_index, slot: local, single });
                float_index += 1;
            }
            else {
                let register_index = if shared_files {
                    u8::try_from(index).map_err(|_| "x86_64 lowering：入口形参序位溢出 u8".to_string())?
                } else {
                    integer_index
                };
                if usize::from(register_index) >= self.abi.integer_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：函数 `{}` 的整数形参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        self.function.symbol,
                        self.abi.name(),
                        self.abi.integer_argument_limit()
                    ));
                }
                self.push(X64TextInstruction::LoadParam { index: register_index, slot: local });
                integer_index += 1;
            }
        }
        Ok(())
    }

    fn lower_block(&mut self, block: &MirBlock) -> Result<()> {
        self.push(X64TextInstruction::Label(block.id.0));
        for instruction in &block.instructions {
            self.lower_instruction(instruction)?;
        }
        self.lower_terminator(&block.terminator)
    }

    fn lower_instruction(&mut self, instruction: &MirInstruction) -> Result<()> {
        if instruction.results.len() > 1 {
            return Err(format!(
                "x86_64 lowering 不支持多结果指令（results={}）: function={}",
                instruction.results.len(),
                self.function.symbol
            ));
        }
        match &instruction.kind {
            InstructionKind::LoadConstant { constant, .. } => {
                let local = self.require_output_local(instruction)?;
                // 浮点常量按结果槽的冻结宽度落位：`f64` 常量进 `f32` 槽时按目标精度
                // 舍入，避免把 64 位位模式原样塞进 32 位语义的槽。
                if let Constant::Float64(value) = constant {
                    let single = matches!(self.slots.local_types.get(local as usize), Some(SemanticType::Float32));
                    let bits = if single { u64::from((value.0 as f32).to_bits()) } else { value.0.to_bits() };
                    self.push(X64TextInstruction::LoadI64(bits as i64));
                    self.push(X64TextInstruction::StoreSlot(local));
                    return Ok(());
                }
                self.emit_constant_to_rax(constant)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            InstructionKind::Copy { source } => {
                let local = self.require_output_local(instruction)?;
                self.emit_operand_to_rax(source)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            InstructionKind::StoreVar { value, .. } => {
                if self.output_local(instruction)?.is_none() {
                    // 具名局部不在 native 后端物化；操作数本身是纯值，丢弃不产生副作用。
                    return Ok(());
                }
                let local = self.require_output_local(instruction)?;
                self.emit_operand_to_rax(value)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            InstructionKind::Cast { source, ty } => {
                // 语言层 `as` 是表示转换：转换语义只由计划冻结的两端 `SemanticType`
                // 决定（槽位宽与符号性无法从 64 位帧槽反推），不得退化为 Copy。
                let local = self.require_output_local(instruction)?;
                let source_ty = self.operand_semantic_type(source)?;
                self.emit_operand_to_rax(source)?;
                self.emit_cast_conversion(&source_ty, ty)?;
                if matches!(ty, SemanticType::Float32 | SemanticType::Float64) {
                    // 浮点目标的结果在 `xmm0`（整数目标在 `rax`）。
                    self.push(X64TextInstruction::StoreXmm0ToSlot { slot: local, single: matches!(ty, SemanticType::Float32) });
                }
                else {
                    self.push(X64TextInstruction::StoreSlot(local));
                }
                Ok(())
            }
            InstructionKind::Call { callee, arguments } => self.emit_call(instruction, callee, arguments),
            InstructionKind::ArrayNew { array_type, length, initialization } => {
                let element = heap_array_element_type(array_type).ok_or_else(|| {
                    format!("x86_64 lowering：`ArrayNew` 的类型 `{array_type}` 不是堆数组 `[T]`：function={}", self.function.symbol)
                })?;
                if !self.is_local_carrier(element) {
                    return Err(format!(
                        "x86_64 lowering：数组元素类型 `{element}` 没有 8 字节槽载体：function={}",
                        self.function.symbol
                    ));
                }
                match initialization {
                    ArrayInitialization::Default => {
                        // `HeapAlloc(HEAP_ZERO_MEMORY)` 已把元素槽置零；标量元素的
                        // 语言默认值就是零，无需第二遍填充。
                    }
                    ArrayInitialization::Fill(_) => {
                        return Err(format!(
                            "x86_64 lowering 尚不支持 `ArrayNew` 的 `Fill` 初始化：function={}",
                            self.function.symbol
                        ));
                    }
                }
                let local = self.require_output_local(instruction)?;
                self.emit_array_allocation(length)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            InstructionKind::ArrayFromElements { array_type, elements, .. } => {
                let element = heap_array_element_type(array_type).ok_or_else(|| {
                    format!(
                        "x86_64 lowering：`ArrayFromElements` 的类型 `{array_type}` 不是堆数组 `[T]`：function={}",
                        self.function.symbol
                    )
                })?;
                if !self.is_local_carrier(element) {
                    return Err(format!(
                        "x86_64 lowering：数组元素类型 `{element}` 没有 8 字节槽载体：function={}",
                        self.function.symbol
                    ));
                }
                let count = i64::try_from(elements.len())
                    .map_err(|_| format!("x86_64 lowering：数组字面量长度超出 `i64`：function={}", self.function.symbol))?;
                let local = self.require_output_local(instruction)?;
                self.emit_array_allocation(&Operand::Constant(Constant::Int(count)))?;
                self.push(X64TextInstruction::StoreSlot(local));
                // 逐元素写入：值先落临时槽，避免常量求值借用 `rax` 覆盖数组指针。
                // 下标是编译期常量且恒小于刚分配的元素个数，范围证明成立，无需运行时检查。
                let base = self.reserve_temps(2);
                let value_slot = self.temp_slot(base, 0)?;
                let index_slot = self.temp_slot(base, 1)?;
                for (index, value) in elements.iter().enumerate() {
                    self.emit_operand_to_rax(value)?;
                    self.push(X64TextInstruction::StoreSlot(value_slot));
                    self.push(X64TextInstruction::LoadI64(index as i64));
                    self.push(X64TextInstruction::StoreSlot(index_slot));
                    self.push(X64TextInstruction::LoadSlot(local));
                    self.push(X64TextInstruction::LoadSlotToRcx(index_slot));
                    self.push(X64TextInstruction::LoadSlotToRdx(value_slot));
                    self.push(X64TextInstruction::StoreIndexedRdxToRaxRcx);
                }
                Ok(())
            }
            InstructionKind::ArrayLength { array } => {
                let local = self.require_output_local(instruction)?;
                self.require_heap_array_operand(array)?;
                self.emit_operand_to_rax(array)?;
                self.push(X64TextInstruction::LoadFromRax);
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            InstructionKind::ArrayGet { array, index } => {
                let local = self.require_output_local(instruction)?;
                self.require_heap_array_operand(array)?;
                self.emit_array_bounds_check(array, index)?;
                // 下标先装载：常量下标会借用 `rax` 作暂存，数组指针必须最后装入。
                self.emit_operand_to_rcx(index)?;
                self.emit_operand_to_rax(array)?;
                self.push(X64TextInstruction::LoadIndexedFromRaxRcx);
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            InstructionKind::ArraySet { array, index, value } => {
                if self.output_local(instruction)?.is_some() {
                    return Err(format!("x86_64 lowering：`ArraySet` 不得绑定结果槽：function={}", self.function.symbol));
                }
                self.require_heap_array_operand(array)?;
                self.emit_array_bounds_check(array, index)?;
                self.emit_operand_to_rcx(index)?;
                self.emit_operand_to_rdx(value)?;
                self.emit_operand_to_rax(array)?;
                self.push(X64TextInstruction::StoreIndexedRdxToRaxRcx);
                Ok(())
            }
            InstructionKind::StructNew { nominal, fields } => self.emit_struct_new(instruction, *nominal, fields),
            InstructionKind::TupleNew { layout_id, fields } => self.emit_tuple_new(instruction, *layout_id, fields),
            InstructionKind::TupleGet { object, layout_id, index } => self.emit_tuple_get(instruction, object, *layout_id, *index),
            InstructionKind::AggregateCopy { layout_id, source, dest } => self.emit_aggregate_copy(instruction, *layout_id, source, dest),
            InstructionKind::FieldGet { object, field } => self.emit_field_get(instruction, object, *field),
            InstructionKind::FieldSet { object, field, value } => self.emit_field_set(instruction, object, *field, value),
            InstructionKind::SumNew { nominal, variant, payload } => self.emit_sum_new(instruction, *nominal, *variant, payload.as_ref()),
            InstructionKind::SumVariantIs { nominal, variant, object } => {
                self.emit_sum_variant_is(instruction, *nominal, *variant, object)
            }
            InstructionKind::SumPayloadGet { nominal, variant, object } => {
                self.emit_sum_payload_get(instruction, *nominal, *variant, object)
            }
            InstructionKind::NullIsNull { value } => {
                let local = self.require_output_local(instruction)?;
                let base = self.reserve_temps(2);
                let value_slot = self.temp_slot(base, 0)?;
                let zero_slot = self.temp_slot(base, 1)?;
                self.emit_operand_to_rax(value)?;
                self.push(X64TextInstruction::StoreSlot(value_slot));
                self.push(X64TextInstruction::LoadI64(0));
                self.push(X64TextInstruction::StoreSlot(zero_slot));
                self.push(X64TextInstruction::LoadSlot(value_slot));
                self.push(X64TextInstruction::LoadSlotToRcx(zero_slot));
                self.push(X64TextInstruction::Operator(X86_64OperatorCode::I64CompareEqual));
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            InstructionKind::NullUnwrap { value } => {
                let local = self.require_output_local(instruction)?;
                let ty = self.operand_semantic_type(value)?;
                match &ty {
                    SemanticType::Nullable(inner)
                        if matches!(
                            **inner,
                            SemanticType::Nominal { .. } | SemanticType::Apply(..) | SemanticType::Array(_) | SemanticType::Union(_)
                                | SemanticType::Utf8
                                | SemanticType::Utf16
                        ) => {}
                    _ => {
                        return Err(format!(
                            "x86_64 lowering：`NullUnwrap` 首期只开放引用可空（值可空需要装箱合同）：type={ty} function={}",
                            self.function.symbol
                        ));
                    }
                }
                // 引用可空：非空值就是同一个指针，语言已保证非空。
                self.emit_operand_to_rax(value)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            other => Err(format!("x86_64 lowering 尚不支持指令 `{}`：function={}", instruction_kind_name(other), self.function.symbol)),
        }
    }

    fn lower_terminator(&mut self, terminator: &Terminator) -> Result<()> {
        match terminator {
            Terminator::Return { value } => self.emit_return(value.as_ref()),
            Terminator::Jump { target, arguments } => self.emit_jump(*target, arguments),
            Terminator::Branch { condition, then_target, else_target } => {
                let targets_have_parameters = self
                    .function
                    .blocks
                    .iter()
                    .filter(|block| block.id == *then_target || block.id == *else_target)
                    .any(|block| !block.parameters.is_empty());
                if targets_have_parameters {
                    return Err(format!("x86_64 lowering 只支持无块参数的条件分支：function={}", self.function.symbol));
                }
                self.emit_operand_to_rax(condition)?;
                self.push(X64TextInstruction::JumpIfZero(else_target.0));
                self.push(X64TextInstruction::Jump(then_target.0));
                Ok(())
            }
            Terminator::Unreachable => {
                // `ud2`：到达即故障，不伪造返回。
                self.push(X64TextInstruction::Trap);
                Ok(())
            }
            Terminator::Raise { .. } => {
                // 无人接手的 `raise`（panic 路径）：原生没有 effect 运行时，与 wasm 同构
                // 以 `ud2` 终止；不伪造返回，也不尝试格式化消息（原生文本载体尚未冻结）。
                self.push(X64TextInstruction::Trap);
                Ok(())
            }
            other => Err(format!("x86_64 lowering 尚不支持终结符 `{other:?}`: function={}", self.function.symbol)),
        }
    }

    fn emit_return(&mut self, value: Option<&Operand>) -> Result<()> {
        let returns_unit = matches!(self.function.return_type, SemanticType::Unit | SemanticType::Bottom);
        match (value, returns_unit) {
            (None, true) => self.push(X64TextInstruction::LoadI64(0)),
            (None, false) => {
                return Err(format!("x86_64 lowering：非 void 函数 `{}` 缺少返回值", self.function.symbol));
            }
            (Some(Operand::Constant(Constant::Unit)), true) => self.push(X64TextInstruction::LoadI64(0)),
            (Some(Operand::Constant(Constant::Unit)), false) => {
                return Err(format!("x86_64 lowering：非 void 函数 `{}` 返回了 unit 常量", self.function.symbol));
            }
            (Some(operand), false) => {
                if matches!(self.function.return_type, SemanticType::Float32 | SemanticType::Float64) {
                    let single = matches!(self.function.return_type, SemanticType::Float32);
                    let slot = self.emit_float_return_value(operand, single)?;
                    self.push(X64TextInstruction::LoadSlotToXmm0 { slot, single });
                }
                else {
                    self.emit_operand_to_rax(operand)?;
                }
            }
            (Some(_), true) => {
                return Err(format!("x86_64 lowering：函数 `{}` 返回了值但签名是 unit", self.function.symbol));
            }
        }
        // 尾声由程序装配器按 LIR `Return` 统一产生。
        self.push(X64TextInstruction::Return);
        Ok(())
    }

    fn emit_jump(&mut self, target: MirBlockRef, arguments: &[Operand]) -> Result<()> {
        let target_block = self
            .function
            .blocks
            .get(target.0 as usize)
            .ok_or_else(|| format!("x86_64 lowering：函数 `{}` 跳转目标块 {:?} 不存在", self.function.symbol, target))?;
        if arguments.len() != target_block.parameters.len() {
            return Err(format!(
                "x86_64 lowering：跳转目标块 {:?} 形参数量与实参不符: function={} expected={} actual={}",
                target,
                self.function.symbol,
                target_block.parameters.len(),
                arguments.len()
            ));
        }
        if arguments.len() > self.abi.integer_argument_limit() {
            return Err(format!(
                "x86_64 lowering：块 {:?} 的跳转实参 {} 个超过临时槽上限 {}（栈上传参未实现）: function={}",
                target,
                arguments.len(),
                self.abi.integer_argument_limit(),
                self.function.symbol
            ));
        }
        // 先按序落到临时帧槽，再从临时槽装入目标块形参槽：
        // 这样源与目标槽重叠时也不会互相覆盖。
        let base = self.reserve_temps(arguments.len());
        for (index, argument) in arguments.iter().enumerate() {
            self.emit_operand_to_rax(argument)?;
            let slot = self.temp_slot(base, index)?;
            self.push(X64TextInstruction::StoreSlot(slot));
        }
        for index in 0..arguments.len() {
            let local = self
                .slots
                .block_param_locals
                .get(&(target, index))
                .copied()
                .ok_or_else(|| format!("x86_64 lowering：跳转目标块 {:?} 形参 {index} 缺少局部槽", target))?;
            let slot = self.temp_slot(base, index)?;
            self.push(X64TextInstruction::LoadSlot(slot));
            self.push(X64TextInstruction::StoreSlot(local));
        }
        self.push(X64TextInstruction::Jump(target.0));
        Ok(())
    }

    fn emit_call(&mut self, instruction: &MirInstruction, callee: &Operand, arguments: &[Operand]) -> Result<()> {
        let Operand::Item(instance) = callee
        else {
            return Err(format!("x86_64 lowering 只支持 `Item` 身份的调用: function={} callee={callee:?}", self.function.symbol));
        };
        if self.submission.backend_plan.imports().contains_key(instance) {
            return self.emit_operator_call(instruction, *instance, arguments);
        }
        if !self.reachable.contains(instance) {
            return Err(format!("x86_64 lowering：同片段调用目标 `{instance:?}` 不在可达函数集中: function={}", self.function.symbol));
        }
        let view = self
            .submission
            .backend_plan
            .get_function(instance)
            .ok_or_else(|| format!("x86_64 lowering：调用目标 `{instance:?}` 缺少函数体: function={}", self.function.symbol))?;
        let callee_function = &view.function;
        if callee_function.param_types.len() != arguments.len() {
            return Err(format!(
                "x86_64 lowering：调用 `{}` 实参数量与冻结签名不符: function={} expected={} actual={}",
                callee_function.symbol,
                self.function.symbol,
                callee_function.param_types.len(),
                arguments.len()
            ));
        }
        let register_limit = if self.abi.shares_register_files() {
            self.abi.integer_argument_limit().max(self.abi.float_argument_limit())
        } else {
            self.abi.integer_argument_limit() + self.abi.float_argument_limit()
        };
        if arguments.len() > register_limit {
            return Err(format!(
                "x86_64 lowering：调用 `{}` 传递 {} 个实参，超过 {} 寄存器传参上限（整数 {} + 浮点 {}；栈上传参未实现）",
                callee_function.symbol,
                arguments.len(),
                self.abi.name(),
                self.abi.integer_argument_limit(),
                self.abi.float_argument_limit()
            ));
        }
        // 整数与浮点实参的寄存器序位与入口形参同一规则：Win64 按形参位置共享
        // 序位，SysV64 各自独立编号。
        let shared_files = self.abi.shares_register_files();
        let mut placements = Vec::with_capacity(arguments.len());
        let mut integer_index = 0usize;
        let mut float_index = 0usize;
        for (index, argument) in arguments.iter().enumerate() {
            let ty = callee_function.param_types.get(index).cloned();
            if matches!(ty, Some(SemanticType::Float32 | SemanticType::Float64)) {
                let single = matches!(ty, Some(SemanticType::Float32));
                let register_index = if shared_files { index } else { float_index };
                if register_index >= self.abi.float_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：调用 `{}` 的浮点实参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        callee_function.symbol,
                        self.abi.name(),
                        self.abi.float_argument_limit()
                    ));
                }
                placements.push(FloatPlacement::Float { index: register_index, single });
                float_index += 1;
            }
            else {
                let register_index = if shared_files { index } else { integer_index };
                if register_index >= self.abi.integer_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：调用 `{}` 的整数实参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        callee_function.symbol,
                        self.abi.name(),
                        self.abi.integer_argument_limit()
                    ));
                }
                placements.push(FloatPlacement::Integer { index: register_index });
                integer_index += 1;
            }
        }
        let name = self
            .names
            .get(instance)
            .cloned()
            .ok_or_else(|| format!("x86_64 lowering：调用目标 `{instance:?}` 缺少镜像符号名: function={}", self.function.symbol))?;
        // 与块参数周转同理：先全部落临时槽，再按序装入实参寄存器。
        let base = self.reserve_temps(arguments.len());
        for (index, argument) in arguments.iter().enumerate() {
            let slot = self.temp_slot(base, index)?;
            // 浮点常量按目标宽度落位模式；其余操作数按统一路径搬运。
            if let (FloatPlacement::Float { single, .. }, Operand::Constant(Constant::Float64(value))) = (&placements[index], argument) {
                let bits = if *single { u64::from((value.0 as f32).to_bits()) } else { value.0.to_bits() };
                self.push(X64TextInstruction::LoadI64(bits as i64));
            }
            else {
                self.emit_operand_to_rax(argument)?;
            }
            self.push(X64TextInstruction::StoreSlot(slot));
        }
        for (slot_index, placement) in placements.iter().enumerate() {
            let slot = self.temp_slot(base, slot_index)?;
            match placement {
                FloatPlacement::Integer { index } => {
                    self.push(X64TextInstruction::LoadSlotToArg { slot, index: *index as u8 });
                }
                FloatPlacement::Float { index, single } => {
                    self.push(X64TextInstruction::LoadSlotToXmmArg { slot, index: *index as u8, single: *single });
                }
            }
        }
        self.push(X64TextInstruction::Call(name));

        if matches!(callee_function.return_type, SemanticType::Unit | SemanticType::Bottom) {
            if self.output_local(instruction)?.is_some() {
                return Err(format!(
                    "x86_64 lowering：void 调用 `{}` 不得绑定结果槽: function={}",
                    callee_function.symbol, self.function.symbol
                ));
            }
            return Ok(());
        }
        let local = self.require_output_local(instruction)?;
        if matches!(callee_function.return_type, SemanticType::Float32 | SemanticType::Float64) {
            let single = matches!(callee_function.return_type, SemanticType::Float32);
            self.push(X64TextInstruction::StoreXmm0ToSlot { slot: local, single });
        }
        else {
            self.push(X64TextInstruction::StoreSlot(local));
        }
        Ok(())
    }

    /// `[native_operator(...)]` 冻结绑定：只机械展开计划冻结的编码。
    fn emit_operator_call(&mut self, instruction: &MirInstruction, instance: ItemInstanceId, arguments: &[Operand]) -> Result<()> {
        let (binding, link, array_element) = {
            let import = self
                .submission
                .backend_plan
                .imports()
                .get(&instance)
                .ok_or_else(|| format!("x86_64 lowering：导入实例 `{instance:?}` 缺失: function={}", self.function.symbol))?;
            let symbol = self.submission.backend_plan.abi_name_for_instance(instance).map(|name| name.to_string()).unwrap_or_default();
            let selected = crate::lowering::shared::interop::select_unique_target(&import.links, x86_64_operator_target)?;
            let binding = match selected {
                Some((link, _)) => freeze_x86_64_operator(link, instance, &symbol, &import.parameter_types, &import.return_type)?,
                None => None,
            };
            // 数组 operator 的元素载体必须由计划裁决：值存储聚合等虽然形状像指针，
            // 在本 lane 没有内联槽合同，不能当元素搬运。
            let array_element = match &binding {
                Some(X86OperatorBinding::Array(plan)) => {
                    let ty = match plan.operation {
                        X64ArrayOperation::New => &import.return_type,
                        X64ArrayOperation::Length | X64ArrayOperation::Get | X64ArrayOperation::Set => &import.parameter_types[0],
                    };
                    heap_array_element_type(ty).cloned()
                }
                _ => None,
            };
            let links = import.links.iter().map(|link| link.to_string()).collect::<Vec<_>>().join(", ");
            (binding, links, array_element)
        };
        if let Some(element) = &array_element
            && !self.is_local_carrier(element)
        {
            return Err(format!(
                "x86_64 lowering：数组 operator 元素类型 `{element}` 没有 8 字节槽载体：function={}",
                self.function.symbol
            ));
        }
        let Some(binding) = binding
        else {
            return Err(format!(
                "x86_64 lowering 尚不支持宿主导入 `{instance:?}`（链接 `{link}`）：缺少宿主导入编码，function={}",
                self.function.symbol
            ));
        };
        let plan = match binding {
            X86OperatorBinding::Array(plan) => return self.emit_array_operator_call(instruction, plan, arguments),
            X86OperatorBinding::Console(operation) => return self.emit_console_write(instruction, operation, arguments),
            X86OperatorBinding::Frozen(plan) => plan,
            X86OperatorBinding::Unfrozen => {
                return Err(format!(
                    "x86_64 目标 operator `{instance:?}` 绑定未冻结（链接 `{link}`）：拒绝补造指令: function={}",
                    self.function.symbol
                ));
            }
        };
        let code = plan.encoding;
        if arguments.len() != plan.arity || plan.arity != code.arity() {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 实参数量与冻结 arity 不符: function={} expected={} actual={}",
                self.function.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        if plan.recipe != TargetOperatorRecipe::Operands {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 冻结了非 `Operands` 配方：x86_64 只执行机械实参发射: function={}",
                self.function.symbol
            ));
        }
        let expected_operand = code.operand_type();
        if plan.operand_type != expected_operand {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 的 operand_type={:?} 与 x86_64 冻结码不符（期望 {expected_operand:?}）: function={}",
                plan.operand_type, self.function.symbol
            ));
        }
        let expected_result = code.result_type();
        if plan.result_type != expected_result {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 的 result_type={:?} 与冻结码不符（期望 {expected_result:?}）: function={}",
                plan.result_type, self.function.symbol
            ));
        }
        let local = self.require_output_local(instruction)?;
        if code.is_float() {
            // 浮点载体：操作数在 `xmm0`/`xmm1`，比较结果回到 `rax` 的 0/1，
            // 其余结果留在 `xmm0` 并按其宽度写回 8 字节槽。
            let single = code.is_single_precision();
            self.emit_float_operand_to_xmm(0, &arguments[0], single)?;
            if plan.arity == 2 {
                self.emit_float_operand_to_xmm(1, &arguments[1], single)?;
            }
            self.push(X64TextInstruction::Operator(code));
            if code.is_comparison() {
                self.push(X64TextInstruction::StoreSlot(local));
            }
            else {
                self.push(X64TextInstruction::StoreXmm0ToSlot { slot: local, single });
            }
            return Ok(());
        }
        self.emit_operand_to_rax(&arguments[0])?;
        if plan.arity == 2 {
            self.emit_operand_to_rcx(&arguments[1])?;
        }
        self.push(X64TextInstruction::Operator(code));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// 把浮点操作数装入 `xmm0`/`xmm1`。
    ///
    /// 常量按冻结宽度落成位模式：`f32` 目标把 `f64` 常量窄化到目标精度（与显式
    /// `as f32` 的舍入一致），`f64` 目标保留原始位模式。
    fn emit_float_operand_to_xmm(&mut self, register: u8, operand: &Operand, single: bool) -> Result<()> {
        let slot = match operand {
            Operand::Value(value) => self.value_local(*value)?,
            Operand::Constant(Constant::Float64(value)) => {
                let bits = if single { u64::from((value.0 as f32).to_bits()) } else { value.0.to_bits() };
                self.push(X64TextInstruction::LoadI64(bits as i64));
                let base = self.reserve_temps(1);
                let slot = self.temp_slot(base, 0)?;
                self.push(X64TextInstruction::StoreSlot(slot));
                slot
            }
            other => {
                return Err(format!(
                    "x86_64 lowering 尚不支持浮点操作数 `{other:?}`：缺少 x86_64 浮点常量载体合同，function={}",
                    self.function.symbol
                ));
            }
        };
        match register {
            0 => self.push(X64TextInstruction::LoadSlotToXmm0 { slot, single }),
            1 => self.push(X64TextInstruction::LoadSlotToXmm1 { slot, single }),
            other => {
                return Err(format!(
                    "x86_64 lowering：浮点算子只使用 xmm0/xmm1，收到 xmm{other}: function={}",
                    self.function.symbol
                ));
            }
        }
        Ok(())
    }

    /// 浮点返回值落槽：值操作数直接复用其局部槽，常量按宽度落临时槽。
    fn emit_float_return_value(&mut self, operand: &Operand, single: bool) -> Result<u16> {
        match operand {
            Operand::Value(value) => self.value_local(*value),
            Operand::Constant(Constant::Float64(value)) => {
                let bits = if single { u64::from((value.0 as f32).to_bits()) } else { value.0.to_bits() };
                self.push(X64TextInstruction::LoadI64(bits as i64));
                let base = self.reserve_temps(1);
                let slot = self.temp_slot(base, 0)?;
                self.push(X64TextInstruction::StoreSlot(slot));
                Ok(slot)
            }
            other => Err(format!(
                "x86_64 lowering 尚不支持浮点返回值操作数 `{other:?}`：缺少 x86_64 浮点常量载体合同，function={}",
                self.function.symbol
            )),
        }
    }

    fn emit_constant_to_rax(&mut self, constant: &Constant) -> Result<()> {
        match constant {
            Constant::Int(value) => {
                self.push(X64TextInstruction::LoadI64(*value));
                Ok(())
            }
            Constant::Float64(value) => {
                // 浮点常量以位模式进入通用寄存器槽；消费方按宽度用 `movsd/movss` 装载。
                self.push(X64TextInstruction::LoadI64(value.0.to_bits() as i64));
                Ok(())
            }
            Constant::Bool(value) => {
                self.push(X64TextInstruction::LoadI64(i64::from(*value)));
                Ok(())
            }
            // 编译期文本物化为真实 `Utf8Text` 对象（见 `emit_text_constant`）。
            Constant::Utf8(value) => self.emit_text_constant(value),
            // `utf16` 复用静态描述符：本 lane 尚未闭合任何真实的 UTF-16 消费路径
            // （见 `is_local_carrier_type`），语言值图里不会出现读取其内容的边。
            Constant::Utf16(value) => {
                self.push(X64TextInstruction::LoadString(value.clone()));
                Ok(())
            }
            // 引用可空的空值就是空指针；标量可空没有内联载体，仍需装箱合同。
            Constant::Null => {
                self.push(X64TextInstruction::LoadI64(0));
                Ok(())
            }
            other => Err(format!("x86_64 lowering 尚不支持常量 `{other:?}`：缺少 x86_64 常量载体合同，function={}", self.function.symbol)),
        }
    }

    /// 把编译期文本物化为 `Utf8Text` 对象：`{ [0] = _repr 数组指针 }`，数组按
    /// `[u8]` 合同逐字节占一个 8 字节槽（见 `array_runtime`）。
    ///
    /// 这是 guest 里语言文本值的唯一载体：std 的内容路径（`FieldGet _repr`、
    /// `equals_repr`、`byte_at`）都按对象读取，静态 `{self_rel,len}` 描述符只
    /// 留给宿主桥的镜像文本，不再充当语言值。分配失败走统一定义好的失败出口。
    fn emit_text_constant(&mut self, value: &str) -> Result<()> {
        let bytes = value.as_bytes();
        let base = self.reserve_temps(5);
        let count_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        let array_slot = self.temp_slot(base, 4)?;
        self.push(X64TextInstruction::LoadI64(bytes.len() as i64));
        self.uses_failure_exit = true;
        for item in array_runtime::array_allocate(
            self.allocator,
            array_runtime::ArrayAllocationSlots { count: count_slot, bytes: size_slot, auxiliary, condition },
            self.failure_label,
        ) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(array_slot));
        // 逐字节写入 8 字节元素槽（常量长度编译期已知，直接展开）。
        for (index, byte) in bytes.iter().enumerate() {
            self.push(X64TextInstruction::LoadI64(i64::from(*byte)));
            self.push(X64TextInstruction::StoreSlot(condition));
            self.push(X64TextInstruction::LoadSlot(array_slot));
            self.push(X64TextInstruction::LoadSlotToRdx(condition));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(array_runtime::ARRAY_ELEMENT_OFFSET + 8 * index as i32));
        }
        // 对象只有一个字段：偏移 0 指向 `_repr` 数组（与 `StructNew` 的引用布局一致）。
        self.push(X64TextInstruction::LoadI64(array_runtime::ARRAY_ELEMENT_BYTES));
        for item in array_runtime::object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        // `rax` 是对象指针，`rcx` 装入数组指针后写入字段 0。
        self.push(X64TextInstruction::LoadSlotToRcx(array_slot));
        self.push(X64TextInstruction::StoreRcxToRax);
        Ok(())
    }

    fn emit_operand_to_rax(&mut self, operand: &Operand) -> Result<()> {
        match operand {
            Operand::Value(value) => {
                let local = self.value_local(*value)?;
                self.push(X64TextInstruction::LoadSlot(local));
                Ok(())
            }
            Operand::Constant(constant) => self.emit_constant_to_rax(constant),
            Operand::Item(instance) => {
                Err(format!("x86_64 lowering 不支持把可调用身份 `{instance:?}` 当值使用: function={}", self.function.symbol))
            }
            Operand::Symbol(path) => Err(format!("x86_64 lowering 不支持符号操作数 `{path:?}`: function={}", self.function.symbol)),
        }
    }

    /// 把操作数装入算子右操作数寄存器 `rcx`。
    fn emit_operand_to_rcx(&mut self, operand: &Operand) -> Result<()> {
        match operand {
            Operand::Value(value) => {
                let local = self.value_local(*value)?;
                self.push(X64TextInstruction::LoadSlotToRcx(local));
                Ok(())
            }
            Operand::Constant(constant) => {
                if !matches!(constant, Constant::Int(_) | Constant::Bool(_)) {
                    return Err(format!(
                        "x86_64 lowering 尚不支持常量 `{constant:?}` 作为算子实参：缺少 x86_64 常量载体合同，function={}",
                        self.function.symbol
                    ));
                }
                self.emit_constant_to_rax(constant)?;
                let base = self.reserve_temps(1);
                let slot = self.temp_slot(base, 0)?;
                self.push(X64TextInstruction::StoreSlot(slot));
                self.push(X64TextInstruction::LoadSlotToRcx(slot));
                Ok(())
            }
            Operand::Item(instance) => {
                Err(format!("x86_64 lowering 不支持把可调用身份 `{instance:?}` 当值使用: function={}", self.function.symbol))
            }
            Operand::Symbol(path) => Err(format!("x86_64 lowering 不支持符号操作数 `{path:?}`: function={}", self.function.symbol)),
        }
    }

    /// 把操作数装入数组写入使用的 `rdx`（值槽）。
    fn emit_operand_to_rdx(&mut self, operand: &Operand) -> Result<()> {
        match operand {
            Operand::Value(value) => {
                let local = self.value_local(*value)?;
                self.push(X64TextInstruction::LoadSlotToRdx(local));
                Ok(())
            }
            Operand::Constant(constant) => {
                // `Utf8` 常量物化为 `Utf8Text` 对象指针（8 字节槽载体），与值操作数同形。
                if !matches!(constant, Constant::Int(_) | Constant::Bool(_) | Constant::Utf8(_)) {
                    return Err(format!(
                        "x86_64 lowering 尚不支持常量 `{constant:?}` 作为数组元素：缺少 x86_64 常量载体合同，function={}",
                        self.function.symbol
                    ));
                }
                self.emit_constant_to_rax(constant)?;
                let base = self.reserve_temps(1);
                let slot = self.temp_slot(base, 0)?;
                self.push(X64TextInstruction::StoreSlot(slot));
                self.push(X64TextInstruction::LoadSlotToRdx(slot));
                Ok(())
            }
            Operand::Item(instance) => Err(format!(
                "x86_64 lowering 不支持把可调用身份 `{instance:?}` 当数组元素使用: function={}",
                self.function.symbol
            )),
            Operand::Symbol(path) => {
                Err(format!("x86_64 lowering 不支持符号操作数 `{path:?}` 作为数组元素: function={}", self.function.symbol))
            }
        }
    }

    /// 数组指令的操作数必须是堆数组 `[T]`。
    fn require_heap_array_operand(&self, operand: &Operand) -> Result<()> {
        let ty = self.operand_semantic_type(operand)?;
        if heap_array_element_type(&ty).is_none() {
            return Err(format!("x86_64 lowering：数组指令的操作数类型 `{ty}` 不是堆数组 `[T]`：function={}", self.function.symbol));
        }
        Ok(())
    }

    /// 字段的冻结归属：布局身份、物理偏移与槽宽。
    fn field_physical(&self, field: compiler_types::FieldId) -> Result<(compiler_types::LayoutId, u32, u32)> {
        let (layout_id, slot) = self
            .submission
            .backend_plan
            .aggregate_layout_by_field()
            .get(&field)
            .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 缺少冻结布局绑定：function={}", self.function.symbol))?;
        let layout = self
            .layouts
            .layout(*layout_id)
            .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 的布局 {layout_id:?} 缺少物理解析：function={}", self.function.symbol))?;
        let field_layout = layout
            .fields
            .get(*slot as usize)
            .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 的槽 {slot} 超出布局字段数：function={}", self.function.symbol))?;
        Ok((*layout_id, field_layout.offset, field_layout.size))
    }

    /// 引用存储聚合的物理布局；值存储聚合首期没有内联槽合同。
    fn reference_layout(
        &self,
        layout_id: compiler_types::LayoutId,
        operation: &str,
    ) -> Result<(u32, StorageKind, Vec<u32>)> {
        let layout = self.layouts.layout(layout_id).ok_or_else(|| {
            format!("x86_64 lowering：{operation} 的布局 {layout_id:?} 缺少物理解析：function={}", self.function.symbol)
        })?;
        if !layout.storage.is_reference() {
            return Err(format!(
                "x86_64 lowering：{operation} 首期只开放引用存储聚合（值存储需要内联槽合同）：layout={} function={}",
                layout.name, self.function.symbol
            ));
        }
        Ok((layout.size, layout.storage, layout.fields.iter().map(|field| field.offset).collect()))
    }

    /// 操作数静态类型解析出的冻结聚合身份（引用/值存储都可能是聚合）。
    fn operand_layout_id(&self, operand: &Operand) -> Result<Option<compiler_types::LayoutId>> {
        let ty = self.operand_semantic_type(operand)?;
        Ok(self.submission.backend_plan.aggregate_layouts().frozen_layout_id(&ty))
    }

    /// 冻结 sum 表示里的 variant tag；本 lane 物理 tag 固定为 4 字节槽。
    fn sum_variant_tag(
        &self,
        nominal: compiler_types::NominalInstanceId,
        variant: compiler_types::VariantId,
        operation: &str,
    ) -> Result<(u32, bool)> {
        let rep = self.submission.backend_plan.sum_representations().get(&nominal).ok_or_else(|| {
            format!("x86_64 lowering：{operation} 的 sum 实例 {nominal:?} 缺少冻结表示：function={}", self.function.symbol)
        })?;
        let variant_rep = rep.variants.get(&variant).ok_or_else(|| {
            format!("x86_64 lowering：{operation} 的 variant {variant:?} 不在 sum 表示内：function={}", self.function.symbol)
        })?;
        Ok((variant_rep.tag, variant_rep.payload_type.is_some()))
    }

    /// `StructNew`：按物理布局分配零初始化对象，再逐字段写入。
    fn emit_struct_new(
        &mut self,
        instruction: &MirInstruction,
        nominal: compiler_types::NominalInstanceId,
        fields: &[(compiler_types::FieldId, Operand)],
    ) -> Result<()> {
        let local = self.require_output_local(instruction)?;
        let layout_id = *self
            .submission
            .backend_plan
            .aggregate_layout_by_nominal()
            .get(&nominal)
            .ok_or_else(|| {
                format!("x86_64 lowering：`StructNew` 的名义实例 {nominal:?} 缺少冻结布局身份：function={}", self.function.symbol)
            })?;
        let (size, _, offsets) = self.reference_layout(layout_id, "`StructNew`")?;
        // 引用存储布局的 `size` 是「作为字段/句柄时占用的引用槽」，不是堆对象 body
        // 大小（共享解析合同，见 physical_layout 的自引用测试）。堆对象的实际字节数
        // 必须由字段物理区间的末端决定，否则多字段对象只分到 8 字节、字段写入即越界。
        let object_bytes = offsets.iter().copied().max().map(|offset| offset.max(8) + 8).unwrap_or(8);
        debug_assert!(object_bytes >= size || size == 8, "引用布局对象字节数不应小于引用槽");
        if offsets.iter().any(|offset| offset % 8 != 0) {
            return Err(format!(
                "x86_64 lowering：`StructNew` 布局 {layout_id:?} 含非 8 字节对齐字段，首期只开放 8 字节槽：function={}",
                self.function.symbol
            ));
        }
        if let Some(offset) = offsets.iter().find(|offset| **offset as u64 + 8 > object_bytes as u64) {
            return Err(format!(
                "x86_64 lowering：`StructNew` 布局 {layout_id:?} 的字段偏移 {offset} 越过对象大小 {object_bytes}：function={}",
                self.function.symbol
            ));
        }
        let mut slot_values: Vec<Option<Operand>> = vec![None; offsets.len()];
        for (field, value) in fields {
            let (owner, _, width) = self.field_physical(*field)?;
            if owner != layout_id {
                return Err(format!(
                    "x86_64 lowering：`StructNew` 字段 {field:?} 属于布局 {owner:?}，与构造布局 {layout_id:?} 冲突：function={}",
                    self.function.symbol
                ));
            }
            if width != 8 {
                return Err(format!(
                    "x86_64 lowering：`StructNew` 字段 {field:?} 宽 {width} 字节，首期只开放 8 字节槽字段：function={}",
                    self.function.symbol
                ));
            }
            let (_, layout_slot) = self
                .submission
                .backend_plan
                .aggregate_layout_by_field()
                .get(field)
                .copied()
                .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 缺少冻结槽位：function={}", self.function.symbol))?;
            let target = slot_values.get_mut(layout_slot as usize).ok_or_else(|| {
                format!(
                    "x86_64 lowering：`StructNew` 字段 {field:?} 的槽 {layout_slot} 越界（布局 {layout_id:?} 有 {} 个字段）：function={}",
                    offsets.len(),
                    self.function.symbol
                )
            })?;
            if target.replace(value.clone()).is_some() {
                return Err(format!("x86_64 lowering：`StructNew` 字段 {field:?} 重复初始化：function={}", self.function.symbol));
            }
        }
        if let Some(index) = slot_values.iter().position(Option::is_none) {
            return Err(format!(
                "x86_64 lowering：`StructNew` 缺少字段槽 {index} 的初始化值（不静默补默认值）：layout={layout_id:?} function={}",
                self.function.symbol
            ));
        }
        let base = self.reserve_temps(4);
        let pointer_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        self.push(X64TextInstruction::LoadI64(object_bytes as i64));
        self.uses_failure_exit = true;
        for item in array_runtime::object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        for (index, value) in slot_values.iter().enumerate() {
            let value = value.as_ref().expect("字段完整性已校验");
            self.emit_operand_to_rdx(value)?;
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(offsets[index] as i32));
        }
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `FieldGet`：按冻结字段偏移读取一个 8 字节槽。
    fn emit_field_get(&mut self, instruction: &MirInstruction, object: &Operand, field: compiler_types::FieldId) -> Result<()> {
        let local = self.require_output_local(instruction)?;
        let (layout_id, offset, width) = self.field_physical(field)?;
        self.reference_layout(layout_id, "`FieldGet`")?;
        if width != 8 {
            return Err(format!(
                "x86_64 lowering：`FieldGet` 字段 {field:?} 宽 {width} 字节，首期只开放 8 字节槽字段：function={}",
                self.function.symbol
            ));
        }
        if let Some(object_layout) = self.operand_layout_id(object)?
            && object_layout != layout_id
        {
            return Err(format!(
                "x86_64 lowering：`FieldGet` 字段 {field:?} 属于布局 {layout_id:?}，对象布局却是 {object_layout:?}：function={}",
                self.function.symbol
            ));
        }
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadFromRaxDisplacement(offset as i32));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `FieldSet`：按冻结字段偏移写入一个 8 字节槽（`mut self` 修改同一实例）。
    fn emit_field_set(
        &mut self,
        instruction: &MirInstruction,
        object: &Operand,
        field: compiler_types::FieldId,
        value: &Operand,
    ) -> Result<()> {
        if self.output_local(instruction)?.is_some() {
            return Err(format!("x86_64 lowering：`FieldSet` 不得绑定结果槽：function={}", self.function.symbol));
        }
        let (layout_id, offset, width) = self.field_physical(field)?;
        self.reference_layout(layout_id, "`FieldSet`")?;
        if width != 8 {
            return Err(format!(
                "x86_64 lowering：`FieldSet` 字段 {field:?} 宽 {width} 字节，首期只开放 8 字节槽字段：function={}",
                self.function.symbol
            ));
        }
        if let Some(object_layout) = self.operand_layout_id(object)?
            && object_layout != layout_id
        {
            return Err(format!(
                "x86_64 lowering：`FieldSet` 字段 {field:?} 属于布局 {layout_id:?}，对象布局却是 {object_layout:?}：function={}",
                self.function.symbol
            ));
        }
        self.emit_operand_to_rdx(value)?;
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::StoreRdxToRaxDisplacement(offset as i32));
        Ok(())
    }

    /// 一个元组元素槽的物理合同：偏移 8 字节对齐、与下一元素不重叠、
    /// 载体宽度与类型一致。
    ///
    /// 元组 SSA 值的载体是堆对象指针（8 字节），元素一律按 8 字节槽存取：
    /// 4 字节标量（`bool`/`char`/窄整数）在槽内按本 lane 的帧槽表示加宽，
    /// 高位置零由分配实现保证。浮点元素（载体寄存器类不同）与值存储聚合
    /// （没有内联槽合同）首期不开放，调用点结构化失败。
    fn tuple_field_offset(&self, layout_id: compiler_types::LayoutId, index: usize, operation: &str) -> Result<u32> {
        let layout = self.layouts.layout(layout_id).ok_or_else(|| {
            format!("x86_64 lowering：{operation} 的布局 {layout_id:?} 缺少物理解析：function={}", self.function.symbol)
        })?;
        let field = layout.fields.get(index).ok_or_else(|| {
            format!(
                "x86_64 lowering：{operation} 元素槽 {index} 超出布局字段数 {}：function={}",
                layout.fields.len(),
                self.function.symbol
            )
        })?;
        if field.offset % 8 != 0 {
            return Err(format!(
                "x86_64 lowering：{operation} 元素 {index} 偏移 {} 不是 8 字节对齐：function={}",
                field.offset, self.function.symbol
            ));
        }
        if let Some(next) = layout.fields.get(index + 1) && next.offset < field.offset + 8 {
            return Err(format!(
                "x86_64 lowering：{operation} 元素 {index} 的 8 字节槽与下一元素重叠（offset={} next={}）：function={}",
                field.offset,
                next.offset,
                self.function.symbol
            ));
        }
        let width_ok = match &field.ty {
            SemanticType::Boolean | SemanticType::Character | SemanticType::Integer8 { .. } | SemanticType::Integer16 { .. } | SemanticType::Integer32 { .. } => field.size == 4,
            SemanticType::Integer64 { .. }
            | SemanticType::Array(_)
            | SemanticType::Utf8
            | SemanticType::Utf16
            | SemanticType::Union(_)
            | SemanticType::Nullable(_)
            | SemanticType::Nominal { .. }
            | SemanticType::Apply(..) => field.size == 8,
            other => {
                return Err(format!(
                    "x86_64 lowering：{operation} 元素 {index} 类型 `{other}` 没有本 lane 开放的元组槽合同：function={}",
                    self.function.symbol
                ));
            }
        };
        if !width_ok {
            return Err(format!(
                "x86_64 lowering：{operation} 元素 {index} 宽度 {} 与类型载体不符：function={}",
                field.size, self.function.symbol
            ));
        }
        Ok(field.offset)
    }

    /// `TupleNew`：分配零初始化的元组堆对象，按冻结偏移逐元素写入 8 字节槽。
    fn emit_tuple_new(&mut self, instruction: &MirInstruction, layout_id: compiler_types::LayoutId, fields: &[Operand]) -> Result<()> {
        let local = self.require_output_local(instruction)?;
        let field_count = self
            .layouts
            .layout(layout_id)
            .ok_or_else(|| format!("x86_64 lowering：`TupleNew` 的布局 {layout_id:?} 缺少物理解析：function={}", self.function.symbol))?
            .fields
            .len();
        if field_count != fields.len() {
            return Err(format!(
                "x86_64 lowering：`TupleNew` 元素个数 {} 与布局字段数 {field_count} 不符：function={}",
                fields.len(),
                self.function.symbol
            ));
        }
        let mut offsets = Vec::with_capacity(fields.len());
        for index in 0..fields.len() {
            offsets.push(self.tuple_field_offset(layout_id, index, "`TupleNew`")?);
        }
        let object_bytes = offsets.iter().copied().max().map(|offset| offset + 8).unwrap_or(8);
        let base = self.reserve_temps(4);
        let pointer_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        self.push(X64TextInstruction::LoadI64(object_bytes as i64));
        self.uses_failure_exit = true;
        for item in array_runtime::object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        for (index, value) in fields.iter().enumerate() {
            self.emit_operand_to_rdx(value)?;
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(offsets[index] as i32));
        }
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `TupleGet`：按冻结偏移读取一个 8 字节元素槽。
    fn emit_tuple_get(&mut self, instruction: &MirInstruction, object: &Operand, layout_id: compiler_types::LayoutId, index: u32) -> Result<()> {
        let local = self.require_output_local(instruction)?;
        let offset = self.tuple_field_offset(layout_id, index as usize, "`TupleGet`")?;
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadFromRaxDisplacement(offset as i32));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `AggregateCopy`：按冻结布局把聚合逐 8 字节槽复制进一个新堆对象。
    ///
    /// 值复制语义：结果指针指向新对象，与源不共享可变存储；元素槽合同与
    /// `tuple_field_offset` 一致（8 字节对齐、不重叠、宽度匹配），不满足的
    /// 布局（浮点、值存储内联体）结构化失败，不做按位浅拷贝冒充。
    fn emit_aggregate_copy(&mut self, instruction: &MirInstruction, layout_id: compiler_types::LayoutId, source: &Operand, dest: &Operand) -> Result<()> {
        // 目标槽与结果槽至少有一个：两者都绑定时写入同一个新对象指针（与 OVM 一致）。
        let output_local = self.output_local(instruction)?;
        let dest_local = match dest {
            Operand::Value(value) => Some(self.value_local(*value)?),
            Operand::Constant(_) | Operand::Item(_) | Operand::Symbol(_) => None,
        };
        if output_local.is_none() && dest_local.is_none() {
            return Err(format!("x86_64 lowering：`AggregateCopy` 没有可写目标（目标与结果槽均缺失）：function={}", self.function.symbol));
        }
        let field_count = self
            .layouts
            .layout(layout_id)
            .ok_or_else(|| format!("x86_64 lowering：`AggregateCopy` 的布局 {layout_id:?} 缺少物理解析：function={}", self.function.symbol))?
            .fields
            .len();
        let mut offsets = Vec::with_capacity(field_count);
        for index in 0..field_count {
            offsets.push(self.tuple_field_offset(layout_id, index, "`AggregateCopy`")?);
        }
        let object_bytes = offsets.iter().copied().max().map(|offset| offset + 8).unwrap_or(8);
        let base = self.reserve_temps(6);
        let source_slot = self.temp_slot(base, 0)?;
        let pointer_slot = self.temp_slot(base, 1)?;
        let size_slot = self.temp_slot(base, 2)?;
        let auxiliary = self.temp_slot(base, 3)?;
        let condition = self.temp_slot(base, 4)?;
        let field_slot = self.temp_slot(base, 5)?;
        self.emit_operand_to_rax(source)?;
        self.push(X64TextInstruction::StoreSlot(source_slot));
        self.push(X64TextInstruction::LoadI64(object_bytes as i64));
        self.uses_failure_exit = true;
        for item in array_runtime::object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        for offset in &offsets {
            self.push(X64TextInstruction::LoadSlot(source_slot));
            self.push(X64TextInstruction::LoadFromRaxDisplacement(*offset as i32));
            self.push(X64TextInstruction::StoreSlot(field_slot));
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::LoadSlotToRdx(field_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(*offset as i32));
        }
        for local in dest_local.into_iter().chain(output_local) {
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreSlot(local));
        }
        Ok(())
    }

    /// `SumNew`：分配 `{u32 tag, i64 payload}` 载体并写入 variant。
    fn emit_sum_new(
        &mut self,
        instruction: &MirInstruction,
        nominal: compiler_types::NominalInstanceId,
        variant: compiler_types::VariantId,
        payload: Option<&Operand>,
    ) -> Result<()> {
        let local = self.require_output_local(instruction)?;
        let (tag, has_payload) = self.sum_variant_tag(nominal, variant, "`SumNew`")?;
        if payload.is_some() != has_payload {
            return Err(format!(
                "x86_64 lowering：`SumNew` variant {variant:?} 的载荷事实与冻结表示不一致（payload={} expected={has_payload}）：function={}",
                payload.is_some(),
                self.function.symbol
            ));
        }
        let base = self.reserve_temps(4);
        let pointer_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        self.push(X64TextInstruction::LoadI64(array_runtime::SUM_CARRIER_BYTES));
        self.uses_failure_exit = true;
        for item in array_runtime::object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        self.push(X64TextInstruction::LoadI64(tag as i64));
        self.push(X64TextInstruction::StoreSlot(condition));
        self.push(X64TextInstruction::LoadSlotToRcx(condition));
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreDwordRcxToRax);
        if let Some(payload) = payload {
            self.emit_operand_to_rdx(payload)?;
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(array_runtime::SUM_PAYLOAD_OFFSET));
        }
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `SumVariantIs`：比较当前 tag 与冻结 discriminant。
    fn emit_sum_variant_is(
        &mut self,
        instruction: &MirInstruction,
        nominal: compiler_types::NominalInstanceId,
        variant: compiler_types::VariantId,
        object: &Operand,
    ) -> Result<()> {
        let local = self.require_output_local(instruction)?;
        let (tag, _) = self.sum_variant_tag(nominal, variant, "`SumVariantIs`")?;
        let base = self.reserve_temps(2);
        let value_slot = self.temp_slot(base, 0)?;
        let expected_slot = self.temp_slot(base, 1)?;
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadDwordFromRax);
        self.push(X64TextInstruction::StoreSlot(value_slot));
        self.push(X64TextInstruction::LoadI64(tag as i64));
        self.push(X64TextInstruction::StoreSlot(expected_slot));
        self.push(X64TextInstruction::LoadSlot(value_slot));
        self.push(X64TextInstruction::LoadSlotToRcx(expected_slot));
        self.push(X64TextInstruction::Operator(X86_64OperatorCode::I64CompareEqual));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `SumPayloadGet`：按冻结偏移读取载荷。
    fn emit_sum_payload_get(
        &mut self,
        instruction: &MirInstruction,
        nominal: compiler_types::NominalInstanceId,
        variant: compiler_types::VariantId,
        object: &Operand,
    ) -> Result<()> {
        let local = self.require_output_local(instruction)?;
        let (_, has_payload) = self.sum_variant_tag(nominal, variant, "`SumPayloadGet`")?;
        if !has_payload {
            return Err(format!(
                "x86_64 lowering：`SumPayloadGet` 的 variant {variant:?} 没有载荷：function={}",
                self.function.symbol
            ));
        }
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadFromRaxDisplacement(array_runtime::SUM_PAYLOAD_OFFSET));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// 分配 `length` 个元素槽；`rax` 返回数组指针。
    ///
    /// 字节数换算、分配实现、失败出口与头部写入全部来自 [`array_runtime`]：
    /// guest lowering 不再自己算字节数、写数组头或直接调用 `HeapAlloc`。
    /// 元素个数先求值进 `rax`；容量乘加溢出与分配失败都跳到本函数的定义失败出口。
    fn emit_array_allocation(&mut self, length: &Operand) -> Result<()> {
        let base = self.reserve_temps(4);
        let slots = ArrayAllocationSlots {
            count: self.temp_slot(base, 0)?,
            bytes: self.temp_slot(base, 1)?,
            auxiliary: self.temp_slot(base, 2)?,
            condition: self.temp_slot(base, 3)?,
        };
        self.emit_operand_to_rax(length)?;
        self.uses_failure_exit = true;
        for instruction in array_runtime::array_allocate(self.allocator, slots, self.failure_label) {
            self.push(instruction);
        }
        Ok(())
    }

    /// `array.get` / `array.set` 的唯一定义边界检查：下标必须落在 `[0, [0])`。
    ///
    /// 这是两个访问入口（MIR `ArrayGet/Set` 与 provider `array.get/set`）共同经过的
    /// 检查；失败跳到本函数的数组失败出口，不把越界访问留成后续内存故障。
    fn emit_array_bounds_check(&mut self, array: &Operand, index: &Operand) -> Result<()> {
        let base = self.reserve_temps(3);
        let array_slot = self.temp_slot(base, 0)?;
        let index_slot = self.temp_slot(base, 1)?;
        let condition = self.temp_slot(base, 2)?;
        self.emit_operand_to_rax(array)?;
        self.push(X64TextInstruction::StoreSlot(array_slot));
        self.emit_operand_to_rax(index)?;
        self.push(X64TextInstruction::StoreSlot(index_slot));
        self.uses_failure_exit = true;
        for instruction in array_runtime::array_bounds_check(array_slot, index_slot, condition, self.failure_label) {
            self.push(instruction);
        }
        Ok(())
    }

    /// 数组 operator 调用的机械发射（`[host_provider]` 冻结的 `array.*`）。
    fn emit_array_operator_call(
        &mut self,
        instruction: &MirInstruction,
        plan: X64ArrayOperatorPlan,
        arguments: &[Operand],
    ) -> Result<()> {
        if arguments.len() != plan.arity {
            return Err(format!(
                "x86_64 数组 operator 实参数量与冻结 arity 不符: function={} expected={} actual={}",
                self.function.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        match plan.operation {
            X64ArrayOperation::New => {
                let local = self.require_output_local(instruction)?;
                self.emit_array_allocation(&arguments[0])?;
                self.push(X64TextInstruction::StoreSlot(local));
            }
            X64ArrayOperation::Length => {
                let local = self.require_output_local(instruction)?;
                self.emit_operand_to_rax(&arguments[0])?;
                self.push(X64TextInstruction::LoadFromRax);
                self.push(X64TextInstruction::StoreSlot(local));
            }
            X64ArrayOperation::Get => {
                let local = self.require_output_local(instruction)?;
                self.emit_array_bounds_check(&arguments[0], &arguments[1])?;
                self.emit_operand_to_rcx(&arguments[1])?;
                self.emit_operand_to_rax(&arguments[0])?;
                self.push(X64TextInstruction::LoadIndexedFromRaxRcx);
                self.push(X64TextInstruction::StoreSlot(local));
            }
            X64ArrayOperation::Set => {
                if self.output_local(instruction)?.is_some() {
                    return Err(format!("x86_64 lowering：数组 `set` 不得绑定结果槽：function={}", self.function.symbol));
                }
                self.emit_array_bounds_check(&arguments[0], &arguments[1])?;
                self.emit_operand_to_rcx(&arguments[1])?;
                self.emit_operand_to_rdx(&arguments[2])?;
                self.emit_operand_to_rax(&arguments[0])?;
                self.push(X64TextInstruction::StoreIndexedRdxToRaxRcx);
            }
        }
        Ok(())
    }

    /// 发射控制台 operator：把 `Utf8Text` 对象的 `_repr` 数组 gather 成连续 UTF-8
    /// 字节后按目标平台的句柄通道写出。
    ///
    /// staging 副本复用数组运行时的分配/释放合同（同一分配器、同一定义失败出口），
    /// 写出后立即释放，有效期就是这一次调用。写出本身是尽力而为（unit 返回、无错误
    /// 通道），但分配失败仍走定义好的失败出口，不拿后续故障充当错误。
    fn emit_console_write(
        &mut self,
        instruction: &MirInstruction,
        operation: X64ConsoleOperation,
        arguments: &[Operand],
    ) -> Result<()> {
        match operation {
            X64ConsoleOperation::Write => {}
        }
        if arguments.len() != 2 {
            return Err(format!(
                "x86_64 控制台 operator 实参数量与冻结 arity 不符: function={} expected=2 actual={}",
                self.function.symbol,
                arguments.len()
            ));
        }
        if self.output_local(instruction)?.is_some() {
            return Err(format!("x86_64 lowering：控制台 `write` 不得绑定结果槽：function={}", self.function.symbol));
        }
        let base = self.reserve_temps(13);
        let handle_slot = self.temp_slot(base, 0)?;
        let obj_slot = self.temp_slot(base, 1)?;
        let array_slot = self.temp_slot(base, 2)?;
        let count_slot = self.temp_slot(base, 3)?;
        let bytes_slot = self.temp_slot(base, 4)?;
        let aux_slot = self.temp_slot(base, 5)?;
        let cond_slot = self.temp_slot(base, 6)?;
        let buf_slot = self.temp_slot(base, 7)?;
        let index_slot = self.temp_slot(base, 8)?;
        let cursor_slot = self.temp_slot(base, 9)?;
        let byte_slot = self.temp_slot(base, 10)?;
        let written_slot = self.temp_slot(base, 11)?;
        let zero_slot = self.temp_slot(base, 12)?;
        let slots = array_runtime::ArrayAllocationSlots { count: count_slot, bytes: bytes_slot, auxiliary: aux_slot, condition: cond_slot };
        // 物化句柄与消息对象指针。
        self.emit_operand_to_rax(&arguments[0])?;
        self.push(X64TextInstruction::StoreSlot(handle_slot));
        self.emit_operand_to_rax(&arguments[1])?;
        self.push(X64TextInstruction::StoreSlot(obj_slot));
        // `Utf8Text` 对象字段 0 是 `_repr` 数组，数组头部第 0 格是字节个数。
        self.push(X64TextInstruction::LoadSlot(obj_slot));
        self.push(X64TextInstruction::LoadFromRax);
        self.push(X64TextInstruction::StoreSlot(array_slot));
        self.push(X64TextInstruction::LoadSlot(array_slot));
        self.push(X64TextInstruction::LoadFromRax);
        self.push(X64TextInstruction::StoreSlot(count_slot));
        // staging：按同一数组合同分配 `count` 个元素槽（每槽只用低字节，连续写出）。
        self.push(X64TextInstruction::LoadSlot(count_slot));
        self.uses_failure_exit = true;
        for item in array_runtime::array_allocate(self.allocator, slots, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(buf_slot));
        // gather：`[array + 16 + 8*i]` 的低字节写入 `[buf + 16 + i]`。
        let loop_label = self.fresh_label();
        let end_label = self.fresh_label();
        self.push(X64TextInstruction::LoadI64(0));
        self.push(X64TextInstruction::StoreSlot(index_slot));
        self.push(X64TextInstruction::LoadSlot(buf_slot));
        self.push(X64TextInstruction::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET));
        self.push(X64TextInstruction::StoreSlot(cursor_slot));
        self.push(X64TextInstruction::Label(loop_label));
        self.push(X64TextInstruction::LoadSlot(index_slot));
        self.push(X64TextInstruction::LoadSlotToRcx(count_slot));
        self.push(X64TextInstruction::Operator(X86_64OperatorCode::I64CompareLessThan));
        self.push(X64TextInstruction::JumpIfZero(end_label));
        self.push(X64TextInstruction::LoadSlot(array_slot));
        self.push(X64TextInstruction::LoadSlotToRcx(index_slot));
        self.push(X64TextInstruction::LoadIndexedFromRaxRcx);
        self.push(X64TextInstruction::StoreSlot(byte_slot));
        self.push(X64TextInstruction::LoadSlot(cursor_slot));
        self.push(X64TextInstruction::LoadSlotToRdx(byte_slot));
        self.push(X64TextInstruction::StoreByteAtRaxFromRdx);
        self.push(X64TextInstruction::LoadSlot(cursor_slot));
        self.push(X64TextInstruction::AddRaxImmediate(1));
        self.push(X64TextInstruction::StoreSlot(cursor_slot));
        self.push(X64TextInstruction::LoadSlot(index_slot));
        self.push(X64TextInstruction::AddRaxImmediate(1));
        self.push(X64TextInstruction::StoreSlot(index_slot));
        self.push(X64TextInstruction::Jump(loop_label));
        self.push(X64TextInstruction::Label(end_label));
        match self.allocator {
            array_runtime::ArrayAllocator::WindowsProcessHeap => {
                // `SetConsoleOutputCP(CP_UTF8)`：控制台句柄按 UTF-8 呈现；重定向管道不受影响。
                self.push(X64TextInstruction::LoadI64(65001));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 0 });
                self.push(X64TextInstruction::CallImport { module: "kernel32.dll".to_string(), field: "SetConsoleOutputCP".to_string() });
                // `GetStdHandle(nStdHandle)`：-11 标准输出 / -12 标准错误伪句柄。
                self.push(X64TextInstruction::LoadSlotToArg { slot: handle_slot, index: 0 });
                self.push(X64TextInstruction::CallImport { module: "kernel32.dll".to_string(), field: "GetStdHandle".to_string() });
                self.push(X64TextInstruction::StoreSlot(handle_slot));
                // `WriteFile(hFile, buf, count, &written, NULL)`：对管道与控制台句柄统一按 UTF-8 字节写出。
                self.push(X64TextInstruction::LoadSlotToArg { slot: handle_slot, index: 0 });
                self.push(X64TextInstruction::LoadSlot(buf_slot));
                self.push(X64TextInstruction::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 1 });
                self.push(X64TextInstruction::LoadSlotToArg { slot: count_slot, index: 2 });
                self.push(X64TextInstruction::LoadSlotAddress(written_slot));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 3 });
                self.push(X64TextInstruction::ZeroStackArg(0));
                self.push(X64TextInstruction::CallImport { module: "kernel32.dll".to_string(), field: "WriteFile".to_string() });
            }
            array_runtime::ArrayAllocator::LinuxMmap => {
                // `write(fd, buf, count)`：句柄即文件描述符（1 标准输出 / 2 标准错误）。
                self.push(X64TextInstruction::LoadSlotToArg { slot: handle_slot, index: 0 });
                self.push(X64TextInstruction::LoadSlot(buf_slot));
                self.push(X64TextInstruction::AddRaxImmediate(array_runtime::ARRAY_ELEMENT_OFFSET));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 1 });
                self.push(X64TextInstruction::LoadSlotToArg { slot: count_slot, index: 2 });
                self.push(X64TextInstruction::SyscallWrite);
            }
        }
        // staging 是一次性副本：写出后立即释放。
        self.push(X64TextInstruction::LoadI64(0));
        self.push(X64TextInstruction::StoreSlot(zero_slot));
        self.push(X64TextInstruction::LoadSlot(buf_slot));
        for item in array_runtime::array_free(self.allocator, slots, zero_slot) {
            self.push(item);
        }
        Ok(())
    }

    /// 操作数在计划中冻结的语义类型。
    ///
    /// `Cast` 的转换语义必须由此决定：`Integer32{signed}` 与 `Integer32{signed:false}`
    /// 占用同一 8 字节帧槽，只有冻结类型能区分符号扩展方向。
    fn operand_semantic_type(&self, operand: &Operand) -> Result<SemanticType> {
        match operand {
            Operand::Value(value) => self
                .function
                .value_types
                .get(value)
                .cloned()
                .ok_or_else(|| format!("x86_64 `Cast` 源 SSA 值 {value:?} 缺少冻结语义类型：function={}", self.function.symbol)),
            Operand::Constant(constant) => match constant {
                Constant::Int(_) => Ok(SemanticType::Integer64 { signed: true }),
                Constant::Bool(_) => Ok(SemanticType::Boolean),
                Constant::Float64(_) => Ok(SemanticType::Float64),
                Constant::Utf8(_) => Ok(SemanticType::Utf8),
                Constant::Utf16(_) => Ok(SemanticType::Utf16),
                Constant::Unit => Ok(SemanticType::Unit),
                Constant::Null => Err(format!("x86_64 `Cast` 的空值操作数没有源类型，无法确定转换语义：function={}", self.function.symbol)),
            },
            Operand::Symbol(path) => Err(format!("x86_64 `Cast` 源操作数不支持符号引用 {path:?}：function={}", self.function.symbol)),
            Operand::Item(instance) => {
                Err(format!("x86_64 `Cast` 源操作数不支持可调用身份 {instance:?}：function={}", self.function.symbol))
            }
        }
    }

    /// 语言层 `as` 的表示转换（整数走 `rax`，浮点走 `xmm0`，结果回写 64 位帧槽）。
    ///
    /// 帧槽不保证处于规范形态：常量以完整 `i64` 落入，32 位运算零扩展结果。
    /// 因此这里**不假设**高位正确，而是显式「按源位宽/符号性规范化 → 按目标位宽/符号性收窄」。
    /// 浮点参与走 [`Self::emit_float_cast_conversion`] 的 SSE2 单指令转换。
    fn emit_cast_conversion(&mut self, source: &SemanticType, target: &SemanticType) -> Result<()> {
        if source == target {
            return Ok(());
        }
        if source.is_float() || target.is_float() {
            return self.emit_float_cast_conversion(source, target);
        }
        if matches!(target, SemanticType::Boolean) {
            self.push(X64TextInstruction::ToBool);
            return Ok(());
        }
        let (Some(source_shape), Some(target_shape)) = (x64_int_shape(source), x64_int_shape(target))
        else {
            return Err(format!(
                "x86_64 后端没有 `{source}` → `{target}` 的确定转换语义：布尔/文本与数值之间的 `as` 缺少目标合同，function={}",
                self.function.symbol
            ));
        };
        if source_shape == target_shape {
            return Ok(());
        }
        // 源规范化：窄整数按源符号性符号/零扩展到 64 位；64 位源已是完整值。
        if source_shape.bits < 64 {
            let step = narrow_normalize(source_shape.bits, source_shape.signed).ok_or_else(|| {
                format!("x86_64 后端没有位宽 {} 的整数载体：function={}", source_shape.bits, self.function.symbol)
            })?;
            self.push(step);
        }
        // 目标表示：窄目标按目标符号性截断并规范化；64 位目标保留当前 64 位值。
        if target_shape.bits < 64 {
            let step = narrow_normalize(target_shape.bits, target_shape.signed).ok_or_else(|| {
                format!("x86_64 后端没有位宽 {} 的整数载体：function={}", target_shape.bits, self.function.symbol)
            })?;
            self.push(step);
        }
        Ok(())
    }

    /// 浮点参与的 `as`：f32/f64 互转、整数 ↔ 浮点与显式截断语义。
    ///
    /// 整数侧先按自身符号性规范化到 64 位再转换；`u64` 与浮点互转缺少单指令合同
    /// （`cvtsi2sd` 只按有符号解释），结构化失败，不用错误符号性冒充。
    fn emit_float_cast_conversion(&mut self, source: &SemanticType, target: &SemanticType) -> Result<()> {
        let source_float = matches!(source, SemanticType::Float32 | SemanticType::Float64);
        let target_float = matches!(target, SemanticType::Float32 | SemanticType::Float64);
        let target_single = matches!(target, SemanticType::Float32);
        if source_float {
            // 源位模式已由 `emit_operand_to_rax` 搬进 `rax`；SSE 转换统一从 `xmm0` 取源。
            self.push(X64TextInstruction::LoadRaxToXmm0 { single: matches!(source, SemanticType::Float32) });
        }
        if source_float && target_float {
            return match (source, target) {
                (SemanticType::Float32, SemanticType::Float64) => {
                    self.push(X64TextInstruction::CastFloat(X64FloatCast::F32ToF64));
                    Ok(())
                }
                (SemanticType::Float64, SemanticType::Float32) => {
                    self.push(X64TextInstruction::CastFloat(X64FloatCast::F64ToF32));
                    Ok(())
                }
                _ => Ok(()),
            };
        }
        if source_float {
            let target_shape = x64_int_shape(target).ok_or_else(|| {
                format!(
                    "x86_64 后端没有 `{source}` → `{target}` 的确定转换语义：浮点只转换为整数族，function={}",
                    self.function.symbol
                )
            })?;
            if target_shape.bits == 64 && !target_shape.signed {
                return Err(format!(
                    "x86_64 后端不支持 `{source}` → `{target}`：`cvttsd2si` 只产生有符号 64 位值，`u64` 目标缺少饱和/检查合同，function={}",
                    self.function.symbol
                ));
            }
            // 越界与 NaN 走定义好的失败出口：`cvttsi2sd` 族的非值（INT64_MIN）不得
            // 冒充合法转换结果。边界都是 2 的幂，在 `f32`/`f64` 下精确可表示。
            let single = matches!(source, SemanticType::Float32);
            let base = self.reserve_temps(2);
            let src_slot = self.temp_slot(base, 0)?;
            let bound_slot = self.temp_slot(base, 1)?;
            // `rax` 仍持有源位模式（`LoadRaxToXmm0` 不破坏它），先落槽供逐次比较。
            self.push(X64TextInstruction::StoreSlot(src_slot));
            let failure = self.failure_label;
            self.uses_failure_exit = true;
            let (lower, upper) = float_int_target_bounds(&target_shape);
            let encode_bound = |value: f64| -> i64 {
                if single { u64::from((value as f32).to_bits()) as i64 } else { value.to_bits() as i64 }
            };
            let (ne_code, lt_code, ge_code) = if single {
                (X86_64OperatorCode::F32CompareNotEqual, X86_64OperatorCode::F32CompareLessThan, X86_64OperatorCode::F32CompareGreaterOrEqual)
            } else {
                (X86_64OperatorCode::F64CompareNotEqual, X86_64OperatorCode::F64CompareLessThan, X86_64OperatorCode::F64CompareGreaterOrEqual)
            };
            // NaN：`x != x` 为真。
            let not_nan_label = self.fresh_label();
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::LoadSlotToXmm1 { slot: src_slot, single });
            self.push(X64TextInstruction::Operator(ne_code));
            self.push(X64TextInstruction::JumpIfZero(not_nan_label));
            self.push(X64TextInstruction::Jump(failure));
            self.push(X64TextInstruction::Label(not_nan_label));
            // 下界：`x < lower` 越界。
            let above_lower_label = self.fresh_label();
            self.push(X64TextInstruction::LoadI64(encode_bound(lower)));
            self.push(X64TextInstruction::StoreSlot(bound_slot));
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::LoadSlotToXmm1 { slot: bound_slot, single });
            self.push(X64TextInstruction::Operator(lt_code));
            self.push(X64TextInstruction::JumpIfZero(above_lower_label));
            self.push(X64TextInstruction::Jump(failure));
            self.push(X64TextInstruction::Label(above_lower_label));
            // 上界：`x >= upper` 越界（上界为排他边界）。
            let below_upper_label = self.fresh_label();
            self.push(X64TextInstruction::LoadI64(encode_bound(upper)));
            self.push(X64TextInstruction::StoreSlot(bound_slot));
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::LoadSlotToXmm1 { slot: bound_slot, single });
            self.push(X64TextInstruction::Operator(ge_code));
            self.push(X64TextInstruction::JumpIfZero(below_upper_label));
            self.push(X64TextInstruction::Jump(failure));
            self.push(X64TextInstruction::Label(below_upper_label));
            // 源重新装入 `xmm0` 后按截断转换。
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::CastFloat(if single { X64FloatCast::F32ToI64 } else { X64FloatCast::F64ToI64 }));
            if target_shape.bits < 64 {
                let step = narrow_normalize(target_shape.bits, target_shape.signed).ok_or_else(|| {
                    format!("x86_64 后端没有位宽 {} 的整数载体：function={}", target_shape.bits, self.function.symbol)
                })?;
                self.push(step);
            }
            return Ok(());
        }
        let source_shape = x64_int_shape(source).ok_or_else(|| {
            format!(
                "x86_64 后端没有 `{source}` → `{target}` 的确定转换语义：浮点目标只接受整数族源，function={}",
                self.function.symbol
            )
        })?;
        if source_shape.bits == 64 && !source_shape.signed {
            return Err(format!(
                "x86_64 后端不支持 `{source}` → `{target}`：`cvtsi2sd` 只按有符号解释 `rax`，`u64` 源缺少无符号转换合同，function={}",
                self.function.symbol
            ));
        }
        if source_shape.bits < 64 {
            let step = narrow_normalize(source_shape.bits, source_shape.signed).ok_or_else(|| {
                format!("x86_64 后端没有位宽 {} 的整数载体：function={}", source_shape.bits, self.function.symbol)
            })?;
            self.push(step);
        }
        self.push(X64TextInstruction::CastFloat(if target_single { X64FloatCast::I64ToF32 } else { X64FloatCast::I64ToF64 }));
        Ok(())
    }

    fn output_local(&self, instruction: &MirInstruction) -> Result<Option<u16>> {
        match self.slots.instruction_output(instruction) {
            None => Ok(None),
            Some(value) => self.value_local(value).map(Some),
        }
    }

    fn require_output_local(&self, instruction: &MirInstruction) -> Result<u16> {
        self.output_local(instruction)?.ok_or_else(|| format!("x86_64 lowering：指令缺少 SSA 结果槽: function={}", self.function.symbol))
    }

    fn value_local(&self, value: MirValueRef) -> Result<u16> {
        let local = *self
            .slots
            .value_locals
            .get(&value)
            .ok_or_else(|| format!("x86_64 lowering：SSA 值 {:?} 缺少规划的局部槽: function={}", value, self.function.symbol))?;
        if let Some(ty) = self.slots.local_types.get(local as usize)
            && !self.is_local_carrier(ty)
        {
            return Err(format!("x86_64 lowering：SSA 值 {:?} 的类型 `{ty}` 没有 x86_64 槽载体: function={}", value, self.function.symbol));
        }
        Ok(local)
    }

    /// 帧槽可承载的类型。
    ///
    /// 在标量/数组/文本之外，x86-64 引用对象、unite（sum）与引用可空都是 8 字节
    /// 指针载体；值存储聚合没有内联槽合同，仍在这里结构化失败。
    fn is_local_carrier(&self, ty: &SemanticType) -> bool {
        is_local_carrier_type(ty) || is_reference_carrier(ty, &self.submission.backend_plan)
    }
}

/// 8 字节指针载体：引用对象、unite（sum）、引用可空、元组与匿名 union。
///
/// 与 [`is_local_carrier_type`] 合起来定义 native 槽载体全集。库导出清单的
/// C ABI 载体也从这一处派生，禁止清单另写一套「什么能过界」的判定。
pub(super) fn is_reference_carrier(ty: &SemanticType, plan: &crate::TargetExecutionPlan) -> bool {
    match ty {
        SemanticType::Nominal { .. } | SemanticType::Apply(..) => {
            if let Some(layout) = plan.aggregate_layouts().frozen_layout(ty) {
                return layout.storage.is_reference();
            }
            // 闭合 unite 实例：物理载体是 sum 对象指针（`{u32 tag, i64 payload}`）。
            plan.sum_layout_by_semantic_type().contains_key(ty) || plan.sum_layout_by_semantic_type().contains_key(&ty.contract_key())
        }
        SemanticType::Union(_) => true,
        SemanticType::Nullable(inner) => {
            is_local_carrier_type(inner) || is_reference_carrier(inner, plan) || matches!(**inner, SemanticType::Array(_) | SemanticType::Utf8 | SemanticType::Utf16)
        }
        // 元组：本 lane 以堆对象指针承载（元素槽合同见 `tuple_field_offset`，
        // 构造/读取在 `TupleNew`/`TupleGet` 发射点逐项校验）。
        SemanticType::Tuple(_) => true,
        _ => false,
    }
}

/// 可作为 native 8 字节标量槽的类型。
///
/// 浮点与整数共用 8 字节槽，但**载体语义不同**：整数读写走 `rax`，浮点读写走
/// `xmm` 的 `movsd/movss`；槽里保存的是位模式，`f32` 有效位在低 32 位。
fn is_slot_type(ty: &SemanticType) -> bool {
    matches!(
        ty,
        SemanticType::Boolean
            | SemanticType::Character
            | SemanticType::Integer8 { .. }
            | SemanticType::Integer16 { .. }
            | SemanticType::Integer32 { .. }
            | SemanticType::Integer64 { .. }
            | SemanticType::Float32
            | SemanticType::Float64
    )
}

/// 数组元素的结构候选：标量，或本 lane 以 8 字节指针承载的引用/聚合/sum/数组/元组/文本。
///
/// 本函数在 import 冻结阶段使用，早于计划建立，因此只能按形状判断；它必须覆盖
/// [`is_local_carrier_type`] 的全部元素形状（以及需要计划才能确认存储类的引用形状），
/// 否则同一元素类型会在构造入口被接受、在 operator 冻结被拒绝。值存储聚合等精确
/// 裁决留给 lowering 的 [`FunctionLowerer::is_local_carrier`]，避免把内联值当成指针搬运。
fn is_array_element_carrier_candidate(ty: &SemanticType) -> bool {
    is_slot_type(ty)
        || matches!(
            ty,
            SemanticType::Nominal { .. }
                | SemanticType::Apply(..)
                | SemanticType::Union(_)
                | SemanticType::Nullable(_)
                | SemanticType::Array(_)
                | SemanticType::Tuple(_)
                | SemanticType::Utf8
                | SemanticType::Utf16
        )
}

/// 帧槽可承载的类型：标量值、堆数组指针（`[T]` 的物理载体是 8 字节指针）、
/// 元组堆对象指针，以及文本对象指针。
///
/// `utf8` 语言值统一物化为 `Utf8Text` 对象 `{ [0] = _repr 数组 }`（见
/// `emit_text_constant`），std 的内容路径按对象读取。`utf16` 仍复用静态自相对
/// 描述符 `{ i32 self_relative_ptr; u32 len }`：本 lane 尚未闭合任何真实的
/// UTF-16 内容消费路径，语言值图里不会出现读取其内容的边。
fn is_local_carrier_type(ty: &SemanticType) -> bool {
    is_slot_type(ty) || heap_array_element_type(ty).is_some() || matches!(ty, SemanticType::Utf8 | SemanticType::Utf16)
}

/// 堆数组 `[T]` 的元素类型；定长值聚合 `[T; N]` 与标量返回 `None`。
///
/// 定长数组在语言里是栈上值聚合，不走本 lane 的堆数组表示，必须在调用点
/// 结构化拒绝而不是把它当指针搬运。
fn heap_array_element_type(ty: &SemanticType) -> Option<&SemanticType> {
    match ty {
        SemanticType::Array(element) => Some(element),
        _ => None,
    }
}

/// `Cast` 的整数表示：位宽 + 符号性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct X64IntShape {
    bits: u8,
    signed: bool,
}

/// 整数族（含 `Character` 的 32 位无符号码位）的表示；浮点/布尔/文本返回 `None`。
fn x64_int_shape(ty: &SemanticType) -> Option<X64IntShape> {
    Some(match ty {
        SemanticType::Integer8 { signed } => X64IntShape { bits: 8, signed: *signed },
        SemanticType::Integer16 { signed } => X64IntShape { bits: 16, signed: *signed },
        SemanticType::Integer32 { signed } => X64IntShape { bits: 32, signed: *signed },
        SemanticType::Integer64 { signed } => X64IntShape { bits: 64, signed: *signed },
        SemanticType::Character => X64IntShape { bits: 32, signed: false },
        _ => return None,
    })
}

/// 浮点转整数的排他区间 `[lower, upper)`：两端都是 2 的幂，`f32`/`f64` 下精确可表示。
///
/// 有符号目标为 `[-2^(bits-1), 2^(bits-1))`，无符号目标为 `[0, 2^bits)`。
/// `u64` 目标在上游已结构化拒绝，这里只会收到可转换的形状。
fn float_int_target_bounds(shape: &X64IntShape) -> (f64, f64) {
    if shape.signed {
        let magnitude = (1u64 << (shape.bits - 1)) as f64;
        (-magnitude, magnitude)
    } else {
        (0.0, (1u128 << shape.bits) as f64)
    }
}

/// 窄整数（8/16/32）在 `rax` 上的规范化指令：有符号符号扩展、无符号零扩展。
fn narrow_normalize(bits: u8, signed: bool) -> Option<X64TextInstruction> {
    Some(match (bits, signed) {
        (8, true) => X64TextInstruction::SignExtend8,
        (16, true) => X64TextInstruction::SignExtend16,
        (32, true) => X64TextInstruction::SignExtend32,
        (8, false) => X64TextInstruction::ZeroExtend8,
        (16, false) => X64TextInstruction::ZeroExtend16,
        (32, false) => X64TextInstruction::ZeroExtend32,
        _ => return None,
    })
}

/// 可直接作为进程退出码的返回类型。
fn is_exit_code_type(ty: &SemanticType) -> bool {
    matches!(ty, SemanticType::Unit | SemanticType::Bottom) || is_slot_type(ty)
}

/// 源码 `[x86_64_operator("i64.add")]` 声明的 x86-64 目标 operator 助记符。
///
/// 注解按 **ISA** 声明，而不是按「native」：`native` 只是产物形态，`native` /
/// `node` / `python` 最终都会展开成规范目标四元组，而算子编码由四元组的 `arch`
/// 决定。只有显式声明 ISA 的绑定才能在此冻结编码；未声明 ISA 的绑定保持
/// `Unfrozen`，由 emit 结构化失败，禁止按宿主架构猜编码。
///
/// 只接受 platform tag `x86_64-operator` 且 locator 恰好一段；其余形态返回 `None`。
pub(crate) fn x86_64_operator_target(link: &compiler_target::ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("x86_64-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// 唯一 x86-64 operator 配方表。未知名字返回 `None`。
///
/// 与 JVM/CLR 使用同一组 provider 名（`integer.*` / `long.*` / `double.*` / `float.*`），
/// 便于 std 按目标替换实现而不改调用点。整数、浮点与无符号比较都已冻结；未实现的
/// provider 返回 `None`，计划落 `Unfrozen`，由 emit 结构化失败，不猜编码。
pub(crate) fn freeze_x86_64_operator_binding(operator_name: &str) -> Option<TargetOperatorPlan<X86_64OperatorCode>> {
    let freeze = |code: X86_64OperatorCode| TargetOperatorPlan {
        arity: code.arity(),
        operand_type: code.operand_type(),
        result_type: code.result_type(),
        recipe: TargetOperatorRecipe::Operands,
        encoding: code,
    };
    Some(freeze(match operator_name {
        // 32 位整数算术/位/移位：`eax` 载体，结果零扩展到 64 位槽。
        "integer.add" => X86_64OperatorCode::I32Add,
        "integer.subtract" => X86_64OperatorCode::I32Subtract,
        "integer.multiply" => X86_64OperatorCode::I32Multiply,
        "integer.divide" => X86_64OperatorCode::I32Divide,
        "integer.remainder" => X86_64OperatorCode::I32Remainder,
        "integer.divide_unsigned" => X86_64OperatorCode::I32DivideUnsigned,
        "integer.remainder_unsigned" => X86_64OperatorCode::I32RemainderUnsigned,
        "integer.negate" => X86_64OperatorCode::I32Negate,
        "integer.and" => X86_64OperatorCode::I32BitAnd,
        "integer.or" => X86_64OperatorCode::I32BitOr,
        "integer.xor" => X86_64OperatorCode::I32BitXor,
        "integer.shift_left" => X86_64OperatorCode::I32ShiftLeft,
        "integer.shift_right" => X86_64OperatorCode::I32ShiftRight,
        "integer.shift_right_unsigned" => X86_64OperatorCode::I32ShiftRightUnsigned,
        "integer.compare_equal" => X86_64OperatorCode::I32CompareEqual,
        "integer.compare_not_equal" => X86_64OperatorCode::I32CompareNotEqual,
        "integer.compare_less_than" => X86_64OperatorCode::I32CompareLessThan,
        "integer.compare_less_or_equal" => X86_64OperatorCode::I32CompareLessOrEqual,
        "integer.compare_greater_than" => X86_64OperatorCode::I32CompareGreaterThan,
        "integer.compare_greater_or_equal" => X86_64OperatorCode::I32CompareGreaterOrEqual,
        "integer.compare_less_than_unsigned" => X86_64OperatorCode::I32CompareLessThanUnsigned,
        // 64 位整数算术/位/移位：`rax` 载体。
        "long.add" => X86_64OperatorCode::I64Add,
        "long.subtract" => X86_64OperatorCode::I64Subtract,
        "long.multiply" => X86_64OperatorCode::I64Multiply,
        "long.divide" => X86_64OperatorCode::I64Divide,
        "long.remainder" => X86_64OperatorCode::I64Remainder,
        "long.divide_unsigned" => X86_64OperatorCode::I64DivideUnsigned,
        "long.remainder_unsigned" => X86_64OperatorCode::I64RemainderUnsigned,
        "long.negate" => X86_64OperatorCode::I64Negate,
        "long.and" => X86_64OperatorCode::I64BitAnd,
        "long.or" => X86_64OperatorCode::I64BitOr,
        "long.xor" => X86_64OperatorCode::I64BitXor,
        "long.shift_left" => X86_64OperatorCode::I64ShiftLeft,
        "long.shift_right" => X86_64OperatorCode::I64ShiftRight,
        "long.shift_right_unsigned" => X86_64OperatorCode::I64ShiftRightUnsigned,
        "long.compare_equal" => X86_64OperatorCode::I64CompareEqual,
        "long.compare_not_equal" => X86_64OperatorCode::I64CompareNotEqual,
        "long.compare_less_than" => X86_64OperatorCode::I64CompareLessThan,
        "long.compare_less_or_equal" => X86_64OperatorCode::I64CompareLessOrEqual,
        "long.compare_greater_than" => X86_64OperatorCode::I64CompareGreaterThan,
        "long.compare_greater_or_equal" => X86_64OperatorCode::I64CompareGreaterOrEqual,
        "long.compare_less_than_unsigned" => X86_64OperatorCode::I64CompareLessThanUnsigned,
        // 64 位浮点：`xmm0/xmm1` 载体，结果留在 `xmm0`；比较归一为 0/1。
        "double.add" => X86_64OperatorCode::F64Add,
        "double.subtract" => X86_64OperatorCode::F64Subtract,
        "double.multiply" => X86_64OperatorCode::F64Multiply,
        "double.divide" => X86_64OperatorCode::F64Divide,
        "double.negate" => X86_64OperatorCode::F64Negate,
        "double.compare_equal" => X86_64OperatorCode::F64CompareEqual,
        "double.compare_not_equal" => X86_64OperatorCode::F64CompareNotEqual,
        "double.compare_less_than" => X86_64OperatorCode::F64CompareLessThan,
        "double.compare_less_or_equal" => X86_64OperatorCode::F64CompareLessOrEqual,
        "double.compare_greater_than" => X86_64OperatorCode::F64CompareGreaterThan,
        "double.compare_greater_or_equal" => X86_64OperatorCode::F64CompareGreaterOrEqual,
        // 32 位浮点：与 `f64` 同构，宽度由 `movss`/`addss` 族承载。
        "float.add" => X86_64OperatorCode::F32Add,
        "float.subtract" => X86_64OperatorCode::F32Subtract,
        "float.multiply" => X86_64OperatorCode::F32Multiply,
        "float.divide" => X86_64OperatorCode::F32Divide,
        "float.negate" => X86_64OperatorCode::F32Negate,
        "float.compare_equal" => X86_64OperatorCode::F32CompareEqual,
        "float.compare_not_equal" => X86_64OperatorCode::F32CompareNotEqual,
        "float.compare_less_than" => X86_64OperatorCode::F32CompareLessThan,
        "float.compare_less_or_equal" => X86_64OperatorCode::F32CompareLessOrEqual,
        "float.compare_greater_than" => X86_64OperatorCode::F32CompareGreaterThan,
        "float.compare_greater_or_equal" => X86_64OperatorCode::F32CompareGreaterOrEqual,
        _ => return None,
    }))
}

/// 指令种类的稳定诊断名（不依赖 Debug 输出细节）。
fn instruction_kind_name(kind: &InstructionKind) -> &'static str {
    match kind {
        InstructionKind::LoadConstant { .. } => "LoadConstant",
        InstructionKind::Copy { .. } => "Copy",
        InstructionKind::Cast { .. } => "Cast",
        InstructionKind::StoreVar { .. } => "StoreVar",
        InstructionKind::Call { .. } => "Call",
        InstructionKind::LoadFunctionRef { .. } => "LoadFunctionRef",
        InstructionKind::StructNew { .. } => "StructNew",
        InstructionKind::TupleNew { .. } => "TupleNew",
        InstructionKind::TupleGet { .. } => "TupleGet",
        InstructionKind::AggregateCopy { .. } => "AggregateCopy",
        InstructionKind::FieldGet { .. } => "FieldGet",
        InstructionKind::FieldSet { .. } => "FieldSet",
        InstructionKind::SumNew { .. } => "SumNew",
        InstructionKind::SumPayloadGet { .. } => "SumPayloadGet",
        InstructionKind::SumVariantIs { .. } => "SumVariantIs",
        InstructionKind::ArrayNew { .. } => "ArrayNew",
        InstructionKind::ArrayFromElements { .. } => "ArrayFromElements",
        InstructionKind::ArrayGet { .. } => "ArrayGet",
        InstructionKind::ArraySet { .. } => "ArraySet",
        InstructionKind::ArrayLength { .. } => "ArrayLength",
        InstructionKind::NullIsNull { .. } => "NullIsNull",
        InstructionKind::NullUnwrap { .. } => "NullUnwrap",
    }
}

/// x86-64 平台私有的 operator 绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum X86OperatorBinding {
    /// 已冻结的 x86-64 标量 operator 编码。
    Frozen(TargetOperatorPlan<X86_64OperatorCode>),
    /// 已冻结的 x86-64 数组 operator 计划（机械堆数组操作）。
    Array(X64ArrayOperatorPlan),
    /// 已冻结的 x86-64 控制台 operator（UTF-8 字节写出）。
    Console(X64ConsoleOperation),
    /// 声明了 x86-64 operator，但没有可编码配方；emit 必须结构化失败。
    Unfrozen,
}

/// 按声明的导入签名冻结 x86-64 operator 绑定。
pub(crate) fn freeze_x86_64_operator(
    link: &compiler_target::ExternalImportLink,
    instance: compiler_types::ItemInstanceId,
    symbol: &str,
    parameter_types: &[compiler_types::SemanticType],
    return_type: &compiler_types::SemanticType,
) -> crate::Result<Option<X86OperatorBinding>> {
    let Some(mnemonic) = x86_64_operator_target(link)
    else {
        return Ok(None);
    };
    if let Some(plan) = freeze_x86_64_array_operator(mnemonic, instance, symbol, parameter_types, return_type)? {
        return Ok(Some(X86OperatorBinding::Array(plan)));
    }
    if let Some(operation) = freeze_x86_64_console_operator(mnemonic, instance, symbol, parameter_types, return_type)? {
        return Ok(Some(X86OperatorBinding::Console(operation)));
    }
    let Some(plan) = freeze_x86_64_operator_binding(mnemonic)
    else {
        return Ok(Some(X86OperatorBinding::Unfrozen));
    };
    crate::target_operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(X86OperatorBinding::Frozen(plan)))
}

/// 按声明签名冻结 x86-64 数组 operator。
///
/// 数组 operator 的物理形状不是标量载体：`[T]` 与 `usize` 都占 8 字节槽，但
/// 载体种类不同，不能用 `validate_frozen_operator` 的标量规则判定。这里只校验
/// arity、数组位置的类型身份与元素可为 8 字节槽，未知助记符返回 `None` 交回
/// 标量表处理。
fn freeze_x86_64_array_operator(
    mnemonic: &str,
    instance: compiler_types::ItemInstanceId,
    symbol: &str,
    parameter_types: &[compiler_types::SemanticType],
    return_type: &compiler_types::SemanticType,
) -> crate::Result<Option<X64ArrayOperatorPlan>> {
    let operation = match mnemonic {
        "array.new" => X64ArrayOperation::New,
        "array.length" => X64ArrayOperation::Length,
        "array.get" => X64ArrayOperation::Get,
        "array.set" => X64ArrayOperation::Set,
        _ => return Ok(None),
    };
    let arity = match operation {
        X64ArrayOperation::New | X64ArrayOperation::Length => 1,
        X64ArrayOperation::Get => 2,
        X64ArrayOperation::Set => 3,
    };
    if parameter_types.len() != arity {
        return Err(format!(
            "x86_64 数组 operator `{mnemonic}` 的声明 arity 与冻结计划不符: function={symbol} expected={arity} actual={} instance={instance:?}",
            parameter_types.len()
        ));
    }
    let require_slot = |index: usize, ty: &compiler_types::SemanticType| -> crate::Result<()> {
        if !is_slot_type(ty) {
            return Err(format!(
                "x86_64 数组 operator `{mnemonic}` 的参数 {index} 类型 `{ty}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
            ));
        }
        Ok(())
    };
    let require_array = |index: usize| -> crate::Result<&compiler_types::SemanticType> {
        let ty = &parameter_types[index];
        let Some(element) = heap_array_element_type(ty)
        else {
            return Err(format!(
                "x86_64 数组 operator `{mnemonic}` 的参数 {index} 类型 `{ty}` 不是堆数组 `[T]`: function={symbol} instance={instance:?}"
            ));
        };
        if !is_array_element_carrier_candidate(element) {
            return Err(format!(
                "x86_64 数组 operator `{mnemonic}` 的元素类型 `{element}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
            ));
        }
        Ok(element)
    };
    match operation {
        X64ArrayOperation::New => {
            require_slot(0, &parameter_types[0])?;
            require_array_return(mnemonic, symbol, instance, return_type)?;
        }
        X64ArrayOperation::Length => {
            require_array(0)?;
            if !is_slot_type(return_type) {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
                ));
            }
        }
        X64ArrayOperation::Get => {
            let element = require_array(0)?;
            require_slot(1, &parameter_types[1])?;
            if return_type != element {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 与元素类型 `{element}` 不一致: function={symbol} instance={instance:?}"
                ));
            }
        }
        X64ArrayOperation::Set => {
            let element = require_array(0)?;
            require_slot(1, &parameter_types[1])?;
            if &parameter_types[2] != element {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的写入值类型 `{}` 与元素类型 `{element}` 不一致: function={symbol} instance={instance:?}",
                    parameter_types[2]
                ));
            }
            if !return_type.is_unitish() {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 必须是 unit: function={symbol} instance={instance:?}"
                ));
            }
        }
    }
    Ok(Some(X64ArrayOperatorPlan { arity, operation }))
}

/// 按声明签名冻结 x86-64 控制台 operator。
///
/// 控制台 operator 的载体既不是纯标量也不是数组头：`handle` 是平台解释的 64 位
/// 整数（Win64 伪句柄 / Linux fd），`message` 是 `Utf8Text` 对象指针。形状在此
/// 一次冻结，emit 不再按名字猜。未知助记符返回 `None` 交回标量表。
fn freeze_x86_64_console_operator(
    mnemonic: &str,
    instance: compiler_types::ItemInstanceId,
    symbol: &str,
    parameter_types: &[compiler_types::SemanticType],
    return_type: &compiler_types::SemanticType,
) -> crate::Result<Option<X64ConsoleOperation>> {
    let operation = match mnemonic {
        "console.write" => X64ConsoleOperation::Write,
        _ => return Ok(None),
    };
    let failure = |reason: String| {
        Err(format!("x86_64 控制台 operator `{mnemonic}` {reason}: function={symbol} instance={instance:?}"))
    };
    match operation {
        X64ConsoleOperation::Write => {
            if parameter_types.len() != 2 {
                return failure(format!("声明 arity 与冻结计划不符: expected=2 actual={}", parameter_types.len()));
            }
            if !matches!(parameter_types[0], compiler_types::SemanticType::Integer64 { signed: true }) {
                return failure(format!("句柄参数类型 `{}` 不是有符号 64 位整数", parameter_types[0]));
            }
            if !matches!(parameter_types[1], compiler_types::SemanticType::Utf8) {
                return failure(format!("消息参数类型 `{}` 不是 utf8 文本", parameter_types[1]));
            }
            if !return_type.is_unitish() {
                return failure(format!("返回类型 `{return_type}` 必须是 unit"));
            }
        }
    }
    Ok(Some(operation))
}

/// `array.new` 的返回必须是元素可承载的堆数组。
fn require_array_return(
    mnemonic: &str,
    symbol: &str,
    instance: compiler_types::ItemInstanceId,
    return_type: &compiler_types::SemanticType,
) -> crate::Result<()> {
    let Some(element) = heap_array_element_type(return_type)
    else {
        return Err(format!(
            "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 不是堆数组 `[T]`: function={symbol} instance={instance:?}"
        ));
    };
    if !is_array_element_carrier_candidate(element) {
        return Err(format!(
            "x86_64 数组 operator `{mnemonic}` 的元素类型 `{element}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod array_element_carrier_tests {
    use super::*;

    /// import 冻结早于计划建立：候选筛查必须覆盖所有无计划即可判定为元素载体的形状，
    /// 否则同一元素类型会在构造入口（`is_local_carrier_type`）被接受、在 operator
    /// 冻结被拒绝——这正是两条入口合同漂移的形态。
    #[test]
    fn operator_candidate_screen_covers_every_plan_free_element_carrier() {
        let shapes = vec![
            SemanticType::Boolean,
            SemanticType::Character,
            SemanticType::Integer8 { signed: true },
            SemanticType::Integer16 { signed: false },
            SemanticType::Integer32 { signed: true },
            SemanticType::Integer64 { signed: false },
            SemanticType::Float32,
            SemanticType::Float64,
            SemanticType::Utf8,
            SemanticType::Utf16,
            SemanticType::Array(Box::new(SemanticType::Integer8 { signed: false })),
            SemanticType::Tuple(vec![SemanticType::Integer64 { signed: true }]),
        ];
        for ty in shapes {
            if is_local_carrier_type(&ty) {
                assert!(
                    is_array_element_carrier_candidate(&ty),
                    "`{ty}` 在构造入口可作数组元素，却被 operator 候选筛查拒绝"
                );
            }
        }
    }
}
