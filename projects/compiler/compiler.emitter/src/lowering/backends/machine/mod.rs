//! Native 产物后端的 lowering 入口。
//!
//! `native` 不是平台，也不是编码身份：它只表示「产物是宿主可直接加载的原生镜像」
//! 这一形态（相对 JVM / CLR / WASM 托管产物）。具体机器码与容器由解析后的规范化
//! 目标四元组 [`CanonicalTarget`]（`arch-vendor-os-abi`）决定——`native`、`node`、
//! `python` 都必须先展开成四元组，再由本模块按架构分派到具体 ISA 后端。
//!
//! 当前闭合两个 x86-64 四元组：
//!
//! - `x86_64-pc-windows-msvc`：Win64 调用约定 + `PE32+` 控制台可执行文件；
//! - `x86_64-unknown-linux-gnu`：SysV64 调用约定 + `ELF64 ET_EXEC` 可执行文件，
//!   入口桩以 `exit(2)` 结束进程。
//!
//! 其余四元组（aarch64、Mach-O 等）一律结构化失败，不借用其他平台的编码或容器。
//! 调用约定（[`X64CallConv`]）与容器一样由 `os`/`abi` 决定，编码器不提供默认 ABI。
//!
//! 算子编码同样按 ISA 冻结（[`x86_64::X86_64OperatorCode`]），不按「native」命名：
//! Node `.node` / Python `.pyd` 只是同一 ISA 上的宿主装载与 ABI 包装形态，不是独立
//! 指令集；宿主调用属于导入合同，不属于算子编码。

mod array_runtime;
mod carrier;
mod container;
mod extension;
mod host;
mod library;
mod x64_lir;
mod x64_program;
pub mod x86_64;

pub use container::compile_shared_container;
pub use extension::{MachineHostExport, X64LoweredExtension, lower_fragment_to_machine_extension};
pub use library::{MachineLibraryExport, X64LoweredLibrary, lower_fragment_to_machine_library};

pub use carrier::{
    HostMethod, HostParameterKind, HostProperty, HostValueKind, PYTHON_EXEC_NAME, X64_TEXT_ARENA_CAPACITY, any_returns_bool, any_returns_utf8,
    append_text_arena, is_c_identifier, node_addon_registration, node_method_wrapper, node_module_data, python_method_wrapper,
    python_module_data, python_module_exec, python_module_registration, reset_text_arena,
};
pub use host::{HostModule, X64Platform, X64ProductTarget};

pub use x64_program::{
    encode_x64_program, encode_x64_program_with_data, encode_x64_program_with_dependencies, encode_x64_program_with_imports,
};

use compiler_target::{
    ArtifactContract, ArtifactPurpose, ResolvedTarget,
    abstractions::{CanonicalAbi, CanonicalArch, CanonicalSpecification, CanonicalTarget},
};

use crate::{FragmentSubmission, Result};

/// x86-64 的整数调用约定。
///
/// 调用约定由规范目标的 `os`/`abi` 决定，不能只按 `arch` 推断：同一个 `x86_64`
/// 在 Windows/PE 上是 Win64，在 Linux/ELF 上是 SysV64。编码器与文本 LIR 都显式
/// 携带本值，不提供「默认 ABI」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X64CallConv {
    /// Win64（x86-64 Windows / `PE32+`）：`rcx`/`rdx`/`r8`/`r9`，调用点上方 32 字节 shadow space。
    Win64,
    /// SysV64（x86-64 Linux / `ELF64`）：`rdi`/`rsi`/`rdx`/`rcx`/`r8`/`r9`，无 shadow space。
    SystemV64,
}

impl X64CallConv {
    /// 调用前调用方必须预留的 shadow space 字节数（Win64 为 32，SysV 为 0）。
    pub fn shadow_space(self) -> u32 {
        match self {
            Self::Win64 => 32,
            Self::SystemV64 => 0,
        }
    }

    /// 寄存器可承载的整数实参上限（超出必须走栈，本 lane 尚未实现栈传参）。
    pub fn integer_argument_limit(self) -> usize {
        match self {
            Self::Win64 => 4,
            Self::SystemV64 => 6,
        }
    }

