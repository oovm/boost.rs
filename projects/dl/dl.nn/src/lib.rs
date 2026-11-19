//! 神经网络层：`Linear`、`ReLU`、`Sequential` 与 MLP 组装。
//!
//! 层只通过 [`Var`] 组合 `dl-autograd` 原语，不实现第二套张量或梯度。参数以共享
//! [`Var`] 暴露，训练器可以直接更新；初始权重来自 `boost-random` 的确定性源。

#![deny(missing_docs)]

use boost_random::{ChaCha12, Random};
use dl_autograd::Var;
use dl_tensor::Tensor;
use dl_types::{DlError, Result};

/// 可训练模块。
pub trait Module {
    /// 前向计算。
    fn forward(&self, input: &Var) -> Result<Var>;

    /// 返回带层级前缀的命名参数。
    fn named_parameters(&self) -> Vec<(String, Var)>;

    /// 返回全部参数。
    fn parameters(&self) -> Vec<Var> {
        self.named_parameters().into_iter().map(|(_, parameter)| parameter).collect()
    }

    /// 推理：不构建梯度，直接返回值。
    fn predict(&self, input: &Tensor) -> Result<Tensor> {
        Ok(self.forward(&Var::leaf(input.clone()))?.value())
    }
}

/// 全连接层，权重形状为 `[out_features, in_features]`。
pub struct Linear {
    /// 权重参数。
    pub weight: Var,
    /// 偏置参数。
    pub bias: Var,
}

impl Linear {
    /// 以 Kaiming uniform 初始化。
    pub fn new(in_features: usize, out_features: usize, random: &mut Random<ChaCha12>) -> Result<Self> {
        let limit = 1.0 / (in_features.max(1) as f32).sqrt();
        let weight = random_tensor(&[out_features, in_features], limit, random)?;
        let bias = random_tensor(&[out_features], limit, random)?;
        Ok(Self { weight: Var::leaf(weight), bias: Var::leaf(bias) })
    }
}

impl Module for Linear {
    fn forward(&self, input: &Var) -> Result<Var> {
        let transposed = self.weight.transpose(0, 1)?;
        input.matmul(&transposed)?.add(&self.bias)
    }

    fn named_parameters(&self) -> Vec<(String, Var)> {
        vec![("weight".to_owned(), self.weight.clone()), ("bias".to_owned(), self.bias.clone())]
    }
}

/// ReLU 层。
#[derive(Default)]
pub struct ReLU;

impl Module for ReLU {
    fn forward(&self, input: &Var) -> Result<Var> {
        input.relu()
    }

    fn named_parameters(&self) -> Vec<(String, Var)> {
        Vec::new()
    }
}

/// 顺序容器；参数名按层下标加前缀。
#[derive(Default)]
pub struct Sequential {
    layers: Vec<Box<dyn Module>>,
}

impl Sequential {
    /// 构造空容器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一层。
    pub fn add(mut self, layer: impl Module + 'static) -> Self {
        self.layers.push(Box::new(layer));
        self
    }
}

impl Module for Sequential {
    fn forward(&self, input: &Var) -> Result<Var> {
        let mut current = input.clone();
        for layer in &self.layers {
            current = layer.forward(&current)?;
        }
        Ok(current)
    }

    fn named_parameters(&self) -> Vec<(String, Var)> {
        let mut output = Vec::new();
        for (index, layer) in self.layers.iter().enumerate() {
            for (name, parameter) in layer.named_parameters() {
                output.push((format!("{index}.{name}"), parameter));
            }
        }
        output
    }
}

/// 按 `sizes` 组装 `Linear + ReLU` 堆叠，最后一层为线性输出。
pub fn mlp(sizes: &[usize], random: &mut Random<ChaCha12>) -> Result<Sequential> {
    if sizes.len() < 2 {
        return Err(DlError::Format { detail: "mlp needs at least input and output sizes".to_owned() });
    }
    let mut network = Sequential::new();
    let total = sizes.len();
    for (position, window) in sizes.windows(2).enumerate() {
        network = network.add(Linear::new(window[0], window[1], random)?);
        if position + 2 < total {
            network = network.add(ReLU);
        }
    }
    Ok(network)
}

/// 用确定性种子创建随机源。
pub fn seeded_random(seed: u64) -> Random<ChaCha12> {
    Random::new(ChaCha12::from_u64(seed, 0))
}

fn random_tensor(shape: &[usize], limit: f32, random: &mut Random<ChaCha12>) -> Result<Tensor> {
    let count: usize = shape.iter().product();
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let unit = random.unit_f32().map_err(|error| DlError::Random { detail: error.to_string() })?;
        values.push((2.0 * unit - 1.0) * limit);
    }
    Tensor::from_vec(values, shape.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_shape_and_parameter_names() {
        let mut random = seeded_random(7);
        let layer = Linear::new(3, 4, &mut random).unwrap();
        let input = Var::leaf(Tensor::zeros([2, 3]).unwrap());
        let output = layer.forward(&input).unwrap();
        assert_eq!(output.shape(), vec![2, 4]);
        let names: Vec<String> = layer.named_parameters().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["weight".to_owned(), "bias".to_owned()]);
    }

    #[test]
    fn mlp_ends_with_linear_output() {
        let mut random = seeded_random(11);
        let network = mlp(&[2, 8, 1], &mut random).unwrap();
        let output = network.predict(&Tensor::zeros([4, 2]).unwrap()).unwrap();
        assert_eq!(output.shape(), &[4, 1]);
        assert_eq!(network.named_parameters().len(), 4);
    }
}
