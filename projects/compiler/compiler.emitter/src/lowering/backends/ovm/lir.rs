//! OVM 目标私有低层 IR（LIR）、字节码发射器与 `.odin` 函数体编码器。
//!
//! 所有权边界（01 §5.5）：MIR 的解读只发生在语言侧 OVM lane；本模块持有目标私有
//! 指令序列与字节编码，不读取共享程序函数体、计划、布局或类型事实。
//! 块标签就是 MIR 块的 `u32` 身份，跳转目标在 LIR 内显式，编码器只按标签偏移
//! 回填 `operand1`，不猜测控制流。

use std::collections::BTreeMap;

use crate::Result;
use data_binary_odin::{IntrinsicOpcode, OdinConstant, OdinHeadCode, OdinImport, OdinImportKind, emit_imm1, emit_imm2, emit_plain};

/// 宿主 builtin 导入模块名（链接名在 imports section；热路径只用下标）。
pub const HOST_IMPORT_MODULE: &str = "odin.host";

/// OVM 私有文本载体桥：raw 宿主串 → `[u8]` 字节数组（不补 NUL、不截断）。
pub const TEXT_TO_BYTES_IMPORT: &str = "utf8.to_bytes";

/// OVM 私有文本载体桥：`[u8]` 字节数组 → raw 宿主串（不要求 NUL 终止）。
pub const TEXT_FROM_BYTES_IMPORT: &str = "utf8.from_bytes";

/// 单条 OVM LIR 指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OdinLirInstruction {
    /// 无立即数头码。
    Plain(OdinHeadCode),
    /// 单立即数头码（`Imm1` 形）。
    Imm1(OdinHeadCode, i32),
    /// 双立即数头码（`Imm2` 形）。
    Imm2(OdinHeadCode, i32, i32),
    /// 函数内块标签（MIR 块身份）；不产生字节，仅供跳转回填。
    Label(u32),
    /// 跳转到函数内块：编码器写入 `opcode` 与 `target - position`。
    Jump { opcode: OdinHeadCode, target: u32 },
    /// 组合序列内部的合成标签；不产生字节，与 MIR 块标签互不占用编号空间。
    SyntheticLabel(u32),
    /// 跳转到合成标签：编码器写入 `opcode` 与 `target - position`。
    JumpTo { opcode: OdinHeadCode, label: u32 },
}

impl OdinLirInstruction {
    /// 该指令在字节流中的固定长度；标签为 0。
    fn encoded_len(&self) -> usize {
        match self {
            Self::Label(_) | Self::SyntheticLabel(_) => 0,
            Self::Plain(_) => 1,
            Self::Imm1(..) | Self::Jump { .. } | Self::JumpTo { .. } => 5,
            Self::Imm2(..) => 9,
        }
    }
}

/// 把一个函数的 LIR 编码为函数体字节。
///
/// 跳转位移与既有 `.odin` 合同一致：`operand1 = target_offset - jump_offset`，
/// 两者都相对函数体起点。缺失标签是 lane/LIR 不一致，结构化失败。
pub fn encode_odin_function(instructions: &[OdinLirInstruction]) -> Result<Vec<u8>> {
    let mut label_offsets = BTreeMap::<u32, i32>::new();
    let mut synthetic_offsets = BTreeMap::<u32, i32>::new();
    let mut offset = 0i32;
    for instruction in instructions {
        match instruction {
            OdinLirInstruction::Label(block) => {
                label_offsets.insert(*block, offset);
            }
            OdinLirInstruction::SyntheticLabel(label) => {
                synthetic_offsets.insert(*label, offset);
            }
            _ => {}
        }
        offset = advance(offset, instruction)?;
    }

    let mut code = Vec::<u8>::with_capacity(usize::try_from(offset).unwrap_or(0));
    let mut position = 0i32;
    for instruction in instructions {
        match instruction {
            OdinLirInstruction::Plain(opcode) => emit_plain(&mut code, *opcode),
            OdinLirInstruction::Imm1(opcode, operand) => emit_imm1(&mut code, *opcode, *operand),
            OdinLirInstruction::Imm2(opcode, first, second) => emit_imm2(&mut code, *opcode, *first, *second),
            OdinLirInstruction::Label(_) | OdinLirInstruction::SyntheticLabel(_) => {}
            OdinLirInstruction::Jump { opcode, target } => {
                let target_offset = label_offsets.get(target).copied().ok_or_else(|| format!("OVM LIR 编码：跳转目标块 {target} 缺少标签"))?;
                emit_imm1(&mut code, *opcode, target_offset - position);
            }
            OdinLirInstruction::JumpTo { opcode, label } => {
                let target_offset =
                    synthetic_offsets.get(label).copied().ok_or_else(|| format!("OVM LIR 编码：跳转目标合成标签 {label} 缺少定义"))?;
                emit_imm1(&mut code, *opcode, target_offset - position);
            }
        }
        position = advance(position, instruction)?;
    }
    Ok(code)
}

