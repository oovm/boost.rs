//! 数据集、批次切分与表格预处理。
//!
//! 本 crate 只做数据组织，不做模型计算：批次产出 `dl-tensor` 张量，直接交给 `dl-*`。
//! 样本类型 [`Sample`] 来自 `ml-types`（单一事实源），本 crate 再导出以保持既有路径可用；
//! 标签以整数保存，批次目标使用 one-hot f32，便于与 `dl-ops` / `dl-train` 的 MSE
//! 损失衔接。

#![deny(missing_docs)]

use dl_tensor::Tensor;
use dl_types::DlError;

pub use ml_types::Sample;

/// 数据层错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataError {
    /// 特征宽度必须大于零。
    ZeroFeatureWidth,
    /// 批大小必须大于零。
    ZeroBatchSize,
    /// 类别数必须大于零。
    ZeroClasses,
    /// 样本特征宽度与数据集声明不一致。
    ShapeMismatch {
        /// 数据集声明的宽度。
        expected: usize,
        /// 样本实际宽度。
        actual: usize,
    },
    /// 标签超出 `[0, class_count)`。
    LabelOutOfRange {
        /// 越界标签。
        label: u32,
        /// 数据集声明的类别数。
        classes: usize,
    },
    /// 无法从空样本集推导数据集宽度。
    EmptyDataset,
    /// CSV 内容无法解析。
    Csv {
        /// 出错的行号（从 1 开始，含表头）。
        line: usize,
        /// 具体原因。
        detail: String,
    },
    /// IDX 字节流无法解析。
    Idx {
        /// 具体原因。
        detail: String,
    },
    /// 张量构造失败。
    Tensor(DlError),
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::ZeroFeatureWidth => write!(f, "feature width must be greater than zero"),
            DataError::ZeroBatchSize => write!(f, "batch size must be greater than zero"),
            DataError::ZeroClasses => write!(f, "class count must be greater than zero"),
            DataError::ShapeMismatch { expected, actual } => {
                write!(f, "feature width mismatch: expected {expected}, got {actual}")
            }
            DataError::LabelOutOfRange { label, classes } => write!(f, "label {label} outside class range {classes}"),
            DataError::EmptyDataset => write!(f, "cannot infer dataset width from an empty sample list"),
            DataError::Csv { line, detail } => write!(f, "csv line {line}: {detail}"),
            DataError::Idx { detail } => write!(f, "idx: {detail}"),
            DataError::Tensor(error) => write!(f, "tensor error: {error}"),
        }
    }
}

impl std::error::Error for DataError {}

impl From<DlError> for DataError {
    fn from(error: DlError) -> Self {
        DataError::Tensor(error)
    }
}

/// 解析 IDX 头，返回 `(样本数, 行, 列, 数据偏移)`；一维流返回 `1, 1`。
fn idx_header(bytes: &[u8], expected_magic: u32, kind: &str) -> Result<(usize, usize, usize, usize), DataError> {
    if bytes.len() < 4 {
        return Err(DataError::Idx { detail: format!("{kind} stream is shorter than the magic") });
    }
    let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic != expected_magic {
        return Err(DataError::Idx { detail: format!("{kind} magic is 0x{magic:08x}, expected 0x{expected_magic:08x}") });
    }
    let rank = (magic & 0xff) as usize;
    if rank == 0 || rank > 3 {
        return Err(DataError::Idx { detail: format!("{kind} rank {rank} is not supported") });
    }
    let header = 4 * (rank + 1);
    if bytes.len() < header {
        return Err(DataError::Idx { detail: format!("{kind} header is truncated") });
    }
    let dimension = |index: usize| {
        let offset = 4 * (index + 1);
        u32::from_be_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]]) as usize
    };
    let rows = if rank >= 2 { dimension(1) } else { 1 };
    let cols = if rank >= 3 { dimension(2) } else { 1 };
    Ok((dimension(0), rows, cols, header))
}

/// 固定特征宽度与类别数的数据集。
#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    feature_count: usize,
    class_count: usize,
    samples: Vec<Sample>,
}

