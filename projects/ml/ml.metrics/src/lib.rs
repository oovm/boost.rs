//! 分类与回归评估指标。
//!
//! 输入是普通切片，不依赖张量或训练框架；调用方把模型输出转成预测标签/数值后即可使用。
//! 样本级入口（[`accuracy_on_samples`] 等）使用 `ml-types` 的 [`Sample`]，避免各消费者
//! 自己重复取 `label`。空输入与长度不一致都返回可判别错误，不做静默截断。

#![deny(missing_docs)]

use ml_types::Sample;

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

/// 对样本集合计算准确率：`predictions[i]` 对应 `samples[i].label`。
pub fn accuracy_on_samples(predictions: &[u32], samples: &[Sample]) -> Result<f32, MetricError> {
    let labels: Vec<u32> = samples.iter().map(|sample| sample.label).collect();
    accuracy(predictions, &labels)
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
    let f1 = harmonic(precision, recall);
    Ok(ClassificationMetrics { precision, recall, f1 })
}

fn ratio(numerator: usize, denominator: usize) -> f32 {
    if denominator == 0 { 0.0 } else { numerator as f32 / denominator as f32 }
}

/// 单个类别的指标。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerClassMetrics {
    /// 类别标签。
    pub class: u32,
    /// 该类的真实样本数。
    pub support: usize,
    /// 该类的精确率、召回率与 F1。
    pub metrics: ClassificationMetrics,
}

/// 多分类指标：逐类、宏平均与微平均。
#[derive(Debug, Clone, PartialEq)]
pub struct MultiClassMetrics {
    /// 按 `classes` 顺序给出的逐类指标。
    pub per_class: Vec<PerClassMetrics>,
    /// 宏平均精确率（对给定类别等权）。
    pub macro_precision: f32,
    /// 宏平均召回率。
    pub macro_recall: f32,
    /// 宏平均 F1。
    pub macro_f1: f32,
    /// 微平均（先汇总 tp/fp/fn 再计算）。
    pub micro: ClassificationMetrics,
    /// 准确率。
    pub accuracy: f32,
}

/// 按给定类别集合计算多分类指标。
///
/// 宏平均对 `classes` 等权，包括数据中未出现的类别；微平均先把所有类别的
/// tp/fp/fn 汇总再计算。`classes` 为空返回 [`MetricError::Empty`]。
pub fn multiclass_metrics(predictions: &[u32], targets: &[u32], classes: &[u32]) -> Result<MultiClassMetrics, MetricError> {
    check_lengths(predictions.len(), targets.len())?;
    if classes.is_empty() {
        return Err(MetricError::Empty);
    }

    let mut per_class = Vec::with_capacity(classes.len());
    let (mut total_true_positive, mut total_false_positive, mut total_false_negative) = (0usize, 0usize, 0usize);
    for class in classes {
        let mut true_positive = 0usize;
        let mut false_positive = 0usize;
        let mut false_negative = 0usize;
        for (prediction, target) in predictions.iter().zip(targets) {
            match (*prediction == *class, *target == *class) {
                (true, true) => true_positive += 1,
                (true, false) => false_positive += 1,
                (false, true) => false_negative += 1,
                (false, false) => {}
            }
        }
        total_true_positive += true_positive;
        total_false_positive += false_positive;
        total_false_negative += false_negative;

        let precision = ratio(true_positive, true_positive + false_positive);
        let recall = ratio(true_positive, true_positive + false_negative);
        per_class.push(PerClassMetrics {
            class: *class,
            support: true_positive + false_negative,
            metrics: ClassificationMetrics { precision, recall, f1: harmonic(precision, recall) },
        });
    }

    let count = classes.len() as f32;
    let micro_precision = ratio(total_true_positive, total_true_positive + total_false_positive);
    let micro_recall = ratio(total_true_positive, total_true_positive + total_false_negative);
    Ok(MultiClassMetrics {
        macro_precision: per_class.iter().map(|entry| entry.metrics.precision).sum::<f32>() / count,
        macro_recall: per_class.iter().map(|entry| entry.metrics.recall).sum::<f32>() / count,
        macro_f1: per_class.iter().map(|entry| entry.metrics.f1).sum::<f32>() / count,
        micro: ClassificationMetrics { precision: micro_precision, recall: micro_recall, f1: harmonic(micro_precision, micro_recall) },
        accuracy: accuracy(predictions, targets)?,
        per_class,
    })
}

fn harmonic(precision: f32, recall: f32) -> f32 {
    if precision + recall == 0.0 { 0.0 } else { 2.0 * precision * recall / (precision + recall) }
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
    fn multiclass_metrics_match_hand_computation() {
        let metrics = multiclass_metrics(&[0, 1, 2, 2], &[0, 2, 2, 1], &[0, 1, 2]).unwrap();
        assert_eq!(metrics.accuracy, 0.5);
        assert_eq!(metrics.per_class[0].metrics, ClassificationMetrics { precision: 1.0, recall: 1.0, f1: 1.0 });
        assert_eq!(metrics.per_class[1].support, 1);
        assert_eq!(metrics.per_class[1].metrics, ClassificationMetrics { precision: 0.0, recall: 0.0, f1: 0.0 });
        assert_eq!(metrics.per_class[2].metrics, ClassificationMetrics { precision: 0.5, recall: 0.5, f1: 0.5 });
        assert_eq!(metrics.macro_precision, 0.5);
        assert_eq!(metrics.macro_recall, 0.5);
        assert_eq!(metrics.macro_f1, 0.5);
        assert!((metrics.micro.precision - 0.5).abs() < 1e-6);
        assert!((metrics.micro.recall - 0.5).abs() < 1e-6);
        assert!((metrics.micro.f1 - 0.5).abs() < 1e-6);

        assert_eq!(multiclass_metrics(&[0], &[0], &[]), Err(MetricError::Empty));
        assert_eq!(multiclass_metrics(&[], &[], &[0]), Err(MetricError::Empty));
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
