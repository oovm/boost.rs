//! x86-64 机器码图元权威（机器码 lane 唯一字节表）。
//!
//! 所有权边界（01 §5）：`ExecutableFunction` 是语言语义事实，本模块不读取它。
//! 帧序言/尾声、寄存器前缀表、算子编码、`rel32` 回填与数据/导入寻址图元在这里
//! 唯一实现；[`super::x64_program`] 的程序装配器与 ELF 入口桩共享同一套图元，
//! 任何其他模块不得再手写同一 ISA 的指令字节。
//!
//! 栈槽固定为帧内 8 字节槽（`[rbp - 8 * (slot + 1)]`）；调用与跳转以 `rel32`
//! 占位记录，由程序装配器在全部函数布局完成后回填。

use crate::Result;

use super::{X64CallConv, x86_64::X86_64OperatorCode};

/// x86-64 通用寄存器（本 lane 目前只使用有固定编码职责的子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum X64Reg {
    /// `rax`：表达式结果 / 返回载体。
    Rax,
    /// `rcx`：Win64 第 1 / SysV 第 4 个整数形参，也是算子右操作数。
    Rcx,
    /// `rdx`：Win64 第 2 / SysV 第 3 个整数形参。
    Rdx,
    /// `r8`：Win64 第 3 / SysV 第 5 个整数形参。
    R8,
    /// `r9`：Win64 第 4 / SysV 第 6 个整数形参。
    R9,
    /// `rdi`：SysV 第 1 个整数形参。
    Rdi,
    /// `rsi`：SysV 第 2 个整数形参。
    Rsi,
}

impl X64Reg {
    /// Win64 整数形参寄存器顺序。
    pub(super) const WIN64_ARGS: [Self; 4] = [Self::Rcx, Self::Rdx, Self::R8, Self::R9];
    /// SysV64（x86-64 ELF）整数形参寄存器顺序。
    pub(super) const SYSTEM_V_ARGS: [Self; 6] = [Self::Rdi, Self::Rsi, Self::Rdx, Self::Rcx, Self::R8, Self::R9];

    /// 按调用约定返回整数形参/实参的寄存器顺序。
    pub(super) fn argument_registers(abi: X64CallConv) -> &'static [Self] {
        match abi {
            X64CallConv::Win64 => &Self::WIN64_ARGS,
            X64CallConv::SystemV64 => &Self::SYSTEM_V_ARGS,
        }
    }

    /// `mov [rbp + disp32], reg` 的编码前缀（含 REX 与 ModRM）。
    fn frame_store_prefix(self) -> [u8; 3] {
        match self {
            Self::Rax => [0x48, 0x89, 0x85],
            Self::Rcx => [0x48, 0x89, 0x8D],
            Self::Rdx => [0x48, 0x89, 0x95],
            Self::R8 => [0x4C, 0x89, 0x85],
            Self::R9 => [0x4C, 0x89, 0x8D],
            Self::Rdi => [0x48, 0x89, 0xBD],
            Self::Rsi => [0x48, 0x89, 0xB5],
        }
    }

    /// `mov reg, [rbp + disp32]` 的编码前缀。
    fn frame_load_prefix(self) -> [u8; 3] {
        match self {
            Self::Rax => [0x48, 0x8B, 0x85],
            Self::Rcx => [0x48, 0x8B, 0x8D],
            Self::Rdx => [0x48, 0x8B, 0x95],
            Self::R8 => [0x4C, 0x8B, 0x85],
            Self::R9 => [0x4C, 0x8B, 0x8D],
            Self::Rdi => [0x48, 0x8B, 0xBD],
            Self::Rsi => [0x48, 0x8B, 0xB5],
        }
    }

    /// `mov reg, imm64` 的编码前缀。
    fn imm64_prefix(self) -> [u8; 2] {
        match self {
            Self::Rax => [0x48, 0xB8],
            Self::Rcx => [0x48, 0xB9],
            Self::Rdx => [0x48, 0xBA],
            Self::R8 => [0x49, 0xB8],
            Self::R9 => [0x49, 0xB9],
            Self::Rdi => [0x48, 0xBF],
            Self::Rsi => [0x48, 0xBE],
        }
    }
}

