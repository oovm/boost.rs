//! 有界执行循环。

use crate::{error::WasmError, op::Instruction, value::WasmValue};

/// 一次执行的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StepReport {
    /// 已执行指令数。
    pub steps: u64,
}

/// WebAssembly 数值栈解释器（骨架）。
#[derive(Debug, Clone)]
pub struct WasmVm {
    locals: Vec<WasmValue>,
    stack: Vec<WasmValue>,
    stack_limit: usize,
}

impl WasmVm {
    /// 创建一个解释器：给定局部变量初值与栈上限。
    pub fn new(locals: Vec<WasmValue>, stack_limit: usize) -> Self {
        Self { locals, stack: Vec::new(), stack_limit }
    }

    /// 数值栈当前深度。
    pub fn stack_depth(&self) -> usize {
        self.stack.len()
    }

    /// 按预算执行并返回 `return` 之前栈顶的值。
    pub fn execute(&mut self, code: &[Instruction], step_budget: u64) -> Result<(WasmValue, StepReport), WasmError> {
        let mut report = StepReport::default();
        for instruction in code {
            if report.steps >= step_budget {
                return Err(WasmError::StepBudgetExceeded { budget: step_budget });
            }
            report.steps += 1;
            match instruction {
                Instruction::Nop => {}
                Instruction::I32Const(value) => self.push(WasmValue::I32(*value))?,
                Instruction::I64Const(value) => self.push(WasmValue::I64(*value))?,
                Instruction::LocalGet(index) => {
                    let value = self.local(*index)?;
                    self.push(value)?;
                }
                Instruction::LocalSet(index) => {
                    let value = self.pop("local.set")?;
                    self.write_local(*index, value)?;
                }
                Instruction::LocalTee(index) => {
                    let value = self.peek("local.tee")?;
                    self.write_local(*index, value)?;
                }
                Instruction::Drop => {
                    self.pop("drop")?;
                }
                Instruction::I32Add | Instruction::I32Sub | Instruction::I32Mul | Instruction::I32DivS => {
                    let right = self.pop_i32("i32")?;
                    let left = self.pop_i32("i32")?;
                    let value = match instruction {
                        Instruction::I32Add => left.wrapping_add(right),
                        Instruction::I32Sub => left.wrapping_sub(right),
                        Instruction::I32Mul => left.wrapping_mul(right),
                        Instruction::I32DivS => {
                            if right == 0 {
                                return Err(WasmError::DivideByZero);
                            }
                            left.wrapping_div(right)
                        }
                        _ => unreachable!("matched arithmetic arm"),
                    };
                    self.push(WasmValue::I32(value))?;
                }
                Instruction::I64Add => {
                    let right = self.pop_i64("i64")?;
                    let left = self.pop_i64("i64")?;
                    self.push(WasmValue::I64(left.wrapping_add(right)))?;
                }
                Instruction::I64ExtendI32S => {
                    let value = self.pop_i32("i32")?;
                    self.push(WasmValue::I64(i64::from(value)))?;
                }
                Instruction::Return => {
                    return Ok((self.pop("return")?, report));
                }
            }
        }
        Err(WasmError::MissingReturn)
    }

    fn local(&self, index: usize) -> Result<WasmValue, WasmError> {
        self.locals.get(index).copied().ok_or(WasmError::LocalOutOfRange { index, locals: self.locals.len() })
    }

    fn write_local(&mut self, index: usize, value: WasmValue) -> Result<(), WasmError> {
        let len = self.locals.len();
        let local = self.locals.get_mut(index).ok_or(WasmError::LocalOutOfRange { index, locals: len })?;
        *local = value;
        Ok(())
    }

    fn push(&mut self, value: WasmValue) -> Result<(), WasmError> {
        if self.stack.len() >= self.stack_limit {
            return Err(WasmError::StackOverflow { limit: self.stack_limit });
        }
        self.stack.push(value);
        Ok(())
    }

    fn pop(&mut self, _context: &'static str) -> Result<WasmValue, WasmError> {
        let available = self.stack.len();
        self.stack.pop().ok_or(WasmError::StackUnderflow { needed: 1, available })
    }

    fn peek(&self, _context: &'static str) -> Result<WasmValue, WasmError> {
        let available = self.stack.len();
        self.stack.last().copied().ok_or(WasmError::StackUnderflow { needed: 1, available })
    }

    fn pop_i32(&mut self, _context: &'static str) -> Result<i32, WasmError> {
        match self.pop("i32")? {
            WasmValue::I32(value) => Ok(value),
            other => Err(WasmError::TypeMismatch { expected: "i32", actual: other.type_name() }),
        }
    }

    fn pop_i64(&mut self, _context: &'static str) -> Result<i64, WasmError> {
        match self.pop("i64")? {
            WasmValue::I64(value) => Ok(value),
            other => Err(WasmError::TypeMismatch { expected: "i64", actual: other.type_name() }),
        }
    }
}
