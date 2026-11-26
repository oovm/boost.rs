#![warn(missing_docs)]
#![forbid(unsafe_code)]

//! 残化特化框架（Futamura 第一投影）的分层合同。
//!
//! # 分层
//!
//! - 本 crate 只提供**语言无关**的特化框架约束：已知／未知传播、控制流与
//!   副作用约束。它不定义任何具体语言的操作枚举、栈指令或值语义。
//! - 具体语言的解释器特化（把解释器分派残化为操作流）连同其 AST/HIR/MIR、
//!   guest 语义与残差形状归该语言前端所有。
//! - 目标产物生成归语言 × 目标适配器与目标编码器，不在本 crate 内。
//! - 未知调用必须作为残余操作保留，不能在特化期执行或丢弃。
//!
//! # 历史裁定
//!
//! 旧 `ResidualSink` 公共 trait 规定了 Lua 真值、整数截断除法、空返回补
//! `nil` 与 Lua 风格表操作，实际是一套小型栈语言，已按架构门删除。框架接口
//! 随第一个真实消费者（TypeScript × OVM 投影线）以语言无关形状重新定义；
//! 在此之前禁止往本 crate 回填任何按语言语义命名的操作。

/// 阶段值：一个计算结果在特化期是否已结算。
///
/// `Known(v)` 表示该值可在特化期消费（折叠进产物、消去分支）；`Unknown` 表示
/// 该值依赖 guest 输入或残余效应，必须以残差形式进入产物，禁止在特化期猜测。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage<T> {
    /// 特化期已结算的值。
    Known(T),
    /// 特化期不可判定，必须残差。
    Unknown,
}

impl<T> Stage<T> {
    /// 是否为已知阶段。
    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known(_))
    }

    /// 是否为未知阶段。
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// 借用已知值；未知返回 `None`。
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown => None,
        }
    }

    /// 已知值映射；未知原样传播（Known/Unknown 传播合同）。
    pub fn map<U>(self, transform: impl FnOnce(T) -> U) -> Stage<U> {
        match self {
            Self::Known(value) => Stage::Known(transform(value)),
            Self::Unknown => Stage::Unknown,
        }
    }

    /// 已知值绑定的下一阶段；未知原样传播。
    pub fn and_then<U>(self, transform: impl FnOnce(T) -> Stage<U>) -> Stage<U> {
        match self {
            Self::Known(value) => transform(value),
            Self::Unknown => Stage::Unknown,
        }
    }

    /// 双输入结算：两侧都已知才已知；任一未知则结果未知。
    pub fn zip<U: Clone>(left: &Stage<T>, right: &Stage<U>) -> Stage<(T, U)>
    where
        T: Clone,
    {
        match (left, right) {
            (Stage::Known(a), Stage::Known(b)) => Stage::Known((a.clone(), b.clone())),
            _ => Stage::Unknown,
        }
    }
}

/// 分支在残差产物中必须呈现的形状（控制流约束）。
///
/// 该形状**只由条件的阶段决定**：语言侧不得为了让产物更短而丢弃未知条件的
/// 任何一侧，也不得保留已知条件被消去的一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchResidual {
    /// 条件已知为真：只残差 then 分支，else 分支必须消去。
    ThenOnly,
    /// 条件已知为假：只残差 else 分支，then 分支必须消去。
    ElseOnly,
    /// 条件未知：两个分支都必须完整残差。
    Both,
}

/// 由条件阶段导出分支残差形状。
pub fn branch_residual_shape(condition: &Stage<bool>) -> BranchResidual {
    match condition {
        Stage::Known(true) => BranchResidual::ThenOnly,
        Stage::Known(false) => BranchResidual::ElseOnly,
        Stage::Unknown => BranchResidual::Both,
    }
}

/// 特化框架违约：约束被违反时的结构化错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecializeError {
    /// 未知条件的分支被丢弃：`dropped` 指出缺残差的一侧。
    UnknownConditionBranchDropped {
        /// 被丢弃的一侧（`"then"` / `"else"`）。
        dropped: &'static str,
    },
    /// 已知条件的消去侧仍被残差：产物里留下了本应消去的分派。
    KnownConditionBranchRetained {
        /// 多余残差的一侧（`"then"` / `"else"`）。
        retained: &'static str,
    },
    /// 副作用残差顺序违约：效应必须按源码序残差，不得重排或省略。
    EffectOrderViolation {
        /// 上一条已记录的源码序。
        last_order: u64,
        /// 本次尝试写入的源码序。
        attempted_order: u64,
    },
}