/// 局部槽到 `rbp` 的负位移：`[rbp - 8 * (slot + 1)]`。
fn frame_disp(slot: u16) -> i32 {
    -8 * (i32::from(slot) + 1)
}

/// 帧序言：`push rbp; mov rbp, rsp; sub rsp, frame_bytes`。
pub(super) fn emit_prologue(text: &mut Vec<u8>, frame_bytes: u32) {
    text.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x81, 0xEC]);
    text.extend_from_slice(&frame_bytes.to_le_bytes());
}

/// 函数返回：`mov rsp, rbp; pop rbp; ret`（返回值已在 `rax`）。
pub(super) fn emit_function_return(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x89, 0xEC, 0x5D, 0xC3]);
}

/// `ud2`：到达即故障。
pub(super) fn emit_trap(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x0F, 0x0B]);
}

/// `call rel32`：写操作码与占位位移，返回位移字段偏移。
pub(super) fn emit_call_placeholder(text: &mut Vec<u8>) -> usize {
    text.push(0xE8);
    let field = text.len();
    text.extend_from_slice(&[0, 0, 0, 0]);
    field
}

/// `jmp rel32`。
pub(super) fn emit_jump_placeholder(text: &mut Vec<u8>) -> usize {
    text.push(0xE9);
    let field = text.len();
    text.extend_from_slice(&[0, 0, 0, 0]);
    field
}

/// `test eax, eax` + `jz rel32`（条件值已在 `rax`，只按 32 位布尔载体判定）。
pub(super) fn emit_jump_if_zero_placeholder(text: &mut Vec<u8>) -> usize {
    text.extend_from_slice(&[0x85, 0xC0, 0x0F, 0x84]);
    let field = text.len();
    text.extend_from_slice(&[0, 0, 0, 0]);
    field
}

/// `call qword ptr [rip + disp32]`：导入间接调用；修复去向由容器决定。
pub(super) fn emit_call_import_placeholder(text: &mut Vec<u8>) -> usize {
    text.extend_from_slice(&[0xFF, 0x15]);
    let field = text.len();
    text.extend_from_slice(&[0, 0, 0, 0]);
    field
}

/// `lea rax, [rip + disp32]`：rip 相对取址，位移在布局确定后回填。
pub(super) fn emit_lea_rip_placeholder(text: &mut Vec<u8>) -> usize {
    text.extend_from_slice(&[0x48, 0x8D, 0x05]);
    let field = text.len();
    text.extend_from_slice(&[0, 0, 0, 0]);
    field
}

/// `rel32` 回填的唯一实现：位移 = 目标 − (字段 + 4)。
pub(super) fn patch_rel32(text: &mut [u8], field: usize, target: usize, out_of_range: impl FnOnce() -> String) -> Result<()> {
    let displacement = i64::try_from(target).unwrap_or(i64::MAX) - (i64::try_from(field).unwrap_or(i64::MAX) + 4);
    let displacement = i32::try_from(displacement).map_err(|_| out_of_range())?;
    text[field..field + 4].copy_from_slice(&displacement.to_le_bytes());
    Ok(())
}

/// `mov reg, imm64`。
pub(super) fn emit_mov_imm64(text: &mut Vec<u8>, register: X64Reg, value: i64) {
    text.extend_from_slice(&register.imm64_prefix());
    text.extend_from_slice(&(value as u64).to_le_bytes());
}

/// `mov reg, [rbp - 8 * (slot + 1)]`。
pub(super) fn emit_mov_from_frame(text: &mut Vec<u8>, register: X64Reg, slot: u16) {
    text.extend_from_slice(&register.frame_load_prefix());
    text.extend_from_slice(&frame_disp(slot).to_le_bytes());
}

/// `mov [rbp - 8 * (slot + 1)], reg`。
pub(super) fn emit_mov_to_frame(text: &mut Vec<u8>, register: X64Reg, slot: u16) {
    text.extend_from_slice(&register.frame_store_prefix());
    text.extend_from_slice(&frame_disp(slot).to_le_bytes());
}

