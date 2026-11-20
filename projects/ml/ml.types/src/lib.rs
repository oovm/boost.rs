//! `ml.*` 的共享词汇：一条带整数标签的稠密样本。
//!
//! [`Sample`] 不依赖张量、CSV/IDX 解析或任何训练框架：`ml-data` 负责把它组织成数据集与
//! 批次，`ml-metrics` 与下游消费者可以直接拿它构造评估数据。本 crate 只做值类型，
//! 不做计算，也不引入 `dl.*` 依赖——这样评估侧不需要把张量栈拖进依赖图。

#![deny(missing_docs)]

/// 一条带整数标签的稠密样本。
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// 连续特征。
    pub features: Vec<f32>,
    /// 类别标签。
    pub label: u32,
}

impl Sample {
    /// 构造样本。
    pub fn new(features: impl Into<Vec<f32>>, label: u32) -> Self {
        Self { features: features.into(), label }
    }

    /// 特征宽度。
    pub fn width(&self) -> usize {
        self.features.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_plain_data() {
        let sample = Sample::new(vec![1.0, 2.0], 3);
        assert_eq!(sample.width(), 2);
        assert_eq!(sample, Sample::new(vec![1.0, 2.0], 3));
        assert_ne!(sample, Sample::new(vec![1.0, 2.0], 4));
    }
}