impl Dataset {
    /// 用显式宽度与类别数构造并校验数据集。
    pub fn new(feature_count: usize, class_count: usize, samples: Vec<Sample>) -> Result<Self, DataError> {
        if feature_count == 0 {
            return Err(DataError::ZeroFeatureWidth);
        }
        if class_count == 0 {
            return Err(DataError::ZeroClasses);
        }
        for sample in &samples {
            if sample.features.len() != feature_count {
                return Err(DataError::ShapeMismatch { expected: feature_count, actual: sample.features.len() });
            }
            if sample.label as usize >= class_count {
                return Err(DataError::LabelOutOfRange { label: sample.label, classes: class_count });
            }
        }
        Ok(Self { feature_count, class_count, samples })
    }

    /// 从样本推导特征宽度与类别数（最大标签 + 1）。
    pub fn from_samples(samples: Vec<Sample>) -> Result<Self, DataError> {
        let first = samples.first().ok_or(DataError::EmptyDataset)?;
        let feature_count = first.features.len();
        let class_count = samples.iter().map(|sample| sample.label as usize + 1).max().unwrap_or(0);
        Self::new(feature_count, class_count, samples)
    }

    /// 从 CSV 文本构建数据集。
    ///
    /// `label_column` 指向标签列（非负整数），其余列按顺序作为 f32 特征；`has_header`
    /// 为 true 时跳过首行。空行被忽略，列数不一致、非数值或空输入都返回
    /// [`DataError::Csv`]。
    pub fn from_csv(text: &str, has_header: bool, label_column: usize) -> Result<Self, DataError> {
        let mut samples = Vec::new();
        let mut width = None;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            if has_header && index == 0 {
                continue;
            }
            let row = raw.trim();
            if row.is_empty() {
                continue;
            }
            let columns: Vec<&str> = row.split(',').map(str::trim).collect();
            if label_column >= columns.len() {
                return Err(DataError::Csv {
                    line,
                    detail: format!("label column {label_column} is out of range for {} columns", columns.len()),
                });
            }
            match width {
                None => width = Some(columns.len()),
                Some(expected) if expected != columns.len() => {
                    return Err(DataError::Csv { line, detail: format!("expected {expected} columns, found {}", columns.len()) });
                }
                Some(_) => {}
            }
            let mut features = Vec::with_capacity(columns.len() - 1);
            for (column, value) in columns.iter().enumerate() {
                if column == label_column {
                    continue;
                }
                let feature = value.parse::<f32>().map_err(|_| DataError::Csv { line, detail: format!("`{value}` is not a number") })?;
                features.push(feature);
            }
            let label = columns[label_column]
                .parse::<u32>()
                .map_err(|_| DataError::Csv { line, detail: format!("`{}` is not a non-negative integer label", columns[label_column]) })?;
            samples.push(Sample::new(features, label));
        }
        if samples.is_empty() {
            return Err(DataError::EmptyDataset);
        }
        Self::from_samples(samples)
    }

    /// 从 IDX 字节构建数据集。
    ///
    /// 图像流必须是 `0x00000803`（u8，`[count, rows, cols]`），标签流必须是
    /// `0x00000801`；像素线性缩放到 `[0, 1]`，标签原样作为类别。数量不一致、magic
    /// 不符或数据长度不足都返回 [`DataError::Idx`]。
    pub fn from_idx(images: &[u8], labels: &[u8]) -> Result<Self, DataError> {
        Self::from_idx_limited(images, labels, usize::MAX)
    }

    /// 与 [`Dataset::from_idx`] 相同，但最多读取 `limit` 条样本；用于冒烟测试与大集抽样。
    pub fn from_idx_limited(images: &[u8], labels: &[u8], limit: usize) -> Result<Self, DataError> {
        let (image_count, rows, cols, image_offset) = idx_header(images, 0x0000_0803, "images")?;
        let (label_count, _, _, label_offset) = idx_header(labels, 0x0000_0801, "labels")?;
        if image_count != label_count {
            return Err(DataError::Idx { detail: format!("{image_count} images but {label_count} labels") });
        }
        let count = image_count.min(limit);
        let width = rows * cols;
        let pixel_bytes = count.checked_mul(width).ok_or(DataError::Idx { detail: "image byte count overflow".into() })?;
        if images.len() < image_offset + pixel_bytes {
            return Err(DataError::Idx { detail: format!("image stream truncated at {pixel_bytes} bytes") });
        }
        if labels.len() < label_offset + count {
            return Err(DataError::Idx { detail: format!("label stream truncated at {count} labels") });
        }

        let mut samples = Vec::with_capacity(count);
        for index in 0..count {
            let start = image_offset + index * width;
            let features = images[start..start + width].iter().map(|pixel| *pixel as f32 / 255.0).collect::<Vec<f32>>();
            samples.push(Sample::new(features, labels[label_offset + index] as u32));
        }
        Self::from_samples(samples)
    }

    /// 用给定种子确定性打乱样本顺序。
    pub fn shuffled(&self, seed: u64) -> Self {
        let mut samples = self.samples.clone();
        let mut state = seed | 1;
        for index in (1..samples.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            samples.swap(index, (state % (index as u64 + 1)) as usize);
        }
        Self { feature_count: self.feature_count, class_count: self.class_count, samples }
    }

    /// 特征宽度。
    pub fn feature_count(&self) -> usize {
        self.feature_count
    }

    /// 类别数。
    pub fn class_count(&self) -> usize {
        self.class_count
    }

    /// 样本数。
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// 是否没有样本。
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 只读样本切片。
    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// 按 `batch_size` 切分批次；最后一批保留剩余样本。
    pub fn batches(&self, batch_size: usize) -> Result<Batches<'_>, DataError> {
        if batch_size == 0 {
            return Err(DataError::ZeroBatchSize);
        }
        Ok(Batches { dataset: self, batch_size, cursor: 0 })
    }

    /// 把全部样本物化为 `(inputs [n, features], targets [n, classes])`。
    ///
    /// 与 [`Dataset::batches`] 使用同一 one-hot 规则，可直接交给 `dl-train`。
    pub fn tensors(&self) -> Result<(Tensor, Tensor), DataError> {
        build_batch(&self.samples, self.feature_count, self.class_count).map(|batch| (batch.inputs, batch.targets))
    }
}