/// `lea rax, [rbp - 8 * (slot + 1)]`：取帧槽地址。
pub(super) fn emit_lea_rax_from_frame(text: &mut Vec<u8>, slot: u16) {
    text.extend_from_slice(&[0x48, 0x8D, 0x85]);
    text.extend_from_slice(&frame_disp(slot).to_le_bytes());
}

/// 按调用约定取第 `index` 个整数形参寄存器；超出寄存器上限返回 `None`。
pub(super) fn argument_register(abi: X64CallConv, index: u8) -> Option<X64Reg> {
    X64Reg::argument_registers(abi).get(index as usize).copied()
}

/// `add rax, imm32`。
pub(super) fn emit_add_rax_imm32(text: &mut Vec<u8>, value: i32) {
    text.extend_from_slice(&[0x48, 0x81, 0xC0]);
    text.extend_from_slice(&value.to_le_bytes());
}

/// `sub rax, imm32`。
pub(super) fn emit_sub_rax_imm32(text: &mut Vec<u8>, value: i32) {
    text.extend_from_slice(&[0x48, 0x81, 0xE8]);
    text.extend_from_slice(&value.to_le_bytes());
}

/// `add rdx, imm32`。
pub(super) fn emit_add_rdx_imm32(text: &mut Vec<u8>, value: i32) {
    text.extend_from_slice(&[0x48, 0x81, 0xC2]);
    text.extend_from_slice(&value.to_le_bytes());
}

/// `neg rax`。
pub(super) fn emit_neg_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0xF7, 0xD8]);
}

/// `rax = rax / 10`，余数进入 `rdx`（无符号除法；调用方保证非负）。
pub(super) fn emit_divide_by_ten(text: &mut Vec<u8>) {
    // `xor rdx, rdx; mov rcx, 10; div rcx`。
    text.extend_from_slice(&[0x48, 0x31, 0xD2, 0x48, 0xB9]);
    text.extend_from_slice(&10u64.to_le_bytes());
    text.extend_from_slice(&[0x48, 0xF7, 0xF1]);
}

/// `mov [rax], dl`：把 `rdx` 的低字节写入 `rax` 指向的缓冲区。
pub(super) fn emit_store_byte_at_rax_from_rdx(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x88, 0x10]);
}

/// `movzx eax, byte [rax]`：读取单字节并零扩展到 64 位。
pub(super) fn emit_load_byte_from_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x0F, 0xB6, 0x00]);
}

/// `mov rax, [rax]`：读取机器字。
pub(super) fn emit_load_from_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x8B, 0x00]);
}

/// `mov rax, [rax + disp32]`：带位移的机器字读取（数组头部的分配字节数）。
pub(super) fn emit_load_from_rax_displacement(text: &mut Vec<u8>, displacement: i32) {
    text.extend_from_slice(&[0x48, 0x8B, 0x80]);
    text.extend_from_slice(&displacement.to_le_bytes());
}

/// `mov [rax], rcx`。
pub(super) fn emit_store_rcx_to_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x89, 0x08]);
}

/// `mov [rax + disp32], rcx`：带位移的机器字写入（数组头部的分配字节数）。
pub(super) fn emit_store_rcx_to_rax_displacement(text: &mut Vec<u8>, displacement: i32) {
    text.extend_from_slice(&[0x48, 0x89, 0x88]);
    text.extend_from_slice(&displacement.to_le_bytes());
}

/// `mov [rax + disp32], rdx`：对象字段 / sum 载荷写入。
pub(super) fn emit_store_rdx_to_rax_displacement(text: &mut Vec<u8>, displacement: i32) {
    text.extend_from_slice(&[0x48, 0x89, 0x90]);
    text.extend_from_slice(&displacement.to_le_bytes());
}

/// `mov eax, [rax]`：读取 32 位 tag 并零扩展。
pub(super) fn emit_load_dword_from_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x8B, 0x00]);
}

/// `mov [rcx], rax`。
pub(super) fn emit_store_rax_to_rcx(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x89, 0x01]);
}