    /// 寄存器可承载的浮点实参上限（`xmm0..`；超出必须走栈，本 lane 尚未实现栈传参）。
    ///
    /// Win64 调用约定按 Microsoft x64 规则为每个形参同时占用整数与浮点寄存器
    /// 文件的同一序位（第 i 个形参：`rcx+i` 与 `xmm0+i` 一并分配），因此浮点
    /// 实参的寄存器序位 = 形参位置；SysV64 两个寄存器文件各自独立编号。
    pub fn float_argument_limit(self) -> usize {
        match self {
            Self::Win64 => 4,
            Self::SystemV64 => 8,
        }
    }

    /// 整数与浮点实参寄存器是否按形参位置共享序位。
    ///
    /// Win64 调用约定按 Microsoft x64 规则为每个形参同时占用整数与浮点寄存器
    /// 文件的同一序位（第 i 个形参：`rcx+i` 与 `xmm0+i` 一并分配）；SysV64
    /// 两个寄存器文件各自独立编号。
    pub fn shares_register_files(self) -> bool {
        match self {
            Self::Win64 => true,
            Self::SystemV64 => false,
        }
    }

    /// 帧底必须为调用约定预留的字节数。
    ///
    /// Win64 的 32 字节 shadow space 同时充当块参数周转区；SysV64 没有 shadow
    /// space，块参数周转需要独立空间（6 个寄存器实参 × 8 字节）。
    pub fn frame_reserve(self) -> usize {
        match self {
            Self::Win64 => 32,
            Self::SystemV64 => 6 * 8,
        }
    }

    /// 调用约定的稳定名称，用于结构化错误。
    pub fn name(self) -> &'static str {
        match self {
            Self::Win64 => "Win64",
            Self::SystemV64 => "SysV64",
        }
    }
}

/// 降低结果：完整原生镜像字节与入口位置事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X64LoweredProgram {
    /// 完整镜像字节（`PE32+` 或 `ELF64 ET_EXEC`，由规范目标决定）。
    pub executable: Vec<u8>,
    /// 入口点在文本节内的偏移。
    pub entry_offset: u32,
    /// 文本节原始长度（未按文件对齐填充）。
    pub text_len: usize,
}

/// 浮点表示转换（SSE2 单指令；源与结果寄存器由变体名固定）。
///
/// 整数侧统一按 64 位有符号值转换：调用方先把窄整数按自身符号性规范化到
/// 64 位，再选择这里的变体；浮点转整数使用向零截断，越界/NaN 得到 x86 的
/// 64 位不定值 `0x8000_0000_0000_0000`（ISA 冻结行为，不做饱和伪装）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X64FloatCast {
    /// `cvtsi2sd xmm0, rax`：`i64`/已规范化的窄整数 → `f64`。
    I64ToF64,
    /// `cvtsi2ss xmm0, rax`：`i64`/已规范化的窄整数 → `f32`（按目标精度舍入）。
    I64ToF32,
    /// `cvttsd2si rax, xmm0`：`f64` → 64 位有符号整数（向零截断）。
    F64ToI64,
    /// `cvttss2si rax, xmm0`：`f32` → 64 位有符号整数（向零截断）。
    F32ToI64,
    /// `cvtss2sd xmm0, xmm0`：`f32` → `f64`（精确提升）。
    F32ToF64,
    /// `cvtsd2ss xmm0, xmm0`：`f64` → `f32`（按目标精度舍入，保留 `-0.0`/NaN 符号）。
    F64ToF32,
}

