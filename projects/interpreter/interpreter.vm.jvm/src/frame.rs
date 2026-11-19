//! JVM 局部变量槽与操作数栈。
//!
//! JVM 的 `long` 与 `double` 各占两个连续槽；本模块把槽宽规则集中在一处，
//! 避免调用方各自猜测槽数。

/// 一个局部变量槽的内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Slot {
    /// 空槽（`long`/`double` 的第二个槽、尚未写入的槽）。
    #[default]
    Empty,
    /// 单槽整数。
    Int(i32),
    /// 双槽长整数的第一个槽。
    LongHigh(i64),
    /// 双槽长整数的第二个槽。
    LongLow,
}

impl Slot {
    /// 该槽内容占用的槽数。
    pub fn width(self) -> usize {
        match self {
            Self::Empty | Self::Int(_) | Self::LongLow => 1,
            Self::LongHigh(_) => 2,
        }
    }

    /// 是否为空槽。
    pub fn is_empty(self) -> bool {
        matches!(self, Self::Empty)
    }
}

/// 栈式执行帧：局部变量槽与操作数栈。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    locals: Vec<Slot>,
    stack: Vec<Slot>,
    stack_limit: usize,
}

impl Frame {
    /// 创建一个帧。
    ///
    /// `local_slots` 是局部变量区的槽数（不是变量个数），`stack_limit` 是操作数栈的槽上限。
    pub fn new(local_slots: usize, stack_limit: usize) -> Self {
        Self { locals: vec![Slot::Empty; local_slots], stack: Vec::new(), stack_limit }
    }

    /// 局部变量区槽数。
    pub fn local_slot_count(&self) -> usize {
        self.locals.len()
    }

    /// 操作数栈当前槽数。
    pub fn stack_depth(&self) -> usize {
        self.stack.len()
    }

    /// 读取局部变量槽。
    pub fn local(&self, index: usize) -> Result<Slot, crate::JvmError> {
        self.locals.get(index).copied().ok_or(crate::JvmError::LocalOutOfRange { index, slots: self.locals.len() })
    }

    /// 写入单槽整数到局部变量区。
    pub fn set_local_int(&mut self, index: usize, value: i32) -> Result<(), crate::JvmError> {
        self.write_local(index, Slot::Int(value))
    }

    /// 写入双槽长整数到局部变量区，占用 `index` 与 `index + 1`。
    pub fn set_local_long(&mut self, index: usize, value: i64) -> Result<(), crate::JvmError> {
        self.write_local(index, Slot::LongHigh(value))?;
        self.write_local(index + 1, Slot::LongLow)
    }

    fn write_local(&mut self, index: usize, slot: Slot) -> Result<(), crate::JvmError> {
        let len = self.locals.len();
        let local = self.locals.get_mut(index).ok_or(crate::JvmError::LocalOutOfRange { index, slots: len })?;
        *local = slot;
        Ok(())
    }

    /// 压入一个槽。
    pub fn push(&mut self, slot: Slot) -> Result<(), crate::JvmError> {
        if self.stack.len() + slot.width() > self.stack_limit {
            return Err(crate::JvmError::StackOverflow { limit: self.stack_limit });
        }
        self.stack.push(slot);
        if slot.width() == 2 {
            self.stack.push(Slot::LongLow);
        }
        Ok(())
    }

    /// 弹出单槽整数。
    pub fn pop_int(&mut self) -> Result<i32, crate::JvmError> {
        let slot = self.pop_slot("int")?;
        match slot {
            Slot::Int(value) => Ok(value),
            other => Err(crate::JvmError::TypeMismatch { expected: "int", actual: slot_name(other) }),
        }
    }

    /// 弹出双槽长整数。
    pub fn pop_long(&mut self) -> Result<i64, crate::JvmError> {
        let low = self.pop_slot("long")?;
        if low != Slot::LongLow {
            return Err(crate::JvmError::TypeMismatch { expected: "long", actual: slot_name(low) });
        }
        let high = self.pop_slot("long")?;
        match high {
            Slot::LongHigh(value) => Ok(value),
            other => Err(crate::JvmError::TypeMismatch { expected: "long", actual: slot_name(other) }),
        }
    }

    fn pop_slot(&mut self, expected: &'static str) -> Result<Slot, crate::JvmError> {
        let available = self.stack.len();
        if available == 0 {
            return Err(crate::JvmError::StackUnderflow { needed: 1, available });
        }
        self.stack
            .pop()
            .ok_or(crate::JvmError::StackUnderflow { needed: 1, available })
            .map_err(|_| crate::JvmError::TypeMismatch { expected, actual: "empty" })
    }

    /// 弹出两个单槽整数，先弹出的为右操作数。
    pub fn pop_int_pair(&mut self) -> Result<(i32, i32), crate::JvmError> {
        let right = self.pop_int()?;
        let left = self.pop_int()?;
        Ok((left, right))
    }
}

fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::Empty => "empty",
        Slot::Int(_) => "int",
        Slot::LongHigh(_) | Slot::LongLow => "long",
    }
}