/// `mov [rax], rdx`。
pub(super) fn emit_store_rdx_to_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x89, 0x10]);
}

/// `shl rax, imm8`。
pub(super) fn emit_shift_left_rax_imm8(text: &mut Vec<u8>, bits: u8) {
    text.extend_from_slice(&[0x48, 0xC1, 0xE0, bits]);
}

/// `mov rax, [rax + rcx*8 + 16]`：数组元素读取（8 字节槽、16 字节头部）。
pub(super) fn emit_load_indexed_from_rax_rcx(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x8B, 0x44, 0xC8, 0x10]);
}

/// `mov [rax + rcx*8 + 16], rdx`：数组元素写入（8 字节槽、16 字节头部）。
pub(super) fn emit_store_indexed_rdx_to_rax_rcx(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x89, 0x54, 0xC8, 0x10]);
}

/// `mov dword ptr [rax], imm32`。
pub(super) fn emit_store_dword_imm_to_rax(text: &mut Vec<u8>, value: u32) {
    text.extend_from_slice(&[0xC7, 0x00]);
    text.extend_from_slice(&value.to_le_bytes());
}

/// `mov [rax], ecx`。
pub(super) fn emit_store_dword_rcx_to_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x89, 0x08]);
}

/// `mov eax, [rax + 4]`：读字符串描述符里的 `u32` 长度（零扩展）。
pub(super) fn emit_load_descriptor_length(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x8B, 0x40, 0x04]);
}

/// 复制字节：`rax` = 目标、`rcx` = 源、`rdx` = 长度。
///
/// 保存/恢复 `rdi`/`rsi`（两种 ABI 都是被调用者保存），`rep movsb` 完成复制；
/// 方向标志由 ABI 保证为 0。
pub(super) fn emit_copy_bytes(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x57, 0x56, 0x48, 0x89, 0xC7, 0x48, 0x89, 0xCE, 0x48, 0x89, 0xD1, 0xF3, 0xA4, 0x5E, 0x5F]);
}

/// Linux `write(2)`：实参按 SysV64 位于 `rdi`/`rsi`/`rdx`。
pub(super) fn emit_syscall_write(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x05]);
}

/// Linux `exit(2)`：退出码取自 `rax`（前置 `mov rdi, rax` 由调用方发射）。
pub(super) fn emit_syscall_exit(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05]);
}

/// Linux `mmap(NULL, len, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)`。
///
/// 长度取自 `rax`；返回值（地址或负 errno）写回 `rax`。匿名映射天然按页对齐且
/// 清零，满足数组元素槽的零初始化与 8 字节对齐要求。
pub(super) fn emit_syscall_mmap(text: &mut Vec<u8>) {
    // mov rsi, rax；xor edi, edi；mov edx, 3；mov r10d, 0x22；
    text.extend_from_slice(&[0x48, 0x89, 0xC6, 0x31, 0xFF, 0xBA, 0x03, 0x00, 0x00, 0x00]);
    text.extend_from_slice(&[0x41, 0xBA, 0x22, 0x00, 0x00, 0x00, 0x49, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF]);
    // xor r9d, r9d；mov eax, 9（SYS_mmap）；syscall。
    text.extend_from_slice(&[0x45, 0x31, 0xC9, 0xB8, 0x09, 0x00, 0x00, 0x00, 0x0F, 0x05]);
}

/// Linux `munmap(ptr, len)`：地址取自 `rax`、长度取自 `rcx`；返回值忽略。
pub(super) fn emit_syscall_munmap(text: &mut Vec<u8>) {
    // mov rdi, rax；mov rsi, rcx；mov eax, 11（SYS_munmap）；syscall。
    text.extend_from_slice(&[0x48, 0x89, 0xC7, 0x48, 0x89, 0xCE, 0xB8, 0x0B, 0x00, 0x00, 0x00, 0x0F, 0x05]);
}

/// `mov rdi, rax`。
pub(super) fn emit_mov_rdi_from_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x48, 0x89, 0xC7]);
}