/// 适配器可以提交的原生文本 LIR 指令。
///
/// 这是语言/平台适配器复用 native 平台编码能力的公开入口：适配器保留自己的
/// 语言语义与 residual，只把已决定的目标操作交给本 lane 的 LIR/编码器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X64TextInstruction {
    /// `mov rax, imm64`：整数值进入返回值寄存器。
    LoadI64(i64),
    /// 函数返回（含帧还原）；返回值已在 `rax`。
    Return,
    /// 把目标 ABI 的整数形参 `index` 存入帧槽 `slot`。
    ///
    /// 寄存器由 [`X64CallConv`] 决定：Win64 为 rcx/rdx/r8/r9，SysV64 为
    /// rdi/rsi/rdx/rcx/r8/r9。
    LoadParam {
        /// 形参序号（uint 序；超过该 ABI 的寄存器上限即失败）。
        index: u8,
        /// 帧槽。
        slot: u16,
    },
    /// `mov rax, [slot]`。
    LoadSlot(u16),
    /// `mov rcx, [slot]`（二元运算的右操作数）。
    LoadSlotToRcx(u16),
    /// 把帧槽 `slot` 装入目标 ABI 的整数实参寄存器 `index`（调用前）。
    LoadSlotToArg {
        /// 帧槽。
        slot: u16,
        /// 实参序号（按 ABI 寄存器序；超过上限即失败）。
        index: u8,
    },
    /// 把目标 ABI 的浮点形参 `index`（`xmm{index}`）存入帧槽 `slot`。
    LoadParamXmm {
        /// 浮点形参序号（独立于整数序；超过该 ABI 的 `xmm` 上限即失败）。
        index: u8,
        /// 帧槽。
        slot: u16,
        /// 宽度：`true` 为 `f32`（`movss`），`false` 为 `f64`（`movsd`）。
        single: bool,
    },
    /// 把帧槽 `slot` 装入目标 ABI 的浮点实参寄存器 `xmm{index}`（调用前）。
    LoadSlotToXmmArg {
        /// 帧槽。
        slot: u16,
        /// 浮点实参序号（独立于整数序；超过上限即失败）。
        index: u8,
        /// 宽度：`true` 为 `f32`（`movss`），`false` 为 `f64`（`movsd`）。
        single: bool,
    },
    /// `movsd/movss xmm0, [slot]`：浮点算子左操作数。
    LoadSlotToXmm0 {
        /// 帧槽。
        slot: u16,
        /// 宽度：`true` 为 `f32`（`movss`），`false` 为 `f64`（`movsd`）。
        single: bool,
    },
    /// `movsd/movss xmm1, [slot]`：浮点算子右操作数。
    LoadSlotToXmm1 {
        /// 帧槽。
        slot: u16,
        /// 宽度：`true` 为 `f32`（`movss`），`false` 为 `f64`（`movsd`）。
        single: bool,
    },
    /// `movsd/movss [slot], xmm0`：浮点结果落槽 / 浮点返回装载。
    StoreXmm0ToSlot {
        /// 帧槽。
        slot: u16,
        /// 宽度：`true` 为 `f32`（`movss`），`false` 为 `f64`（`movsd`）。
        single: bool,
    },
    /// 把 `rax` 中的浮点位模式搬进 `xmm0`（浮点转换的源装载）。
    LoadRaxToXmm0 {
        /// 宽度：`true` 为 `f32`（`movd`），`false` 为 `f64`（`movq`）。
        single: bool,
    },
    /// 显式浮点表示转换（源在 `rax` 或 `xmm0`，结果按变体约定）。
    CastFloat(X64FloatCast),
    /// `mov [slot], rax`。
    StoreSlot(u16),
    /// `rax = rax + rcx`。
    Add,
    /// `rax = rax - rcx`。
    Sub,
    /// `rax = rax * rcx`。
    Mul,
    /// `rax = (rax == rcx) as i64`。
    Eq,
    /// `rax = (rax != rcx) as i64`。
    Ne,
    /// `rax = (rax < rcx) as i64`（有符号）。
    Lt,
    /// `rax = (rax <= rcx) as i64`。
    Le,
    /// `rax = (rax > rcx) as i64`。
    Gt,
    /// `rax = (rax >= rcx) as i64`。
    Ge,
    /// 按冻结 x86-64 operator 编码执行（操作数已按约定位于 `rax`/`rcx`）。
    ///
    /// 正式 lane 的计划降低携带 [`x86_64::X86_64OperatorCode`] 全量算子集；
    /// 上面的固定变体是残差语言的整数便捷别名，两者经同一字节表编码。
    Operator(x86_64::X86_64OperatorCode),
    /// `movsx rax, al`：8 位有符号拓宽到 64 位（显式 cast）。
    SignExtend8,
    /// `movsx rax, ax`：16 位有符号拓宽到 64 位（显式 cast）。
    SignExtend16,
    /// `movsxd rax, eax`：32 位有符号拓宽到 64 位（显式 cast）。
    SignExtend32,
    /// `movzx eax, al`：8 位无符号拓宽到 64 位（显式 cast）。
    ZeroExtend8,
    /// `movzx eax, ax`：16 位无符号拓宽到 64 位（显式 cast）。
    ZeroExtend16,
    /// `mov eax, eax`：32 位无符号拓宽到 64 位（显式 cast）。
    ZeroExtend32,
    /// `rax = (rax != 0)`：整数/字符转布尔。
    ToBool,
    /// 函数内标签（数值 id）。
    Label(u32),
    /// 无条件跳转。
    Jump(u32),
    /// `test rax, rax` 后为零跳转。
    JumpIfZero(u32),
    /// 调用同程序内命名函数（`rel32` 由程序编码器回填）。
    Call(String),
    /// 把静态字符串的描述符地址装入 `rax`。
    ///
    /// 描述符与 UTF-8 字节由程序编码器内联在同一个 `.text` 里：
    /// `{ i32 self_relative_ptr; u32 len }` 紧跟字节。宿主按
    /// `descriptor + self_relative_ptr` 读取内容，所有权归镜像本身（只读、永不释放）。
    LoadString(String),
    /// 通过宿主导入调用函数（`call [rip+disp32]`）。
    ///
    /// 编码按目标 ABI 冻结：Win64 走 `.idata` 的 IAT 槽并要求 `module` 为宿主模块名；
    /// SysV64 走未定义动态符号 + 可写 GOT 槽（`R_X86_64_GLOB_DAT`），
    /// **`module` 必须为空**——ELF 导入按装载进程的全局符号作用域解析，不按模块名装载。
    CallImport {
        /// 宿主模块名（仅 Win64 使用）。
        module: String,
        /// 导入函数名。
        field: String,
    },
    /// 把静态字符串的**字节指针**装入 `rax`（描述符 + 8）。
    LoadStringPtr(String),
    /// 把静态字符串的**字节长度**装入 `rax`。
    LoadStringLen(String),
    /// 把 `rax` 指向的字符串描述符里的 UTF-8 长度装入 `rax`。
    ///
    /// 描述符布局为 `{ i32 self_relative_ptr; u32 len }`（见 [`Self::LoadString`]）：
    /// 该指令读取 `[rax + 4]` 的 `u32` 长度，用于把运行时得到的描述符地址转成
    /// 宿主字符串（`PyUnicode_FromStringAndSize` / `napi_create_string_utf8`）。
    LoadDescriptorLength,
    /// 把镜像数据段内 `offset` 处的地址装入 `rax`（`lea rax, [rip+disp32]`）。
    ///
    /// 数据段内容属于适配器（例如 CPython `PyModuleDef`）；容器写出器把
    /// `.text` 内的 `disp32` 回填到 `.data` 目标，并在数据段布局确定后应用
    /// 自相对重定位。`offset` 超出数据段即失败闭合。
    LoadDataAddress(u32),
    /// 把同程序内命名函数的地址装入 `rax`（`lea rax, [rip+disp32]`）。
    ///
    /// 用于把函数指针作为值传给宿主（例如 Node-API `napi_create_function` 的
    /// 回调）。目标与本指令同处 `.text`，编码器在布局完成后直接解析，不需要
    /// 容器参与。
    LoadFunctionAddress(String),
    /// 以目标 OS 的 `write` 原语写 `(fd, ptr, len)` 到宿主 effect 通道。
    ///
    /// SysV64/Linux 编码为 `write(2)` 系统调用，实参按调用约定位于
    /// `rdi`/`rsi`/`rdx`；Win64 没有对应的 syscall 合同，遇到该指令显式失败，
    /// Windows 的 effect 写入必须走 `CallImport` 的 IAT。
    SyscallWrite,
    /// SysV64/Linux `mmap(2)`：长度取自 `rax`，返回地址（失败为负 errno）写回 `rax`。
    ///
    /// 只归数组运行时使用；Win64 没有对应合同，遇到该指令显式失败。
    SyscallMmap,
    /// SysV64/Linux `munmap(2)`：地址在 `rax`、长度在 `rcx`；返回值忽略。
    ///
    /// 只归数组运行时使用；Win64 遇到该指令显式失败。
    SyscallMunmap,
    /// 把 Win64 栈上第 `index` 个额外实参（`[rsp + 32 + 8*index]`）置零。
    ///
    /// 用于第 5 个及以后的整数实参（前 4 个走 rcx/rdx/r8/r9）。这是 Win64
    /// shadow space 专属合同；SysV64 文本 ABI 遇到该指令显式失败。
    ZeroStackArg(u8),
    /// 把帧槽 `slot` 的值写入 Win64 栈上第 `index` 个额外实参（`[rsp + 32 + 8*index]`）。
    ///
    /// 与 [`Self::ZeroStackArg`] 同属 Win64 栈传参合同；SysV64 显式失败。
    /// 编码器按这些指令自动为函数预留栈实参空间。
    StoreSlotToStackArg {
        /// 帧槽。
        slot: u16,
        /// 栈实参序号（从第 5 个实参起）。
        index: u8,
    },
    /// 把帧槽 `slot` 的**地址**写入 Win64 栈上第 `index` 个额外实参。
    ///
    /// 宿主的 out-参数（例如 `napi_create_function` 的 `napi_value* result`）
    /// 需要指针而不仅是值。SysV64 显式失败。
    StoreSlotAddressToStackArg {
        /// 帧槽。
        slot: u16,
        /// 栈实参序号（从第 5 个实参起）。
        index: u8,
    },
    /// `lea rax, [rbp - 8*(slot+1)]`：取帧槽地址（例如字节缓冲区起点）。
    LoadSlotAddress(u16),
    /// `add rax, imm32`。
    AddRaxImmediate(i32),
    /// `sub rax, imm32`。
    SubRaxImmediate(i32),
    /// `neg rax`。
    NegateRax,
    /// `rax = rax / 10`，余数进入 `rdx`（无符号除法；调用方保证非负）。
    DivideByTen,
    /// `add rdx, imm32`（例如把余数转成 ASCII）。
    AddRdxImmediate(i32),
    /// `mov [rax], dl`：把 `rdx` 的低字节写入 `rax` 指向的缓冲区。
    StoreByteAtRaxFromRdx,
    /// `movzx eax, byte [rax]`：读取 `rax` 指向的单字节（零扩展到 64 位）。
    ///
    /// 用于字符串的字节序比较；描述符里的 UTF-8 字节由适配器负责定位。
    LoadByteFromRax,
    /// `mov rax, [rax]`：读取 `rax` 指向的机器字（例如数据段的 arena 游标）。
    LoadFromRax,
    /// `mov [rax], rcx`：把 `rcx` 写入 `rax` 指向的机器字（arena 游标更新）。
    StoreRcxToRax,
    /// `mov [rax + disp32], rcx`：把 `rcx` 写入 `rax + disp32`（数组头部的分配字节数）。
    StoreRcxToRaxDisplacement(i32),
    /// `mov rax, [rax + disp32]`：读取 `rax + disp32` 处的机器字（数组头部的分配字节数）。
    LoadFromRaxDisplacement(i32),
    /// `mov [rax + disp32], rdx`：把 `rdx` 写入 `rax + disp32`（对象字段 / sum 载荷）。
    StoreRdxToRaxDisplacement(i32),
    /// `mov eax, [rax]`：读取 32 位并零扩展到 `rax`（sum tag）。
    LoadDwordFromRax,
    /// `mov [rcx], rax`：把 `rax` 写入 `rcx` 指向的机器字（入口处重置游标）。
    StoreRaxToRcx,
    /// `mov dword ptr [rax], imm32`：写描述符里的自相对指针字段。
    StoreDwordImmediateToRax(u32),
    /// `mov [rax], ecx`：写描述符里的 `u32` 长度字段。
    StoreDwordRcxToRax,
    /// `mov rdx, [slot]`：把帧槽装入 `rdx`（`CopyBytes` 的长度实参）。
    LoadSlotToRdx(u16),
    /// `shl rax, imm8`：按固定位数左移（数组元素 8 字节槽的容量换算）。
    ShiftLeftRaxImmediate(u8),
    /// `mov [rax], rdx`：把 `rdx` 写入 `rax` 指向的机器字（数组头部长度）。
    StoreRdxToRax,
    /// `mov rax, [rax + rcx*8 + 16]`：按 `rcx` 索引读取数组元素。
    ///
    /// 数组表示由数组运行时合同冻结：`[0]` 是 `i64` 元素个数、`[8]` 是分配字节数，
    /// 元素从 `+16` 起按 8 字节槽连续排列（见 [`array_runtime`]）。本指令不隐式
    /// 检查越界；两个访问入口都必须先经过 [`array_runtime::array_bounds_check`]，
    /// 或携带可验证的范围证明（常量下标 / 以长度为界的循环）。
    LoadIndexedFromRaxRcx,
    /// `mov [rax + rcx*8 + 16], rdx`：按 `rcx` 索引写入数组元素（同一检查前提）。
    StoreIndexedRdxToRaxRcx,
    /// 复制字节：`rax` = 目标、`rcx` = 源、`rdx` = 长度。
    ///
    /// 编码器负责保存/恢复被 SysV64 与 Win64 共同视为被调用者保存的
    /// `rdi`/`rsi`，并用 `rep movsb` 完成复制；`rcx`/`rdx` 按调用约定属于
    /// 调用者保存，复制后内容不再保证。
    CopyBytes,
    /// `ud2`：以陷入方式失败闭合（例如镜像文本 arena 预算耗尽）。
    ///
    /// 产品 ABI 没有异常通道；资源预算耗尽必须显式终止，不能返回伪造的空值。
    Trap,
}

