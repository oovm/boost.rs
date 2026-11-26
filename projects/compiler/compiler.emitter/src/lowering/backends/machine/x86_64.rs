//! x86-64 正式 lane 降低：计划事实 → 文本 LIR → 唯一程序装配器 → 容器。
//!
//! 本文件是 **x86-64 ISA 后端**，只保留目标私有的装配服务：语言侧
//! Valkyrie × Machine 适配器直接消费 MIR 产出 [`X64ProgramInput`]，按规范目标
//! 四元组分派进入本 lane；这里不判断「native」形态，只实现四元组确定的编码、
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

use crate::{
    Result,
    emitter_backend_machine::{MachineImageKind, classify_machine_image},
    target_operator::{TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe},
    transitional::binary::pe::{NativePeImage, NativePeWriter},
};
use compiler_target::abstractions::BinaryArch;
use compiler_types::SemanticType;

use data_binary_pe::elf::{NativeElfImage, NativeElfWriter};

use super::{X64CallConv, X64LoweredProgram, X64Platform, X64ProgramFunction, X64TextInstruction, encode_x64_program, x64_lir};

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
pub enum X64ArrayOperation {
    /// `array.new<T>(capacity) -> [T]`：分配并零初始化容量槽位。
    New,
    /// `array.length<T>([T]) -> usize`：读取头部元素个数。
    Length,
    /// `array.get<T>([T], index) -> T`：按 0 基下标读取一个 8 字节槽。
    Get,
    /// `array.set<T>([T], index, T) -> unit`：按 0 基下标写入一个 8 字节槽。
    Set,
}

/// 已冻结的 x86-64 数组 operator 计划。
///
/// 物理布局、分配/释放实现与失败出口由 [`super::array_runtime`] 唯一冻结；本计划
/// 只固定操作身份与调用点 arity，不复制第二套数组表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X64ArrayOperatorPlan {
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
pub enum X64ConsoleOperation {
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
/// 容器 + 自检）只接受这个形状，不再接触任何跨目标共享 IR 形状。
pub struct X64ProgramInput {
    abi: X64CallConv,
    container: ContainerKind,
    functions: Vec<X64ProgramFunction>,
    entry_name: String,
}

impl X64ProgramInput {
    /// 语言适配器构造目标私有进程镜像输入：容器与调用约定由平台身份一次冻结。
    pub fn new(platform: X64Platform, functions: Vec<X64ProgramFunction>, entry_name: String) -> Self {
        let container = if platform.is_windows() { ContainerKind::Pe } else { ContainerKind::Elf };
        Self { abi: platform.abi(), container, functions, entry_name }
    }

    /// 目标平台身份：由容器一次冻结，不从 `arch` 推断 `os`/`abi`。
    pub fn platform(&self) -> X64Platform {
        match self.container {
            ContainerKind::Pe => X64Platform::windows_x64(),
            ContainerKind::Elf => X64Platform::linux_x64(),
        }
    }
}

/// 目标私有 LIR 程序 → 容器镜像。
///
/// 这是本 lane 唯一的编码与容器装配入口：签名里没有跨目标共享 IR 形状，
/// 复用唯一程序装配器与 PE/ELF 容器写出。
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

/// 窄整数（8/16/32）在 `rax` 上的规范化指令：有符号符号扩展、无符号零扩展。
pub fn narrow_normalize(bits: u8, signed: bool) -> Option<X64TextInstruction> {
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

/// 唯一 x86-64 operator 配方表。未知名字返回 `None`。
///
/// 与 JVM/CLR 使用同一组 provider 名（`integer.*` / `long.*` / `double.*` / `float.*`），
/// 便于 std 按目标替换实现而不改调用点。整数、浮点与无符号比较都已冻结；未实现的
/// provider 返回 `None`，计划落 `Unfrozen`，由 emit 结构化失败，不猜编码。
pub fn freeze_x86_64_operator_binding(operator_name: &str) -> Option<TargetOperatorPlan<X86_64OperatorCode>> {
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

/// x86-64 平台私有的 operator 绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86OperatorBinding {
    /// 已冻结的 x86-64 标量 operator 编码。
    Frozen(TargetOperatorPlan<X86_64OperatorCode>),
    /// 已冻结的 x86-64 数组 operator 计划（机械堆数组操作）。
    Array(X64ArrayOperatorPlan),
    /// 已冻结的 x86-64 控制台 operator（UTF-8 字节写出）。
    Console(X64ConsoleOperation),
    /// 声明了 x86-64 operator，但没有可编码配方；emit 必须结构化失败。
    Unfrozen,
}