/// Win64：`mov qword ptr [rsp + displacement], 0`（第 5+ 个栈实参置零）。
pub(super) fn emit_zero_stack_arg(text: &mut Vec<u8>, displacement: u8) {
    text.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, displacement, 0, 0, 0, 0]);
}

/// Win64：`mov [rsp + offset], rax`（栈实参周转）。
pub(super) fn emit_store_rax_to_stack(text: &mut Vec<u8>, offset: u32) {
    text.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]);
    text.extend_from_slice(&offset.to_le_bytes());
}

/// 按冻结 x86-64 operator 编码（唯一算子字节表；操作数已按约定位于 `rax`/`rcx`）。
pub(super) fn encode_operator(text: &mut Vec<u8>, code: X86_64OperatorCode) {
    match code {
        X86_64OperatorCode::I64Add => text.extend_from_slice(&[0x48, 0x01, 0xC8]),
        X86_64OperatorCode::I64Subtract => text.extend_from_slice(&[0x48, 0x29, 0xC8]),
        X86_64OperatorCode::I64Multiply => text.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]),
        // `cqo` 把 rax 符号扩展到 rdx:rax；`idiv rcx` 后商在 rax、余数在 rdx。
        X86_64OperatorCode::I64Divide => text.extend_from_slice(&[0x48, 0x99, 0x48, 0xF7, 0xF9]),
        X86_64OperatorCode::I64Remainder => text.extend_from_slice(&[0x48, 0x99, 0x48, 0xF7, 0xF9, 0x48, 0x89, 0xD0]),
        // 无符号除法必须先把 rdx 清零，再 `div rcx`。
        X86_64OperatorCode::I64DivideUnsigned => text.extend_from_slice(&[0x31, 0xD2, 0x48, 0xF7, 0xF1]),
        X86_64OperatorCode::I64RemainderUnsigned => text.extend_from_slice(&[0x31, 0xD2, 0x48, 0xF7, 0xF1, 0x48, 0x89, 0xD0]),
        X86_64OperatorCode::I64Negate => text.extend_from_slice(&[0x48, 0xF7, 0xD8]),
        X86_64OperatorCode::I64BitAnd => text.extend_from_slice(&[0x48, 0x21, 0xC8]),
        X86_64OperatorCode::I64BitOr => text.extend_from_slice(&[0x48, 0x09, 0xC8]),
        X86_64OperatorCode::I64BitXor => text.extend_from_slice(&[0x48, 0x31, 0xC8]),
        X86_64OperatorCode::I64ShiftLeft => text.extend_from_slice(&[0x48, 0xD3, 0xE0]),
        X86_64OperatorCode::I64ShiftRight => text.extend_from_slice(&[0x48, 0xD3, 0xF8]),
        X86_64OperatorCode::I64CompareEqual => encode_compare_i64(text, 0x94),
        X86_64OperatorCode::I64CompareNotEqual => encode_compare_i64(text, 0x95),
        X86_64OperatorCode::I64CompareLessThan => encode_compare_i64(text, 0x9C),
        X86_64OperatorCode::I64CompareLessOrEqual => encode_compare_i64(text, 0x9E),
        X86_64OperatorCode::I64CompareGreaterThan => encode_compare_i64(text, 0x9F),
        X86_64OperatorCode::I64CompareGreaterOrEqual => encode_compare_i64(text, 0x9D),
        // 32 位编码不写 REX.W；x86 规则下 32 位结果自动零扩展到 64 位槽。
        X86_64OperatorCode::I32Add => text.extend_from_slice(&[0x01, 0xC8]),
        X86_64OperatorCode::I32Subtract => text.extend_from_slice(&[0x29, 0xC8]),
        X86_64OperatorCode::I32Multiply => text.extend_from_slice(&[0x0F, 0xAF, 0xC1]),
        // `cdq` 把 eax 符号扩展到 edx:eax；32 位写入自动零扩展高 32 位。
        X86_64OperatorCode::I32Divide => text.extend_from_slice(&[0x99, 0xF7, 0xF9]),
        X86_64OperatorCode::I32Remainder => text.extend_from_slice(&[0x99, 0xF7, 0xF9, 0x89, 0xD0]),
        X86_64OperatorCode::I32DivideUnsigned => text.extend_from_slice(&[0x31, 0xD2, 0xF7, 0xF1]),
        X86_64OperatorCode::I32RemainderUnsigned => text.extend_from_slice(&[0x31, 0xD2, 0xF7, 0xF1, 0x89, 0xD0]),
        X86_64OperatorCode::I32Negate => text.extend_from_slice(&[0xF7, 0xD8]),
        X86_64OperatorCode::I32BitAnd => text.extend_from_slice(&[0x21, 0xC8]),
        X86_64OperatorCode::I32BitOr => text.extend_from_slice(&[0x09, 0xC8]),
        X86_64OperatorCode::I32BitXor => text.extend_from_slice(&[0x31, 0xC8]),
        X86_64OperatorCode::I32ShiftLeft => text.extend_from_slice(&[0xD3, 0xE0]),
        X86_64OperatorCode::I32ShiftRight => text.extend_from_slice(&[0xD3, 0xF8]),
        X86_64OperatorCode::I32CompareEqual => encode_compare_i32(text, 0x94),
        X86_64OperatorCode::I32CompareNotEqual => encode_compare_i32(text, 0x95),
        X86_64OperatorCode::I32CompareLessThan => encode_compare_i32(text, 0x9C),
        X86_64OperatorCode::I32CompareLessOrEqual => encode_compare_i32(text, 0x9E),
        X86_64OperatorCode::I32CompareGreaterThan => encode_compare_i32(text, 0x9F),
        X86_64OperatorCode::I32CompareGreaterOrEqual => encode_compare_i32(text, 0x9D),
        X86_64OperatorCode::I64ShiftRightUnsigned => text.extend_from_slice(&[0x48, 0xD3, 0xE8]),
        X86_64OperatorCode::I32ShiftRightUnsigned => text.extend_from_slice(&[0xD3, 0xE8]),
        // `setb` = 无符号小于（CF=1）。
        X86_64OperatorCode::I64CompareLessThanUnsigned => encode_compare_i64(text, 0x92),
        X86_64OperatorCode::I32CompareLessThanUnsigned => encode_compare_i32(text, 0x92),
        // 浮点算术：操作数在 xmm0/xmm1，结果留在 xmm0。
        X86_64OperatorCode::F64Add => text.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]),
        X86_64OperatorCode::F64Subtract => text.extend_from_slice(&[0xF2, 0x0F, 0x5C, 0xC1]),
        X86_64OperatorCode::F64Multiply => text.extend_from_slice(&[0xF2, 0x0F, 0x59, 0xC1]),
        X86_64OperatorCode::F64Divide => text.extend_from_slice(&[0xF2, 0x0F, 0x5E, 0xC1]),
        X86_64OperatorCode::F64Negate => encode_float_negate(text, false),
        X86_64OperatorCode::F32Add => text.extend_from_slice(&[0xF3, 0x0F, 0x58, 0xC1]),
        X86_64OperatorCode::F32Subtract => text.extend_from_slice(&[0xF3, 0x0F, 0x5C, 0xC1]),
        X86_64OperatorCode::F32Multiply => text.extend_from_slice(&[0xF3, 0x0F, 0x59, 0xC1]),
        X86_64OperatorCode::F32Divide => text.extend_from_slice(&[0xF3, 0x0F, 0x5E, 0xC1]),
        X86_64OperatorCode::F32Negate => encode_float_negate(text, true),
        // 浮点比较：`ucomis{s,d}` 后按 IEEE 无序语义归一为 0/1。
        X86_64OperatorCode::F64CompareEqual => encode_float_compare(text, false, FloatCompareKind::Equal),
        X86_64OperatorCode::F64CompareNotEqual => encode_float_compare(text, false, FloatCompareKind::NotEqual),
        X86_64OperatorCode::F64CompareLessThan => encode_float_compare(text, false, FloatCompareKind::LessThan),
        X86_64OperatorCode::F64CompareLessOrEqual => encode_float_compare(text, false, FloatCompareKind::LessOrEqual),
        X86_64OperatorCode::F64CompareGreaterThan => encode_float_compare(text, false, FloatCompareKind::GreaterThan),
        X86_64OperatorCode::F64CompareGreaterOrEqual => encode_float_compare(text, false, FloatCompareKind::GreaterOrEqual),
        X86_64OperatorCode::F32CompareEqual => encode_float_compare(text, true, FloatCompareKind::Equal),
        X86_64OperatorCode::F32CompareNotEqual => encode_float_compare(text, true, FloatCompareKind::NotEqual),
        X86_64OperatorCode::F32CompareLessThan => encode_float_compare(text, true, FloatCompareKind::LessThan),
        X86_64OperatorCode::F32CompareLessOrEqual => encode_float_compare(text, true, FloatCompareKind::LessOrEqual),
        X86_64OperatorCode::F32CompareGreaterThan => encode_float_compare(text, true, FloatCompareKind::GreaterThan),
        X86_64OperatorCode::F32CompareGreaterOrEqual => encode_float_compare(text, true, FloatCompareKind::GreaterOrEqual),
    }
}