/// 多函数文本程序中的一个函数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X64ProgramFunction {
    /// 函数名（导出符号与调用目标）。
    pub name: String,
    /// 帧槽数量（每槽 8 字节）。
    pub slot_count: u16,
    /// 目标 LIR。
    pub instructions: Vec<X64TextInstruction>,
}

/// 数据段自相对指针的目标。
///
/// 数据段起点由容器（页对齐）决定，适配器无法提前知道最终虚拟地址，所以目标按
/// 「节 + 节内偏移」声明，由容器换算成自相对重定位的加数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X64DataTarget {
    /// `.text` 内偏移（例如导出函数地址）。
    Text(u32),
    /// `.data` 内偏移（例如模块名、常量名）。
    Data(u32),
    /// 命名函数（例如方法表里的 `ml_meth`）：由编码器在布局完成后解析成符号偏移，
    /// 适配器不需要预测函数在 `.text` 中的位置。
    Function(String),
}

/// 多函数文本程序编码结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X64ProgramImage {
    /// `.text` 节字节。
    pub text: Vec<u8>,
    /// 导出符号及其在 `.text` 内的偏移。
    pub symbols: Vec<(String, u32)>,
    /// `.idata` 字节（由宿主导入表构造，段内偏移尚未加 RVA）。
    pub idata: Vec<u8>,
    /// 按 DLL 归组的导入描述。
    pub imports: Vec<data_binary_pe::pe::NativeDllImport>,
    /// `.text` 内的 `rel32` 修复项：`(字段偏移, `.idata` 内目标偏移)`（Win64）。
    pub text_rel32_fixups: Vec<(u32, u32)>,
    /// `.text` 内的 GOT 间接调用修复项：`(字段偏移, 导入序号)`（SysV64）。
    ///
    /// 容器（`ELF` 写出器）放置可写 GOT 槽后，把 `call [rip+disp32]` 回填到槽地址。
    pub elf_got_fixups: Vec<(u32, u32)>,
    /// SysV64 的动态导入符号名，下标与 `elf_got_fixups` 的导入序号一致。
    pub elf_imports: Vec<String>,
    /// `.data` 节内容（可读写、文件内初始化；容器保证 `p_vaddr == 文件偏移`）。
    pub data: Vec<u8>,
    /// `.data` 内的自相对指针重定位：`(数据段偏移, 目标节与偏移)`。
    pub data_relocations: Vec<(u32, X64DataTarget)>,
    /// `.text` 内指向 `.data` 的 `rel32` 引用：`(字段偏移, 数据段偏移)`。
    pub data_references: Vec<(u32, u32)>,
    /// 产物声明依赖的宿主库 `SONAME`（`ELF` 写成 `DT_NEEDED`；`PE` 走导入表，忽略）。
    pub host_dependencies: Vec<String>,
}