fn advance(position: i32, instruction: &OdinLirInstruction) -> Result<i32> {
    let length = i32::try_from(instruction.encoded_len()).map_err(|_| "OVM LIR 编码：函数体长度溢出 `i32`".to_string())?;
    position.checked_add(length).ok_or_else(|| "OVM LIR 编码：函数体长度溢出 `i32`".to_string())
}

/// 逐函数字节码发射器：常量池、LIR 指令序列与宿主导入表的一次性结算。
///
/// 只记录指令与标签；`.odin` 函数体字节在函数降低完成后由
/// [`encode_odin_function`] 一次编码。
pub struct BytecodeEmitter<'a> {
    /// 本函数新增的常量（发射期追加，完成后并入模块常量池）。
    pub constants: Vec<OdinConstant>,
    /// 目标私有 LIR：降低期只记录指令与标签，编码在函数降低完成后一次完成。
    pub instructions: Vec<OdinLirInstruction>,
    /// 模块常量池的函数内基址。
    pub constants_base: i32,
    /// 共享的模块导入表。
    pub imports: &'a mut Vec<OdinImport>,
    /// 组合序列合成标签的分配计数器；与 MIR 块标签互不占用编号空间。
    next_label: u32,
}

impl<'a> BytecodeEmitter<'a> {
    /// 以模块常量池基址与共享导入表构造发射器。
    pub fn new(constants_base: i32, imports: &'a mut Vec<OdinImport>) -> Self {
        Self { constants: Vec::new(), instructions: Vec::new(), constants_base, imports, next_label: 0 }
    }

    /// 登记一个字符串常量并返回模块常量池下标。
    pub fn intern_string(&mut self, value: &str) -> i32 {
        let index = self.constants.len() as i32;
        self.constants.push(OdinConstant::String(value.to_string()));
        index + self.constants_base
    }

    /// 登记一个 i32 常量并返回模块常量池下标（复用 `constants_base`）。
    pub fn intern_i32(&mut self, value: i32) -> i32 {
        let index = self.constants.len() as i32;
        self.constants.push(OdinConstant::Integer32(value));
        index + self.constants_base
    }

    /// 发射无立即数头码。
    pub fn emit_plain(&mut self, opcode: OdinHeadCode) {
        self.instructions.push(OdinLirInstruction::Plain(opcode));
    }

    /// 发射单立即数头码。
    pub fn emit_imm1(&mut self, opcode: OdinHeadCode, operand: i32) {
        self.instructions.push(OdinLirInstruction::Imm1(opcode, operand));
    }

    /// 发射双立即数头码。
    pub fn emit_imm2(&mut self, opcode: OdinHeadCode, first: i32, second: i32) {
        self.instructions.push(OdinLirInstruction::Imm2(opcode, first, second));
    }

    /// 幂等登记宿主 builtin 导入，返回 imports 表下标。
    pub fn ensure_host_import(&mut self, symbol: &str) -> i32 {
        if let Some((index, _)) =
            self.imports.iter().enumerate().find(|(_, import)| import.module_name == HOST_IMPORT_MODULE && import.symbol_name == symbol)
        {
            return index as i32;
        }
        let index = self.imports.len() as i32;
        self.imports.push(OdinImport {
            kind: OdinImportKind::Function,
            module_name: HOST_IMPORT_MODULE.to_string(),
            symbol_name: symbol.to_string(),
        });
        index
    }

    /// 按 imports 表下标调用宿主能力；operand1 = import index，operand2 = argc。
    pub fn emit_call_import(&mut self, symbol: &str, arg_count: i32) {
        let import_index = self.ensure_host_import(symbol);
        self.emit_imm2(OdinHeadCode::CallImport, import_index, arg_count);
    }

    /// 按 [`IntrinsicOpcode`] 编码调用内置；operand1 = 指令编号，operand2 = argc。
    pub fn emit_call_intrinsic(&mut self, op: IntrinsicOpcode, arg_count: i32) {
        self.emit_imm2(OdinHeadCode::CallIntrinsic, op.raw(), arg_count);
    }

    /// 记录一次块跳转；位移由编码器按标签偏移回填。
    pub fn emit_jump_to_block(&mut self, opcode: OdinHeadCode, target: u32) {
        self.instructions.push(OdinLirInstruction::Jump { opcode, target });
    }

    /// 记录一个 MIR 块标签。
    pub fn emit_block_label(&mut self, block: u32) {
        self.instructions.push(OdinLirInstruction::Label(block));
    }

    /// 分配一个组合序列内部的合成标签。
    pub fn alloc_label(&mut self) -> u32 {
        let label = self.next_label;
        self.next_label += 1;
        label
    }