/// `cmp rax, rcx` + `setcc al` + `movzx eax, al`：结果为 0/1 的 32 位载体。
fn encode_compare_i64(text: &mut Vec<u8>, setcc: u8) {
    text.extend_from_slice(&[0x48, 0x39, 0xC8]);
    text.extend_from_slice(&[0x0F, setcc, 0xC0]);
    text.extend_from_slice(&[0x0F, 0xB6, 0xC0]);
}

/// `cmp eax, ecx` + `setcc al` + `movzx eax, al`：32 位比较，结果 0/1。
fn encode_compare_i32(text: &mut Vec<u8>, setcc: u8) {
    text.extend_from_slice(&[0x39, 0xC8]);
    text.extend_from_slice(&[0x0F, setcc, 0xC0]);
    text.extend_from_slice(&[0x0F, 0xB6, 0xC0]);
}

/// `[rbp + disp32]` 的 ModRM 字节（`mod=10`、`rm=101`，`reg` 为宽度选择的寄存器）。
fn frame_modrm(register: u8) -> u8 {
    0x85 | (register << 3)
}

/// `movsd/movss xmm{register}, [rbp - 8 * (slot + 1)]`。
pub(super) fn emit_float_from_frame(text: &mut Vec<u8>, register: u8, slot: u16, single: bool) {
    let opcode = if single { [0xF3, 0x0F, 0x10, frame_modrm(register)] } else { [0xF2, 0x0F, 0x10, frame_modrm(register)] };
    text.extend_from_slice(&opcode);
    text.extend_from_slice(&frame_disp(slot).to_le_bytes());
}

