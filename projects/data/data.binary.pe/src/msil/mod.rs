//! `MSIL` 文本模型：`model.rs` 定义结构、`writer.rs` 写回文本、`parser.rs` 提取方法块与签名。
//! 不承载命令行输出格式，也不含 `CLR` 构建计划或后端选择逻辑。
#![allow(missing_docs)]

mod lexer;
mod method_body;
mod model;
/// ECMA-335 标准 CIL 操作码表与操作数解码。
pub mod op_codes;
mod parser;
mod text;

pub use method_body::{MethodBodyEncoder, MethodBodyError};
pub use model::{
    MsilAssembly, MsilField, MsilInstruction, MsilInstructionOperand, MsilMethodBody, MsilMethodRef, MsilMethodSignature, MsilModule,
    MsilOpcode, MsilType, MsilTypeDef,
};
pub use op_codes::{IlOperand, OP_CODES, OpCodeInfo, OperandType, decode_op_code, get_operand_type, lookup_opcode, read_operand};
pub use parser::{MsilParser, MsilTextMethod};
pub use text::MsilTextWriter;
