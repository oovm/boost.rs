//! 端到端 ML 验收门禁：数据集 → 图训练 → 指标 → 门禁 → 报告。
//!
//! 本 crate 只做编排与判定，不实现任何训练或指标语义：训练固定走 `dl-train` 的
//! [`GraphMlp`]（其执行已经过 `dl-plan` 放置计划），指标固定走 `ml-metrics`。
//! 门禁是可枚举的 findings，[`render_markdown`] 产出可读报告；不足一批的尾行按
//! `GraphMlp` 的固定批形状丢弃，与 `dl-plan` 边界约定一致。

#![deny(missing_docs)]

use boost_random::{ChaCha12, Random};
use dl_tensor::Tensor;
use dl_train::{GraphAdam, GraphMlp};
use ml_data::{DataError, Dataset};
use ml_metrics::MetricError;

/// 验收编排错误。
#[derive(Debug)]
pub enum HarnessError {
    /// 参数组合不合法（轮数为零等）。
    Config(String),
    /// 训练图构造、步进或前向失败。
    Train(dl_types::DlError),
    /// 指标计算失败。
    Metric(MetricError),
    /// 数据集或批次切分失败。
    Data(DataError),
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HarnessError::Config(detail) => write!(f, "invalid harness configuration: {detail}"),
            HarnessError::Train(error) => write!(f, "training failed: {error}"),
            HarnessError::Metric(error) => write!(f, "metric failed: {error}"),
            HarnessError::Data(error) => write!(f, "data failed: {error}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<dl_types::DlError> for HarnessError {
    fn from(error: dl_types::DlError) -> Self {
        HarnessError::Train(error)
    }
}

impl From<MetricError> for HarnessError {
    fn from(error: MetricError) -> Self {
        HarnessError::Metric(error)
    }
}

impl From<DataError> for HarnessError {
    fn from(error: DataError) -> Self {
        HarnessError::Data(error)
    }
}

/// 通过条件：末轮准确率不低于 `min_accuracy`，且末轮损失不超过首轮损失乘
/// `max_loss_ratio`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gate {
    /// 最低末轮准确率。
    pub min_accuracy: f32,
    /// 末轮/首轮损失的最大比例。
    pub max_loss_ratio: f32,
}

/// 验收发现。
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// 末轮损失不低于首轮损失：训练没有收敛迹象。
    LossNotDecreasing {
        /// 首轮平均损失。
        first: f32,
        /// 末轮平均损失。
        last: f32,
    },
    /// 末轮损失超过门禁允许的比例。
    LossRatioAboveGate {
        /// 首轮平均损失。
        first: f32,
        /// 末轮平均损失。
        last: f32,
        /// 门禁比例。
        ratio: f32,
    },
    /// 末轮准确率低于门禁。
    AccuracyBelowGate {
        /// 末轮准确率。
        accuracy: f32,
        /// 门禁下限。
        gate: f32,
    },
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::LossNotDecreasing { first, last } => write!(f, "LOSS_NOT_DECREASING: first {first}, last {last}"),
            Finding::LossRatioAboveGate { first, last, ratio } => write!(f, "LOSS_RATIO_ABOVE_GATE: first {first}, last {last}, ratio gate {ratio}"),
            Finding::AccuracyBelowGate { accuracy, gate } => write!(f, "ACCURACY_BELOW_GATE: accuracy {accuracy}, gate {gate}"),
        }
    }
}

/// 一轮的结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EpochSummary {
    /// 轮号（从 0 开始）。
    pub epoch: usize,
    /// 该轮各批训练损失的平均。
    pub mean_loss: f32,
    /// 该轮结束后的全量评估准确率（整批部分）。
    pub accuracy: f32,
}

/// 验收报告。
#[derive(Debug, Clone, PartialEq)]
pub struct MlReport {
    /// 逐轮结果。
    pub epochs: Vec<EpochSummary>,
    /// 全部发现；为空即通过。
    pub findings: Vec<Finding>,
}

impl MlReport {
    /// 是否没有任何发现。
    pub fn passed(&self) -> bool {
        self.findings.is_empty()
    }
}