    /// 在当前位置定义合成标签。
    pub fn emit_label(&mut self, label: u32) {
        self.instructions.push(OdinLirInstruction::SyntheticLabel(label));
    }

    /// 跳转到合成标签；位移由编码器按标签偏移回填。
    pub fn emit_jump_to_label(&mut self, opcode: OdinHeadCode, label: u32) {
        self.instructions.push(OdinLirInstruction::JumpTo { opcode, label });
    }

    /// 压入 i32 常量。
    pub fn emit_const_i32(&mut self, value: i32) {
        let index = self.intern_i32(value);
        self.emit_imm1(OdinHeadCode::Constant, index);
    }

    /// 压入 i64 常量。
    pub fn emit_const_i64(&mut self, value: i64) {
        if let Ok(value) = i32::try_from(value) {
            self.emit_const_i32(value);
            self.emit_call_import("i32_to_i64", 1);
            return;
        }
        // 超出 `i32` 表示范围必须用 `Integer64`：`value as i32` + `i32_to_i64`
        // 只保留低 32 位并符号扩展，会把 `i64::MAX` 静默编成 `-1`。
        let index = self.constants.len() as i32 + self.constants_base;
        self.constants.push(OdinConstant::Integer64(value));
        self.emit_imm1(OdinHeadCode::Constant, index);
    }

    /// 压入 bool 常量。
    pub fn emit_const_bool(&mut self, value: bool) {
        let index = self.constants.len() as i32 + self.constants_base;
        self.constants.push(OdinConstant::Boolean(value));
        self.emit_imm1(OdinHeadCode::Constant, index);
    }

    /// 压入 `null`：无捕获闭包的环境槽需要显式空引用，不能用 `i32 0` 冒充。
    pub fn emit_const_null(&mut self) {
        let index = self.constants.len() as i32 + self.constants_base;
        self.constants.push(OdinConstant::Null);
        self.emit_imm1(OdinHeadCode::Constant, index);
    }

    /// 压入 f64 常量（位模式原样入池）。
    pub fn emit_const_f64(&mut self, bits: u64) {
        let index = self.constants.len() as i32 + self.constants_base;
        self.constants.push(OdinConstant::Float64(f64::from_bits(bits)));
        self.emit_imm1(OdinHeadCode::Constant, index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编码器只消费 LIR：标签不产字节，跳转位移相对函数体起点回填。
    #[test]
    fn encodes_labels_and_relative_jumps() {
        let instructions = vec![
            OdinLirInstruction::Label(0),
            OdinLirInstruction::Jump { opcode: OdinHeadCode::Jump, target: 1 },
            OdinLirInstruction::Plain(OdinHeadCode::Nop),
            OdinLirInstruction::Label(1),
        ];
        let code = encode_odin_function(&instructions).expect("LIR 必须可编码");
        assert_eq!(code.len(), 6, "标签不产生字节");
        assert_eq!(code[0], OdinHeadCode::Jump as u8);
        assert_eq!(i32::from_le_bytes(code[1..5].try_into().expect("operand1")), 6, "跳至 `Nop` 之后的标签");
    }

    /// 缺失标签的跳转是 LIR 结构违约，必须结构化失败。
    #[test]
    fn jump_to_missing_label_is_rejected() {
        let instructions = vec![OdinLirInstruction::Jump { opcode: OdinHeadCode::Jump, target: 9 }];
        let error = encode_odin_function(&instructions).expect_err("缺少标签必须失败");
        assert!(error.contains("缺少标签"), "{error}");
    }

    /// 合成标签与块标签互不占用编号空间，回填合同一致；未定义的合成标签结构化失败。
    #[test]
    fn synthetic_labels_encode_relative_jumps() {
        let instructions = vec![
            OdinLirInstruction::Label(0),
            OdinLirInstruction::JumpTo { opcode: OdinHeadCode::JumpIfFalse, label: 0 },
            OdinLirInstruction::Plain(OdinHeadCode::Nop),
            OdinLirInstruction::SyntheticLabel(0),
            OdinLirInstruction::Plain(OdinHeadCode::Nop),
        ];
        let code = encode_odin_function(&instructions).expect("合成标签 LIR 必须可编码");
        assert_eq!(code.len(), 7, "合成标签不产生字节");
        assert_eq!(code[0], OdinHeadCode::JumpIfFalse as u8);
        assert_eq!(i32::from_le_bytes(code[1..5].try_into().expect("operand1")), 6, "跳至合成标签处");
        let error = encode_odin_function(&[OdinLirInstruction::JumpTo { opcode: OdinHeadCode::Jump, label: 3 }]).expect_err("未定义合成标签必须失败");
        assert!(error.contains("缺少定义"), "{error}");
    }
}