/// `movsd/movss [rbp - 8 * (slot + 1)], xmm{register}`。
pub(super) fn emit_float_to_frame(text: &mut Vec<u8>, register: u8, slot: u16, single: bool) {
    let opcode = if single { [0xF3, 0x0F, 0x11, frame_modrm(register)] } else { [0xF2, 0x0F, 0x11, frame_modrm(register)] };
    text.extend_from_slice(&opcode);
    text.extend_from_slice(&frame_disp(slot).to_le_bytes());
}

/// 浮点表示转换编码（源/结果寄存器由变体固定）。
pub(super) fn encode_float_cast(text: &mut Vec<u8>, cast: super::X64FloatCast) {
    use super::X64FloatCast;
    match cast {
        // cvtsi2sd/cvtsi2ss xmm0, rax
        X64FloatCast::I64ToF64 => text.extend_from_slice(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]),
        X64FloatCast::I64ToF32 => text.extend_from_slice(&[0xF3, 0x48, 0x0F, 0x2A, 0xC0]),
        // cvttsd2si/cvttss2si rax, xmm0
        X64FloatCast::F64ToI64 => text.extend_from_slice(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]),
        X64FloatCast::F32ToI64 => text.extend_from_slice(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]),
        // cvtss2sd/cvtsd2ss xmm0, xmm0
        X64FloatCast::F32ToF64 => text.extend_from_slice(&[0xF3, 0x0F, 0x5A, 0xC0]),
        X64FloatCast::F64ToF32 => text.extend_from_slice(&[0xF2, 0x0F, 0x5A, 0xC0]),
    }
}