/// 正式 lane：把已冻结目标降低为原生进程镜像。
///
/// 只接受进程可执行用途（`Application` / `TestBundle`）与普通原生合同：库交付与
/// 宿主扩展分别由库 lane 与宿主扩展降低承载，在这里结构化拒绝，不把进程镜像
/// 改标成其他产物。入口身份必须已由计划冻结；缺失时由 lowering 报错，不回填
/// 默认名。
pub fn lower_resolved_fragment_to_machine_image(
    submission: &FragmentSubmission,
    resolved: &ResolvedTarget,
    purpose: ArtifactPurpose,
) -> Result<X64LoweredProgram> {
    match purpose {
        ArtifactPurpose::Application | ArtifactPurpose::TestBundle => {}
        ArtifactPurpose::Library => {
            return Err("机器码库交付尚未接线：拒绝把进程镜像改标为库产物".to_string());
        }
        ArtifactPurpose::HostExtension => {
            return Err("宿主扩展必须走宿主扩展降低：进程镜像不接受宿主扩展用途".to_string());
        }
    }
    match &resolved.contract {
        ArtifactContract::Native => {}
        ArtifactContract::Platform => {
            return Err(format!("机器码进程镜像不接受平台合同 `{}`：请使用 `native` 或显式机器目标", resolved.identity()));
        }
        ArtifactContract::NodeApi | ArtifactContract::Cpython(_) => {
            return Err(format!(
                "机器码进程镜像不接受 `{}` 宿主合同：宿主扩展必须走宿主扩展降低",
                resolved.contract.as_str()
            ));
        }
    }
    lower_fragment_to_machine_image(submission, resolved.platform)
}