/// 一批已物化为张量的数据：输入 `[n, features]`，目标 `[n, classes]` one-hot。
#[derive(Debug, Clone)]
pub struct Batch {
    /// 输入特征。
    pub inputs: Tensor,
    /// one-hot 目标。
    pub targets: Tensor,
}

impl Batch {
    /// 批内样本数。
    pub fn len(&self) -> usize {
        self.inputs.shape().first().copied().unwrap_or(0)
    }

    /// 是否为空批（正常切分不会产生）。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 顺序批次迭代器。
pub struct Batches<'a> {
    dataset: &'a Dataset,
    batch_size: usize,
    cursor: usize,
}

impl Iterator for Batches<'_> {
    type Item = Result<Batch, DataError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.dataset.len() {
            return None;
        }
        let end = (self.cursor + self.batch_size).min(self.dataset.len());
        let slice = &self.dataset.samples[self.cursor..end];
        self.cursor = end;
        Some(build_batch(slice, self.dataset.feature_count, self.dataset.class_count))
    }
}

fn build_batch(samples: &[Sample], feature_count: usize, class_count: usize) -> Result<Batch, DataError> {
    let mut features = Vec::with_capacity(samples.len() * feature_count);
    let mut targets = vec![0.0f32; samples.len() * class_count];
    for (row, sample) in samples.iter().enumerate() {
        features.extend_from_slice(&sample.features);
        targets[row * class_count + sample.label as usize] = 1.0;
    }
    let inputs = Tensor::from_vec(features, [samples.len(), feature_count])?;
    let targets = Tensor::from_vec(targets, [samples.len(), class_count])?;
    Ok(Batch { inputs, targets })
}

/// 逐特征标准化：减均值、除以总体标准差。
///
/// 标准差为零（常量特征）时使用 1.0，避免除零。
#[derive(Debug, Clone, PartialEq)]
pub struct Standardizer {
    means: Vec<f32>,
    stds: Vec<f32>,
}