impl std::fmt::Display for SpecializeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownConditionBranchDropped { dropped } => {
                write!(formatter, "控制流约束违约：未知条件的 {dropped} 分支必须残差，不允许丢弃")
            }
            Self::KnownConditionBranchRetained { retained } => {
                write!(formatter, "控制流约束违约：已知条件的 {retained} 分支必须消去，产物中仍存在其残差")
            }
            Self::EffectOrderViolation { last_order, attempted_order } => {
                write!(formatter, "副作用约束违约：效应序 {attempted_order} 未严格大于上一条 {last_order}")
            }
        }
    }
}

impl std::error::Error for SpecializeError {}

/// 按控制流约束校验分支残差覆盖。
///
/// - [`BranchResidual::Both`]：两侧都必须已残差；
/// - [`BranchResidual::ThenOnly`] / [`BranchResidual::ElseOnly`]：被消去的一侧
///   必须没有残差。
///
/// 违约返回 [`SpecializeError`]，供消费者失败闭合（不是 panic、不是静默通过）。
pub fn check_branch_residual(shape: BranchResidual, then_residualized: bool, else_residualized: bool) -> Result<(), SpecializeError> {
    match shape {
        BranchResidual::Both => {
            if !then_residualized {
                return Err(SpecializeError::UnknownConditionBranchDropped { dropped: "then" });
            }
            if !else_residualized {
                return Err(SpecializeError::UnknownConditionBranchDropped { dropped: "else" });
            }
            Ok(())
        }
        BranchResidual::ThenOnly => {
            if else_residualized {
                return Err(SpecializeError::KnownConditionBranchRetained { retained: "else" });
            }
            Ok(())
        }
        BranchResidual::ElseOnly => {
            if then_residualized {
                return Err(SpecializeError::KnownConditionBranchRetained { retained: "then" });
            }
            Ok(())
        }
    }
}

/// 一条已残差的副作用效应（身份由语言侧给出，框架不解释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideEffectEntry {
    /// 源码序：效应必须按该序严格递增残差。
    pub order: u64,
    /// 语言侧效应身份（例如某语言的日志、外部调用符号）；框架只记录不解释。
    pub identity: String,
}

/// 副作用账本：约束副作用不得在特化期执行、必须按源码序残差。
///
/// 消费者在残差每条效应时调用 [`SideEffectLedger::record`]；任何乱序或
/// 重复写入都会得到 [`SpecializeError::EffectOrderViolation`]，从而在结构上
/// 阻止「特化期偷跑效应」或「重排效应」。
#[derive(Debug, Clone, Default)]
pub struct SideEffectLedger {
    entries: Vec<SideEffectEntry>,
}

impl SideEffectLedger {
    /// 新建空账本。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一条按源码序残差的效应。
    pub fn record(&mut self, order: u64, identity: impl Into<String>) -> Result<(), SpecializeError> {
        let last_order = self.entries.last().map(|entry| entry.order);
        if let Some(last) = last_order {
            if order <= last {
                return Err(SpecializeError::EffectOrderViolation { last_order: last, attempted_order: order });
            }
        }
        self.entries.push(SideEffectEntry { order, identity: identity.into() });
        Ok(())
    }

    /// 已记录的效应序列（源码序）。
    pub fn entries(&self) -> &[SideEffectEntry] {
        &self.entries
    }

    /// 是否没有任何已残差效应（用于纯性判定）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_propagates_through_zip() {
        let known = Stage::Known(1i32);
        let unknown = Stage::<i32>::Unknown;
        assert_eq!(Stage::zip(&known, &known), Stage::Known((1, 1)));
        assert!(Stage::zip(&known, &unknown).is_unknown());
    }

    #[test]
    fn branch_shape_enforces_both_and_elimination() {
        assert_eq!(branch_residual_shape(&Stage::Unknown), BranchResidual::Both);
        assert!(check_branch_residual(BranchResidual::Both, true, false) == Err(SpecializeError::UnknownConditionBranchDropped { dropped: "else" }));
        assert!(check_branch_residual(BranchResidual::ThenOnly, true, true) == Err(SpecializeError::KnownConditionBranchRetained { retained: "else" }));
        assert!(check_branch_residual(BranchResidual::ThenOnly, true, false).is_ok());
    }

    #[test]
    fn effect_ledger_rejects_reordering() {
        let mut ledger = SideEffectLedger::new();
        assert!(ledger.record(3, "a").is_ok());
        assert!(ledger.record(2, "b").is_err());
        assert_eq!(ledger.entries().len(), 1);
    }
}
