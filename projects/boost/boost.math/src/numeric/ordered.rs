//! 需要 `Eq` 的 `f64` 键语义。

use core::cmp::Ordering;

/// 可用于 `Eq`/顺序比较的 `f64` 包装。
///
/// 语言层常量、模式匹配与需要稳定身份的中间表示要求字面量满足 `Eq`，
/// 而 `f64` 的 `NaN != NaN` 会破坏该前提。本类型的语义在以下三点冻结：
///
/// - 所有 `NaN`（含不同 payload）视为同一值，`NaN == NaN`；
/// - `+0.0 == -0.0`，与 IEEE 比较一致，不按位区分零的符号；
/// - `NaN` 在顺序中大于一切非 `NaN` 值，其余沿用 `partial_cmp`。
///
/// 这是「可比较的数值身份」，不是按位重释义：`to_bits()` 仍由调用方显式使用，
/// 取值与位模式互不替代。`Ord` 与 `Eq` 在此定义下一致，因此可以进入
/// `BTreeMap`/`HashMap` 之外的键位置而不破坏排序前提。
#[derive(Clone, Copy, Debug, Default)]
pub struct OrderedF64(f64);

impl OrderedF64 {
    /// 包装一个 `f64`；不改变其值，包括符号与 `NaN` payload。
    #[must_use]
    pub const fn new(value: f64) -> Self {
        Self(value)
    }

    /// 取出原始 `f64`。
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl From<f64> for OrderedF64 {
    fn from(value: f64) -> Self {
        Self(value)
    }
}

impl From<OrderedF64> for f64 {
    fn from(value: OrderedF64) -> Self {
        value.0
    }
}

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        if self.0.is_nan() { other.0.is_nan() } else { self.0 == other.0 }
    }
}

impl Eq for OrderedF64 {}

impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.0.is_nan(), other.0.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal),
        }
    }
}