impl Standardizer {
    /// 在数据集上拟合均值与标准差。
    pub fn fit(dataset: &Dataset) -> Self {
        let width = dataset.feature_count();
        let count = dataset.len();
        let mut means = vec![0.0f32; width];
        if count > 0 {
            for sample in dataset.samples() {
                for (mean, value) in means.iter_mut().zip(&sample.features) {
                    *mean += value;
                }
            }
            for mean in &mut means {
                *mean /= count as f32;
            }
        }
        let mut stds = vec![0.0f32; width];
        if count > 0 {
            for sample in dataset.samples() {
                for (index, value) in sample.features.iter().enumerate() {
                    let delta = value - means[index];
                    stds[index] += delta * delta;
                }
            }
            for std in &mut stds {
                let variance = *std / count as f32;
                let stddev = variance.sqrt();
                *std = if stddev == 0.0 { 1.0 } else { stddev };
            }
        }
        else {
            stds.fill(1.0);
        }
        Self { means, stds }
    }

    /// 返回逐特征均值。
    pub fn means(&self) -> &[f32] {
        &self.means
    }

    /// 返回逐特征标准差。
    pub fn stds(&self) -> &[f32] {
        &self.stds
    }

    /// 标准化一条特征向量。
    pub fn transform(&self, features: &[f32]) -> Result<Vec<f32>, DataError> {
        if features.len() != self.means.len() {
            return Err(DataError::ShapeMismatch { expected: self.means.len(), actual: features.len() });
        }
        Ok(features.iter().zip(&self.means).zip(&self.stds).map(|((value, mean), std)| (value - mean) / std).collect())
    }

