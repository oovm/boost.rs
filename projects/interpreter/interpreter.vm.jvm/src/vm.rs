//! 有界执行循环。

use crate::{error::JvmError, frame::Frame, host::NativeRegistry, op::Instruction};

/// 一次执行的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StepReport {
    /// 已执行指令数。
    pub steps: u64,
    /// 是否因返回指令结束。
    pub returned: bool,
}

/// JVM 栈式解释器（骨架）。
#[derive(Debug, Default, Clone)]
pub struct Jvm {
    natives: NativeRegistry,
}

impl Jvm {
    /// 使用给定 native 注册表创建解释器。
    pub fn new(natives: NativeRegistry) -> Self {
        Self { natives }
    }

    /// 按给定步进预算执行一段已解码指令，返回 `ireturn` 的值与统计。
    pub fn execute(&self, code: &[Instruction], frame: &mut Frame, step_budget: u64) -> Result<(i32, StepReport), JvmError> {
        let mut report = StepReport::default();
        for instruction in code {
            if report.steps >= step_budget {
                return Err(JvmError::StepBudgetExceeded { budget: step_budget });
            }
            report.steps += 1;
            match instruction {
                Instruction::Nop => {}
                Instruction::IConst(value) => frame.push(crate::Slot::Int(*value))?,
                Instruction::LConst(value) => frame.push(crate::Slot::LongHigh(*value))?,
                Instruction::ILoad(index) => {
                    let slot = frame.local(*index)?;
                    frame.push(slot)?;
                }
                Instruction::LLoad(index) => {
                    let high = frame.local(*index)?;
                    if let crate::Slot::LongHigh(value) = high {
                        frame.push(crate::Slot::LongHigh(value))?;
                    }
                    else {
                        return Err(JvmError::TypeMismatch { expected: "long", actual: "int" });
                    }
                }
                Instruction::IStore(index) => {
                    let value = frame.pop_int()?;
                    frame.set_local_int(*index, value)?;
                }
                Instruction::LStore(index) => {
                    let value = frame.pop_long()?;
                    frame.set_local_long(*index, value)?;
                }
                Instruction::Pop => {
                    frame.pop_int()?;
                }
                Instruction::IAdd | Instruction::ISub | Instruction::IMul | Instruction::IDiv => {
                    let (left, right) = frame.pop_int_pair()?;
                    let value = match instruction {
                        Instruction::IAdd => left.wrapping_add(right),
                        Instruction::ISub => left.wrapping_sub(right),
                        Instruction::IMul => left.wrapping_mul(right),
                        Instruction::IDiv => {
                            if right == 0 {
                                return Err(JvmError::DivideByZero);
                            }
                            left.wrapping_div(right)
                        }
                        _ => unreachable!("matched arithmetic arm"),
                    };
                    frame.push(crate::Slot::Int(value))?;
                }
                Instruction::LAdd => {
                    let right = frame.pop_long()?;
                    let left = frame.pop_long()?;
                    frame.push(crate::Slot::LongHigh(left.wrapping_add(right)))?;
                }
                Instruction::I2L => {
                    let value = frame.pop_int()?;
                    frame.push(crate::Slot::LongHigh(i64::from(value)))?;
                }
                Instruction::InvokeStaticNative { name } => {
                    let binding = self.natives.get(name)?;
                    let argument = frame.pop_int()?;
                    match binding(&[argument]) {
                        Some(value) => frame.push(crate::Slot::Int(value))?,
                        None => {}
                    }
                }
                Instruction::IReturn => {
                    report.returned = true;
                    return Ok((frame.pop_int()?, report));
                }
                Instruction::LReturn | Instruction::Return => {
                    report.returned = true;
                    return Ok((0, report));
                }
            }
        }
        Err(JvmError::MissingReturn)
    }
}
