//! 分类与回归评估指标。
//!
//! 输入是普通切片，不依赖张量或训练框架；调用方把模型输出转成预测标签/数值后即可使用。
//! 空输入与长度不一致都返回可判别错误，不做静默截断。

#![deny(missing_docs)]

/// 指标计算错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricError {
    /// 输入为空，指标无定义。
    Empty,
    /// 预测与标签长度不一致。
    LengthMismatch {
        /// 预测数量。
        predictions: usize,
        /// 标签数量。
        targets: usize,
    },
}

impl std::fmt::Display for MetricError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetricError::Empty => write!(f, "metric input is empty"),
            MetricError::LengthMismatch { predictions, targets } => {
                write!(f, "length mismatch: {predictions} predictions, {targets} targets")
            }
        }
    }
}

impl std::error::Error for MetricError {}

fn check_lengths(predictions: usize, targets: usize) -> Result<usize, MetricError> {
    if predictions == 0 || targets == 0 {
        return Err(MetricError::Empty);
    }
    if predictions != targets {
        return Err(MetricError::LengthMismatch { predictions, targets });
    }
    Ok(predictions)
}

/// 多分类准确率。
pub fn accuracy(predictions: &[u32], targets: &[u32]) -> Result<f32, MetricError> {
    let count = check_lengths(predictions.len(), targets.len())?;
    let correct = predictions.iter().zip(targets).filter(|(prediction, target)| prediction == target).count();
    Ok(correct as f32 / count as f32)
}

/// 二分类精确率、召回率与 F1。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClassificationMetrics {
    /// 精确率 `tp / (tp + fp)`；没有正预测时为 0。
    pub precision: f32,
    /// 召回率 `tp / (tp + fn)`；没有正标签时为 0。
    pub recall: f32,
    /// 调和平均；精确率与召回率同时为 0 时为 0。
    pub f1: f32,
}

/// 按 `positive` 标签计算二分类指标。
pub fn precision_recall_f1(predictions: &[u32], targets: &[u32], positive: u32) -> Result<ClassificationMetrics, MetricError> {
    check_lengths(predictions.len(), targets.len())?;
    let mut true_positive = 0usize;
    let mut false_positive = 0usize;
    let mut false_negative = 0usize;
    for (prediction, target) in predictions.iter().zip(targets) {
        match (*prediction == positive, *target == positive) {
            (true, true) => true_positive += 1,
            (true, false) => false_positive += 1,
            (false, true) => false_negative += 1,
            (false, false) => {}
        }
    }
    let precision = ratio(true_positive, true_positive + false_positive);
    let recall = ratio(true_positive, true_positive + false_negative);
    let f1 = if precision + recall == 0.0 { 0.0 } else { 2.0 * precision * recall / (precision + recall) };
    Ok(ClassificationMetrics { precision, recall, f1 })
}

fn ratio(numerator: usize, denominator: usize) -> f32 {
    if denominator == 0 { 0.0 } else { numerator as f32 / denominator as f32 }
}

/// 均方误差。
pub fn mse(predictions: &[f32], targets: &[f32]) -> Result<f32, MetricError> {
    let count = check_lengths(predictions.len(), targets.len())?;
    let total: f32 = predictions.iter().zip(targets).map(|(prediction, target)| (prediction - target).powi(2)).sum();
    Ok(total / count as f32)
}

/// 平均绝对误差。
pub fn mae(predictions: &[f32], targets: &[f32]) -> Result<f32, MetricError> {
    let count = check_lengths(predictions.len(), targets.len())?;
    let total: f32 = predictions.iter().zip(targets).map(|(prediction, target)| (prediction - target).abs()).sum();
    Ok(total / count as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accuracy_counts_exact_matches() {
        assert_eq!(accuracy(&[1, 2, 3, 4], &[1, 0, 3, 3]).unwrap(), 0.5);
        assert_eq!(accuracy(&[1], &[1]).unwrap(), 1.0);
    }

    #[test]
    fn precision_recall_f1_hand_computed() {
        let metrics = precision_recall_f1(&[1, 1, 0, 0, 1], &[1, 0, 1, 0, 1], 1).unwrap();
        assert_eq!(metrics.precision, 2.0 / 3.0);
        assert_eq!(metrics.recall, 2.0 / 3.0);
        assert!((metrics.f1 - 2.0 / 3.0).abs() < 1e-6);

        let none = precision_recall_f1(&[0, 0], &[0, 0], 1).unwrap();
        assert_eq!(none, ClassificationMetrics { precision: 0.0, recall: 0.0, f1: 0.0 });
    }

    #[test]
    fn regression_errors() {
        assert_eq!(mse(&[1.0, 2.0, 3.0], &[1.0, 2.0, 5.0]).unwrap(), 4.0 / 3.0);
        assert_eq!(mae(&[1.0, 2.0, 3.0], &[1.0, 2.0, 5.0]).unwrap(), 2.0 / 3.0);
    }

    #[test]
    fn rejects_empty_and_mismatched_inputs() {
        assert_eq!(accuracy(&[], &[]), Err(MetricError::Empty));
        assert_eq!(accuracy(&[1], &[1, 2]), Err(MetricError::LengthMismatch { predictions: 1, targets: 2 }));
        assert_eq!(mse(&[], &[]), Err(MetricError::Empty));
        assert_eq!(mae(&[1.0, 2.0], &[1.0]), Err(MetricError::LengthMismatch { predictions: 2, targets: 1 }));
        assert_eq!(precision_recall_f1(&[], &[], 1), Err(MetricError::Empty));
    }
}
