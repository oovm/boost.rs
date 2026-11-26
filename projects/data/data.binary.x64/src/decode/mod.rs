//! 自研 x86 / x64 指令解码，支持 Intel、MASM、NASM 三种语法渲染。
//!
//! 手写表驱动实现，覆盖 PE / Mach-O 代码中常见的编码；未知或畸形编码安全回退为
//! 单字节 `.byte`，任何截断输入都不会 panic。

use format::render;
use opcode::Ctx;
use operand::Operand;
use prefix::Reader;

mod format;
mod modrm;
mod opcode;
mod operand;
mod prefix;
mod vex;

/// 渲染指令时使用的汇编语法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// Intel 语法。
    Intel,
    /// MASM 语法。
    Masm,
    /// NASM 语法。
    Nasm,
}

/// 操作码及其操作数的解码结果。
pub(crate) struct Decoded {
    /// 助记符，可能带有 `lock` 或 `rep` 前缀。
    pub(crate) mnemonic: String,
    /// 按 Intel 操作数顺序排列的操作数。
    pub(crate) operands: Vec<Operand>,
}

impl Decoded {
    /// 构造无操作数的指令。
    pub(crate) fn bare(mnemonic: &'static str) -> Self {
        Self { mnemonic: mnemonic.to_string(), operands: Vec::new() }
    }

    /// 构造带助记符与操作数的指令。
    pub(crate) fn new(mnemonic: &'static str, operands: Vec<Operand>) -> Self {
        Self { mnemonic: mnemonic.to_string(), operands }
    }
}

/// 一条已解码的指令。
#[derive(Debug, Clone)]
pub struct Instruction {
    /// 指令地址。
    pub ip: u64,
    /// 编码长度（字节）。
    pub len: usize,
    /// 字节是否无法解码。
    invalid: bool,
    /// Intel 语法渲染结果。
    intel: String,
    /// MASM 语法渲染结果。
    masm: String,
    /// NASM 语法渲染结果。
    nasm: String,
}

impl Instruction {
    /// 返回 `syntax` 语法下的渲染结果。
    pub fn format(&self, syntax: Syntax) -> &str {
        match syntax {
            Syntax::Intel => &self.intel,
            Syntax::Masm => &self.masm,
            Syntax::Nasm => &self.nasm,
        }
    }

    /// 指令地址。
    pub fn ip(&self) -> u64 {
        self.ip
    }

    /// 编码长度（字节）。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 字节是否无法解码。
    pub fn is_invalid(&self) -> bool {
        self.invalid
    }
}

/// 从字节切片中逐条流出指令。
pub struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
    ip: u64,
    bitness: u32,
}

impl<'a> Decoder<'a> {
    /// 在 `bytes` 上创建解码器，指定位数与首条指令地址。
    pub fn new(bitness: u32, bytes: &'a [u8], ip: u64) -> Self {
        Self { bytes, position: 0, ip, bitness }
    }

    /// 当前字节位置。
    pub fn position(&self) -> usize {
        self.position
    }

    /// 下一条指令的地址。
    pub fn ip(&self) -> u64 {
        self.ip
    }

    /// 将游标移动到 `position`，超出长度时收敛到末尾。
    pub fn set_position(&mut self, position: usize) {
        self.position = position.min(self.bytes.len());
    }

    /// 设置下一条指令的地址。
    pub fn set_ip(&mut self, ip: u64) {
        self.ip = ip;
    }

    /// 是否仍有至少一个字节待解码。
    pub fn can_decode(&self) -> bool {
        self.position < self.bytes.len()
    }

    /// 解码下一条指令。
    pub fn decode(&mut self) -> Instruction {
        let start = self.position;
        let ip = self.ip;
        if start >= self.bytes.len() {
            self.position = self.bytes.len();
            return Instruction { ip, len: 0, invalid: true, intel: String::new(), masm: String::new(), nasm: String::new() };
        }

        let mut reader = Reader::new(self.bytes, start);
        let prefixes = prefix::read_prefixes(&mut reader, self.bitness);
        let ctx = Ctx::new(self.bitness, prefixes, start, ip);
        let decoded = opcode::decode(&mut reader, ctx).ok();
        if decoded.is_none() {
            reader.pos = start + 1;
        }

        let len = reader.pos - start;
        let (mnemonic, operands, invalid) = match decoded {
            Some(decoded) => (decoded.mnemonic, decoded.operands, false),
            None => (String::new(), Vec::<Operand>::new(), true),
        };
        let intel = if invalid { format!(".byte 0x{:02x}", self.bytes[start]) } else { render(Syntax::Intel, &mnemonic, &operands) };
        let masm = if invalid { format!(".byte 0x{:02x}", self.bytes[start]) } else { render(Syntax::Masm, &mnemonic, &operands) };
        let nasm = if invalid { format!(".byte 0x{:02x}", self.bytes[start]) } else { render(Syntax::Nasm, &mnemonic, &operands) };

        self.position = reader.pos;
        self.ip = self.ip.wrapping_add(len as u64);
        Instruction { ip, len, invalid, intel, masm, nasm }
    }
}