/// 用 [`GraphMlp`] 训练给定数据集并按门禁判定。
///
/// `sizes` 为层宽（含输入与类别数）；尾行不足 `batch_size` 时丢弃。任何训练或指标
/// 错误直接上抛——验收台不吞错误。
pub fn train_and_evaluate(
    dataset: &Dataset,
    sizes: &[usize],
    epochs: usize,
    batch_size: usize,
    learning_rate: f32,
    seed: u64,
    gate: &Gate,
) -> Result<MlReport, HarnessError> {
    if epochs == 0 {
        return Err(HarnessError::Config("epochs must be positive".to_owned()));
    }
    let mut model = GraphMlp::new(sizes, batch_size, &mut Random::new(ChaCha12::from_u64(seed, 0)))?;
    let mut optimizer = GraphAdam::new(model.training(), learning_rate)?;

    let mut summaries = Vec::with_capacity(epochs);
    for epoch in 0..epochs {
        let mut total_loss = 0.0f32;
        let mut steps = 0usize;
        for batch in dataset.batches(batch_size)? {
            let batch = batch?;
            if batch.len() != batch_size {
                continue;
            }
            total_loss += model.step_batch(&mut optimizer, batch.inputs.clone(), batch.targets.clone())?;
            steps += 1;
        }
        if steps == 0 {
            return Err(HarnessError::Config(format!("dataset has no full batch of {batch_size} rows")));
        }
        let accuracy = evaluate(&model, dataset, batch_size)?;
        summaries.push(EpochSummary { epoch, mean_loss: total_loss / steps as f32, accuracy });
    }

    let first = summaries[0].mean_loss;
    let last = summaries[summaries.len() - 1].mean_loss;
    let mut findings = Vec::new();
    if summaries.len() >= 2 && last >= first {
        findings.push(Finding::LossNotDecreasing { first, last });
    }
    if first > 0.0 && last > first * gate.max_loss_ratio {
        findings.push(Finding::LossRatioAboveGate { first, last, ratio: gate.max_loss_ratio });
    }
    if summaries[summaries.len() - 1].accuracy < gate.min_accuracy {
        findings.push(Finding::AccuracyBelowGate { accuracy: summaries[summaries.len() - 1].accuracy, gate: gate.min_accuracy });
    }
    Ok(MlReport { epochs: summaries, findings })
}

/// 全量评估：对每个整批做前向，按 argmax 比对 one-hot 目标。
fn evaluate(model: &GraphMlp, dataset: &Dataset, batch_size: usize) -> Result<f32, HarnessError> {
    let mut predictions = Vec::new();
    let mut truths = Vec::new();
    for batch in dataset.batches(batch_size)? {
        let batch = batch?;
        if batch.len() != batch_size {
            continue;
        }
        let logits = model.predict_batch(batch.inputs.clone())?;
        predictions.extend(argmax_rows(&logits));
        truths.extend(argmax_rows(&batch.targets));
    }
    Ok(ml_metrics::accuracy(&predictions, &truths)?)
}

/// 逐行 argmax；空行返回空集合。
fn argmax_rows(values: &Tensor) -> Vec<u32> {
    let width = values.shape().last().copied().unwrap_or(0);
    if width == 0 {
        return Vec::new();
    }
    values.to_vec().chunks(width).map(|row| row.iter().enumerate().fold((0usize, f32::MIN), |best, (index, value)| if *value > best.1 { (index, *value) } else { best }).0 as u32).collect()
}

/// 渲染 Markdown 报告。
pub fn render_markdown(report: &MlReport) -> String {
    let mut text = String::from("# ML 端到端验收报告\n\n| epoch | mean loss | accuracy |\n| --- | --- | --- |\n");
    for summary in &report.epochs {
        text.push_str(&format!("| {} | {:.5} | {:.4} |\n", summary.epoch, summary.mean_loss, summary.accuracy));
    }
    text.push_str(&format!("\n结论: {}\n\n## Findings\n\n", if report.passed() { "通过" } else { "不通过" }));
    if report.findings.is_empty() {
        text.push_str("- NONE\n");
    } else {
        for finding in &report.findings {
            text.push_str(&format!("- {finding}\n"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use ml_types::Sample;

    /// 线性可分的两簇数据：x 接近 0.1 为类 0，接近 0.9 为类 1。
    fn separable() -> Dataset {
        let mut samples = Vec::new();
        for index in 0..24 {
            let offset = index as f32 * 0.02;
            samples.push(Sample::new(vec![0.1 + offset, 0.5 - offset], 0));
            samples.push(Sample::new(vec![0.9 - offset, 0.5 + offset], 1));
        }
        Dataset::from_samples(samples).expect("dataset")
    }

    #[test]
    fn separable_dataset_passes_the_gate() {
        let report =
            train_and_evaluate(&separable(), &[2, 8, 2], 10, 8, 0.1, 7, &Gate { min_accuracy: 0.9, max_loss_ratio: 0.5 }).expect("run");
        assert!(report.passed(), "findings: {:?}", report.findings);
        assert_eq!(report.epochs.len(), 10);
        assert!(report.epochs.last().unwrap().accuracy >= 0.9);
        let markdown = render_markdown(&report);
        assert!(markdown.contains("结论: 通过"));
    }

    #[test]
    fn untrained_model_trips_the_accuracy_gate() {
        let report =
            train_and_evaluate(&separable(), &[2, 8, 2], 1, 8, 0.0, 7, &Gate { min_accuracy: 0.99, max_loss_ratio: 1.0 }).expect("run");
        assert!(!report.passed());
        assert!(report.findings.iter().any(|finding| matches!(finding, Finding::AccuracyBelowGate { .. })));
    }

    #[test]
    fn config_errors_are_discriminable() {
        let dataset = separable();
        assert!(matches!(train_and_evaluate(&dataset, &[2, 4], 0, 8, 0.1, 1, &Gate { min_accuracy: 0.5, max_loss_ratio: 1.0 }), Err(HarnessError::Config(_))));
        assert!(matches!(
            train_and_evaluate(&dataset, &[2, 4], 1, 100, 0.1, 1, &Gate { min_accuracy: 0.5, max_loss_ratio: 1.0 }),
            Err(HarnessError::Config(detail)) if detail.contains("no full batch")
        ));
    }
}