    /// 标准化整个数据集，保留标签与宽度。
    pub fn transform_dataset(&self, dataset: &Dataset) -> Result<Dataset, DataError> {
        let samples = dataset
            .samples()
            .iter()
            .map(|sample| self.transform(&sample.features).map(|features| Sample { features, label: sample.label }))
            .collect::<Result<Vec<_>, _>>()?;
        Dataset::new(dataset.feature_count(), dataset.class_count(), samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset() -> Dataset {
        Dataset::from_samples(vec![
            Sample::new(vec![1.0, 10.0], 0),
            Sample::new(vec![2.0, 20.0], 1),
            Sample::new(vec![3.0, 30.0], 1),
            Sample::new(vec![4.0, 40.0], 0),
            Sample::new(vec![5.0, 50.0], 1),
        ])
        .expect("dataset")
    }

    #[test]
    fn infers_width_and_classes() {
        let data = dataset();
        assert_eq!(data.feature_count(), 2);
        assert_eq!(data.class_count(), 2);
        assert_eq!(data.len(), 5);
        assert!(matches!(Dataset::from_samples(Vec::new()), Err(DataError::EmptyDataset)));
    }

    #[test]
    fn rejects_bad_samples() {
        let bad_width = Dataset::from_samples(vec![Sample::new(vec![1.0, 2.0], 0), Sample::new(vec![1.0], 0)]);
        assert!(matches!(bad_width, Err(DataError::ShapeMismatch { expected: 2, actual: 1 })));
        let bad_label = Dataset::new(1, 2, vec![Sample::new(vec![1.0], 2)]);
        assert!(matches!(bad_label, Err(DataError::LabelOutOfRange { label: 2, classes: 2 })));
        assert!(matches!(Dataset::new(0, 1, Vec::new()), Err(DataError::ZeroFeatureWidth)));
        assert!(matches!(Dataset::new(1, 0, Vec::new()), Err(DataError::ZeroClasses)));
    }

    #[test]
    fn batches_cover_tail_and_one_hot_targets() {
        let data = dataset();
        let batches: Vec<Batch> = data.batches(2).unwrap().map(|batch| batch.unwrap()).collect();
        assert_eq!(batches.len(), 3);
        assert_eq!(batches.iter().map(Batch::len).collect::<Vec<_>>(), vec![2, 2, 1]);
        assert_eq!(batches[0].inputs.shape(), [2, 2]);
        assert_eq!(batches[0].targets.shape(), [2, 2]);
        assert_eq!(batches[0].targets.to_vec(), vec![1.0, 0.0, 0.0, 1.0]);
        assert_eq!(batches[2].targets.to_vec(), vec![0.0, 1.0]);
        assert!(matches!(data.batches(0), Err(DataError::ZeroBatchSize)));
    }

    #[test]
    fn standardizer_centers_scales_and_handles_constant_features() {
        let data = Dataset::from_samples(vec![Sample::new(vec![1.0, 5.0], 0), Sample::new(vec![3.0, 5.0], 1)]).unwrap();
        let standardizer = Standardizer::fit(&data);
        assert_eq!(standardizer.means(), &[2.0, 5.0]);
        assert_eq!(standardizer.stds(), &[1.0, 1.0]);
        assert_eq!(standardizer.transform(&[3.0, 5.0]).unwrap(), vec![1.0, 0.0]);
        assert!(matches!(standardizer.transform(&[1.0]), Err(DataError::ShapeMismatch { .. })));

        let transformed = standardizer.transform_dataset(&data).unwrap();
        assert_eq!(transformed.samples()[0], Sample::new(vec![-1.0, 0.0], 0));
        assert_eq!(transformed.class_count(), 2);
    }

    #[test]
    fn csv_builds_a_dataset_and_skips_the_header() {
        let dataset = Dataset::from_csv("x,y,label\n1,2,0\n3,4,1\n", true, 2).unwrap();
        assert_eq!(dataset.feature_count(), 2);
        assert_eq!(dataset.class_count(), 2);
        assert_eq!(dataset.len(), 2);
        assert_eq!(dataset.samples()[1], Sample::new(vec![3.0, 4.0], 1));

        let no_header = Dataset::from_csv("1,2,0\n", false, 0).unwrap();
        assert_eq!(no_header.samples()[0], Sample::new(vec![2.0, 0.0], 1));
    }

    #[test]
    fn csv_reports_line_accurate_errors() {
        assert!(matches!(Dataset::from_csv("1,2,0\n3,4\n", false, 2), Err(DataError::Csv { line: 2, .. })));
        assert!(matches!(Dataset::from_csv("a,1,0\n", false, 2), Err(DataError::Csv { line: 1, .. })));
        assert!(matches!(Dataset::from_csv("1,2,x\n", false, 2), Err(DataError::Csv { line: 1, .. })));
        assert_eq!(Dataset::from_csv("", false, 0), Err(DataError::EmptyDataset));
    }

    #[test]
    fn shuffled_is_seeded_and_preserves_every_sample() {
        let dataset = Dataset::from_samples((0..8u32).map(|index| Sample::new(vec![index as f32], index % 3)).collect()).unwrap();
        let first = dataset.shuffled(7);
        assert_eq!(first, dataset.shuffled(7));
        let mut labels: Vec<u32> = first.samples().iter().map(|sample| sample.label).collect();
        labels.sort_unstable();
        assert_eq!(labels, vec![0, 0, 0, 1, 1, 1, 2, 2]);
        assert_eq!(first.feature_count(), dataset.feature_count());
        assert_eq!(first.class_count(), dataset.class_count());
    }

    #[test]
    fn idx_streams_become_scaled_samples() {
        let mut images = Vec::new();
        images.extend_from_slice(&0x0000_0803u32.to_be_bytes());
        for dimension in [2u32, 2, 2] {
            images.extend_from_slice(&dimension.to_be_bytes());
        }
        images.extend_from_slice(&[0, 51, 102, 255, 10, 20, 30, 40]);

        let mut labels = Vec::new();
        labels.extend_from_slice(&0x0000_0801u32.to_be_bytes());
        labels.extend_from_slice(&2u32.to_be_bytes());
        labels.extend_from_slice(&[3, 7]);

        let dataset = Dataset::from_idx(&images, &labels).unwrap();
        assert_eq!(dataset.feature_count(), 4);
        assert_eq!(dataset.class_count(), 8);
        assert_eq!(dataset.samples()[0], Sample::new(vec![0.0, 0.2, 0.4, 1.0], 3));
        assert_eq!(dataset.samples()[1].label, 7);

        let limited = Dataset::from_idx_limited(&images, &labels, 1).unwrap();
        assert_eq!(limited.len(), 1);

        assert!(matches!(Dataset::from_idx(&images[..10], &labels), Err(DataError::Idx { .. })));
        let wrong_magic = [0u8; 28];
        assert!(matches!(Dataset::from_idx(&wrong_magic, &labels), Err(DataError::Idx { .. })));
        assert!(matches!(Dataset::from_idx_limited(&images, &labels[..5], 2), Err(DataError::Idx { .. })));
    }
}
