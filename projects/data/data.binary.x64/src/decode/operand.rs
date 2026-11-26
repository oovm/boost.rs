//! 解码器与格式化器共享的操作数、寄存器与内存引用。

/// 解码器产生的寄存器族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegKind {
    /// 通用寄存器。
    Gpr,
    /// 64 位 MMX 寄存器。
    Mm,
    /// 128 位 SSE 寄存器。
    Xmm,
    /// 256 位 AVX 寄存器。
    Ymm,
    /// 512 位 AVX-512 寄存器。
    Zmm,
    /// 段寄存器。
    Seg,
    /// 控制寄存器。
    Cr,
    /// 调试寄存器。
    Dr,
}

/// 一个寄存器操作数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reg {
    /// 寄存器族。
    pub kind: RegKind,
    /// 操作数宽度（位）；向量寄存器忽略该字段。
    pub size: u32,
    /// 寄存器编号，`0..31`。
    pub num: u8,
    /// 是否带 REX 前缀；REX 会重命名低字节寄存器。
    pub rex: bool,
}

const GPR8: [&str; 16] = ["al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b", "r15b"];
const GPR8_LEGACY: [&str; 8] = ["al", "cl", "dl", "bl", "ah", "ch", "dh", "bh"];
const GPR16: [&str; 16] = ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"];
const GPR32: [&str; 16] =
    ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d", "r15d"];
const GPR64: [&str; 16] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];

impl Reg {
    /// 构造通用寄存器。
    pub(crate) fn gpr(size: u32, num: u8, rex: bool) -> Self {
        Self { kind: RegKind::Gpr, size, num, rex }
    }

    /// 构造 `kind` 族的向量寄存器。
    pub(crate) fn vec(kind: RegKind, num: u8) -> Self {
        Self { kind, size: 0, num, rex: false }
    }

    /// 渲染寄存器名。
    pub(crate) fn text(self) -> String {
        match self.kind {
            RegKind::Gpr => {
                let n = usize::from(self.num & 15);
                match self.size {
                    8 => {
                        if self.num >= 8 || self.rex {
                            GPR8[n].to_string()
                        }
                        else {
                            GPR8_LEGACY[n].to_string()
                        }
                    }
                    16 => GPR16[n].to_string(),
                    32 => GPR32[n].to_string(),
                    _ => GPR64[n].to_string(),
                }
            }
            RegKind::Mm => format!("mm{}", self.num & 7),
            RegKind::Xmm => format!("xmm{}", self.num),
            RegKind::Ymm => format!("ymm{}", self.num),
            RegKind::Zmm => format!("zmm{}", self.num),
            RegKind::Seg => {
                const SEG: [&str; 6] = ["es", "cs", "ss", "ds", "fs", "gs"];
                SEG[usize::from(self.num.min(5))].to_string()
            }
            RegKind::Cr => format!("cr{}", self.num),
            RegKind::Dr => format!("dr{}", self.num),
        }
    }
}

/// 一个内存引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mem {
    /// 访问宽度（位）；`0` 表示指令隐含无宽度。
    pub size: u32,
    /// 段覆盖前缀。
    pub seg: Option<&'static str>,
    /// 基址寄存器名。
    pub base: Option<String>,
    /// 变址寄存器名。
    pub index: Option<String>,
    /// 变址比例，始终为 2 的幂。
    pub scale: u8,
    /// 有符号位移。
    pub disp: i64,
    /// 有效地址是否相对 RIP。
    pub rip: bool,
    /// 有效地址是否为绝对位移。
    pub absolute: bool,
}

impl Mem {
    /// 构造宽度为 `size` 的空内存引用。
    pub(crate) fn new(size: u32) -> Self {
        Self { size, seg: None, base: None, index: None, scale: 1, disp: 0, rip: false, absolute: false }
    }
}

/// 一个已解码的操作数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Operand {
    /// 寄存器操作数。
    Reg(Reg),
    /// 内存操作数。
    Mem(Mem),
    /// 立即数，已按 `size` 位符号扩展。
    Imm {
        /// 立即数宽度（位）。
        size: u32,
        /// 立即数取值，在 `u64` 中零扩展。
        value: u64,
    },
    /// 分支目标绝对地址。
    Rel(u64),
}
