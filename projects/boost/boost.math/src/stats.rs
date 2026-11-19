//! 采样统计的纯数学内核：百分位位次与最低样本策略。
//!
//! 这里只计算「排序后取第几个样本」，不拥有样本的存储、单位或报告格式：
//! `f64` 毫秒样本与 `Duration` 样本共用同一函数，避免各 harness 的私有实现
//! 对同一批样本给出不同结果。

/// 最近秩（nearest-rank）百分位位次。
///
/// `numerator / denominator` 以整数表达百分位（例如 `95 / 100`），避免浮点
/// 边界。返回 0 起下标：该下标的样本是满足「至少 `numerator / denominator`
/// 比例样本不超过它」的最小值。`len = 32, p = 95/100` 时位次为 31（下标 30）。
///
/// 空样本或非法分母返回 `None`；`numerator = 0` 返回最小样本。
#[must_use]
pub fn nearest_rank_index(len: usize, numerator: u32, denominator: u32) -> Option<usize> {
    if len == 0 || denominator == 0 {
        return None;
    }
    if numerator == 0 {
        return Some(0);
    }
    let rank = (len as u128 * numerator as u128).div_ceil(denominator as u128);
    let index = rank.max(1) - 1;
    Some(usize::try_from(index).unwrap_or(usize::MAX).min(len - 1))
}

/// 百分位策略：把「哪个百分位」与「最低有效样本数」一起显式声明。
///
/// 不同 harness 可以有不同的最低样本门槛，但策略必须由调用方以此类型写明，
/// 不能藏在各自的私有函数里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PercentilePolicy {
    /// 百分位分子，例如 `95`。
    pub numerator: u32,
    /// 百分位分母，例如 `100`。
    pub denominator: u32,
    /// 低于该样本数时不报该百分位。
    pub min_samples: usize,
}

impl PercentilePolicy {
    /// 声明一个百分位策略。
    #[must_use]
    pub const fn new(numerator: u32, denominator: u32, min_samples: usize) -> Self {
        Self { numerator, denominator, min_samples }
    }

    /// 按策略取位次；样本数低于门槛时返回 `None`。
    #[must_use]
    pub fn index(&self, len: usize) -> Option<usize> {
        if len < self.min_samples { None } else { nearest_rank_index(len, self.numerator, self.denominator) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_matches_the_standard_definition() {
        // 1..=32 的 p95 位次是第 31 个样本（下标 30），不是四舍五入的 30。
        assert_eq!(nearest_rank_index(32, 95, 100), Some(30));
        assert_eq!(nearest_rank_index(20, 95, 100), Some(18));
        assert_eq!(nearest_rank_index(100, 99, 100), Some(98));
        assert_eq!(nearest_rank_index(1, 95, 100), Some(0));
        assert_eq!(nearest_rank_index(4, 50, 100), Some(1));
    }

    #[test]
    fn empty_samples_and_zero_denominator_have_no_rank() {
        assert_eq!(nearest_rank_index(0, 95, 100), None);
        assert_eq!(nearest_rank_index(10, 95, 0), None);
    }

    #[test]
    fn policy_makes_the_minimum_sample_count_explicit() {
        let policy = PercentilePolicy::new(95, 100, 20);
        assert_eq!(policy.index(19), None);
        assert_eq!(policy.index(20), Some(18));
        let permissive = PercentilePolicy::new(95, 100, 1);
        assert_eq!(permissive.index(1), Some(0));
        assert_eq!(permissive.index(0), None);
    }
}