/// `movq rax, xmm0`。
fn emit_movq_rax_from_xmm0(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]);
}

/// `movq xmm0, rax`。
fn emit_movq_xmm0_from_rax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
}

/// 把 `rax` 中的浮点位模式搬进 `xmm0`。
pub(super) fn emit_rax_bits_to_xmm0(text: &mut Vec<u8>, single: bool) {
    if single {
        emit_movd_xmm0_from_eax(text);
    }
    else {
        emit_movq_xmm0_from_rax(text);
    }
}

/// `movd eax, xmm0`。
fn emit_movd_eax_from_xmm0(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x66, 0x0F, 0x7E, 0xC0]);
}

/// `movd xmm0, eax`。
fn emit_movd_xmm0_from_eax(text: &mut Vec<u8>) {
    text.extend_from_slice(&[0x66, 0x0F, 0x6E, 0xC0]);
}

/// 浮点取负：翻转符号位，保留 `-0.0` 与 NaN 载荷（单/双精度各自位宽）。
fn encode_float_negate(text: &mut Vec<u8>, single: bool) {
    if single {
        emit_movd_eax_from_xmm0(text);
        // `btc eax, 31`
        text.extend_from_slice(&[0x0F, 0xBA, 0xF8, 0x1F]);
        emit_movd_xmm0_from_eax(text);
    }
    else {
        emit_movq_rax_from_xmm0(text);
        // `btc rax, 63`
        text.extend_from_slice(&[0x48, 0x0F, 0xBA, 0xF8, 0x3F]);
        emit_movq_xmm0_from_rax(text);
    }
}

/// 浮点比较后的布尔归一：`setcc al` + 额外谓词 + `movzx eax, al`，结果 0/1。
fn encode_float_compare(text: &mut Vec<u8>, single: bool, kind: FloatCompareKind) {
    text.extend_from_slice(if single {
        &[0x0F, 0x2E, 0xC1][..]
    }
    else {
        &[0x66, 0x0F, 0x2E, 0xC1][..]
    });
    match kind {
        // 相等：`sete al && setnp cl`。
        FloatCompareKind::Equal => {
            text.extend_from_slice(&[0x0F, 0x94, 0xC0, 0x0F, 0x9B, 0xC1, 0x20, 0xC8]);
        }
        // 不等：`setne al || setp cl`。
        FloatCompareKind::NotEqual => {
            text.extend_from_slice(&[0x0F, 0x95, 0xC0, 0x0F, 0x9A, 0xC1, 0x08, 0xC8]);
        }
        // 小于：`setb al && setnp cl`。
        FloatCompareKind::LessThan => {
            text.extend_from_slice(&[0x0F, 0x92, 0xC0, 0x0F, 0x9B, 0xC1, 0x20, 0xC8]);
        }
        // 小于等于：`setbe al && setnp cl`。
        FloatCompareKind::LessOrEqual => {
            text.extend_from_slice(&[0x0F, 0x96, 0xC0, 0x0F, 0x9B, 0xC1, 0x20, 0xC8]);
        }
        // 大于：`seta al`（无序时 `CF=1` 已为假）。
        FloatCompareKind::GreaterThan => text.extend_from_slice(&[0x0F, 0x97, 0xC0]),
        // 大于等于：`setae al`（无序时 `CF=1` 已为假）。
        FloatCompareKind::GreaterOrEqual => text.extend_from_slice(&[0x0F, 0x93, 0xC0]),
    }
    text.extend_from_slice(&[0x0F, 0xB6, 0xC0]);
}

/// 浮点比较的谓词族。
#[derive(Debug, Clone, Copy)]
enum FloatCompareKind {
    Equal,
    NotEqual,
    LessThan,
    LessOrEqual,
    GreaterThan,
    GreaterOrEqual,
}