/// 按解析后的规范目标四元组降低为原生镜像。
///
/// 四元组是唯一分派依据；`TargetFamily::MachineCode` 只说明产物形态，不能用来选编码、
/// 容器或 ABI。未实现的目标四元组必须失败闭合，指名具体四元组。
pub(crate) fn lower_fragment_to_machine_image(submission: &FragmentSubmission, target: CanonicalTarget) -> Result<X64LoweredProgram> {
    match target.arch {
        CanonicalArch::X86_64 => match target.specification {
            CanonicalSpecification::Windows => match target.abi {
                // x86-64 Windows 的 MSVC 与 GNU 目标共用 Win64 调用约定与 PE 容器。
                None | Some(CanonicalAbi::Msvc) | Some(CanonicalAbi::Gnu) => x86_64::lower_x86_64_windows_pe(submission),
                Some(other) => Err(format!(
                    "机器码后端不支持 `x86_64` + `windows` + `{}` 的 ABI：只闭合 Win64 调用约定（target={}）",
                    abi_name(other),
                    target.as_canonical_str()
                )),
            },
            // x86-64 Linux 只有 SysV64 调用约定 + ELF 容器；不借用 Win64 编码。
            CanonicalSpecification::Linux => match target.abi {
                None | Some(CanonicalAbi::Gnu) | Some(CanonicalAbi::SystemV) => x86_64::lower_x86_64_linux_elf(submission),
                Some(other) => Err(format!(
                    "机器码后端不支持 `x86_64` + `linux` + `{}` 的 ABI：只闭合 SysV64 调用约定（target={}）",
                    abi_name(other),
                    target.as_canonical_str()
                )),
            },
            other => Err(format!(
                "机器码后端没有 `x86_64` + `{}` 的容器实现：拒绝借用其他平台的镜像格式（target={}）",
                specification_name(other),
                target.as_canonical_str()
            )),
        },
        other => Err(format!("机器码后端尚未实现架构 `{}` 的机器码降低（target={}）", other.as_str(), target.as_canonical_str())),
    }
}

fn specification_name(specification: CanonicalSpecification) -> &'static str {
    match specification {
        CanonicalSpecification::Unknown => "unknown",
        CanonicalSpecification::Browser => "browser",
        CanonicalSpecification::Wasi => "wasi",
        CanonicalSpecification::Windows => "windows",
        CanonicalSpecification::Linux => "linux",
        CanonicalSpecification::MacOs => "macos",
        CanonicalSpecification::Android => "android",
        CanonicalSpecification::Ios => "ios",
    }
}

fn abi_name(abi: CanonicalAbi) -> &'static str {
    match abi {
        CanonicalAbi::Managed => "managed",
        CanonicalAbi::Clr => "clr",
        CanonicalAbi::Jvm => "jvm",
        CanonicalAbi::WebAssembly => "wasm",
        CanonicalAbi::Wasi => "wasi",
        CanonicalAbi::WasiP3 => "wasip3",
        CanonicalAbi::Msvc => "msvc",
        CanonicalAbi::Gnu => "gnu",
        CanonicalAbi::SystemV => "systemv",
        CanonicalAbi::Aapcs64 => "aapcs64",
    }
}
