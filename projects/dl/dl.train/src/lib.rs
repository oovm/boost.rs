//! 优化器与训练循环。
//!
//! 训练只做三件事：前向、反向、参数更新；checkpoint 由 `dl-model` 负责。优化器持有
//! 参数 [`Var`] 的共享句柄，因此模型与优化器看到的是同一份存储。

#![warn(missing_docs)]

use std::collections::BTreeMap;

use dl_autograd::{Var, mse_loss};
use dl_graph::{Bindings, Graph, TrainingGraph};
use dl_nn::Module;
use dl_tensor::Tensor;
use dl_types::{DlError, Result};

/// 优化器合同。
pub trait Optimizer {
    /// 按已累积的梯度更新参数。
    fn step(&mut self) -> Result<()>;

    /// 清空全部参数梯度。
    fn zero_grad(&mut self);
}

/// 随机梯度下降，可选动量。
pub struct Sgd {
    parameters: Vec<Var>,
    learning_rate: f32,
    momentum: f32,
    velocity: Vec<Vec<f32>>,
}

impl Sgd {
    /// 创建无动量 SGD。
    pub fn new(parameters: Vec<Var>, learning_rate: f32) -> Self {
        let velocity = parameters.iter().map(|parameter| vec![0.0; parameter.shape().iter().product()]).collect();
        Self { parameters, learning_rate, momentum: 0.0, velocity }
    }

    /// 启用动量。
    pub fn with_momentum(mut self, momentum: f32) -> Self {
        self.momentum = momentum;
        self
    }
}

impl Optimizer for Sgd {
    fn step(&mut self) -> Result<()> {
        for (index, parameter) in self.parameters.iter().enumerate() {
            let Some(gradient) = parameter.grad()
            else {
                continue;
            };
            let gradient = gradient.to_vec();
            let mut values = parameter.value().to_vec();
            let velocity = &mut self.velocity[index];
            for (position, value) in values.iter_mut().enumerate() {
                velocity[position] = self.momentum * velocity[position] + gradient[position];
                *value -= self.learning_rate * velocity[position];
            }
            parameter.set_values(&values)?;
        }
        Ok(())
    }

    fn zero_grad(&mut self) {
        for parameter in &self.parameters {
            parameter.zero_grad();
        }
    }
}

/// Adam 优化器。
pub struct Adam {
    parameters: Vec<Var>,
    learning_rate: f32,
    beta1: f32,
    beta2: f32,
    epsilon: f32,
    step_count: u64,
    first_moment: Vec<Vec<f32>>,
    second_moment: Vec<Vec<f32>>,
}

impl Adam {
    /// 以默认超参数创建 Adam。
    pub fn new(parameters: Vec<Var>, learning_rate: f32) -> Self {
        let first_moment = parameters.iter().map(|parameter| vec![0.0; parameter.shape().iter().product()]).collect();
        let second_moment = parameters.iter().map(|parameter| vec![0.0; parameter.shape().iter().product()]).collect();
        Self { parameters, learning_rate, beta1: 0.9, beta2: 0.999, epsilon: 1e-8, step_count: 0, first_moment, second_moment }
    }
}

impl Optimizer for Adam {
    fn step(&mut self) -> Result<()> {
        self.step_count += 1;
        let bias_first = 1.0 - self.beta1.powi(self.step_count as i32);
        let bias_second = 1.0 - self.beta2.powi(self.step_count as i32);
        for (index, parameter) in self.parameters.iter().enumerate() {
            let Some(gradient) = parameter.grad()
            else {
                continue;
            };
            let gradient = gradient.to_vec();
            let mut values = parameter.value().to_vec();
            let first = &mut self.first_moment[index];
            let second = &mut self.second_moment[index];
            for (position, value) in values.iter_mut().enumerate() {
                let gradient = gradient[position];
                first[position] = self.beta1 * first[position] + (1.0 - self.beta1) * gradient;
                second[position] = self.beta2 * second[position] + (1.0 - self.beta2) * gradient * gradient;
                let corrected_first = first[position] / bias_first;
                let corrected_second = second[position] / bias_second;
                *value -= self.learning_rate * corrected_first / (corrected_second.sqrt() + self.epsilon);
            }
            parameter.set_values(&values)?;
        }
        Ok(())
    }

    fn zero_grad(&mut self) {
        for parameter in &self.parameters {
            parameter.zero_grad();
        }
    }
}

/// 基于 `dl-graph` 训练图的 SGD。
///
/// 参数存放在调用方的 [`Bindings`] 里；每一步执行整张训练图（前向 + 反向节点），再按梯度
/// 原地更新参数。同一张训练图既能直接 [`Graph::run`]，也能交给 `dl-plan` 的流式执行。
pub struct GraphSgd {
    graph: Graph,
    loss_index: usize,
    parameter_grads: Vec<(String, Vec<usize>, usize)>,
    learning_rate: f32,
    momentum: f32,
    velocity: BTreeMap<String, Vec<f32>>,
}

impl GraphSgd {
    /// 从训练图创建无动量 SGD。
    pub fn new(training: &TrainingGraph, learning_rate: f32) -> Self {
        let graph = training.graph.clone();
        let loss_index = graph.outputs().iter().position(|output| *output == training.loss).unwrap_or(0);
        let parameter_grads = training
            .gradients
            .parameters
            .iter()
            .map(|(name, node)| {
                let shape = graph.shape(*node).unwrap_or_default().to_vec();
                let index = graph.outputs().iter().position(|output| output == node).unwrap_or(0);
                (name.clone(), shape, index)
            })
            .collect();
        Self { graph, loss_index, parameter_grads, learning_rate, momentum: 0.0, velocity: BTreeMap::new() }
    }

    /// 启用动量。
    pub fn with_momentum(mut self, momentum: f32) -> Self {
        self.momentum = momentum;
        self
    }

    /// 训练图本身。
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// 执行一次前向 + 反向，按梯度更新绑定中的参数，返回损失。
    pub fn step(&mut self, bindings: &mut Bindings) -> Result<f32> {
        let values = self.graph.run(bindings)?;
        let loss = values[self.loss_index].as_slice()?[0];
        for (name, shape, index) in &self.parameter_grads {
            let gradient = values[*index].to_vec();
            let current = bindings.parameter(name).ok_or_else(|| DlError::MissingBinding { kind: "parameter", name: name.clone() })?.to_vec();
            let velocity = self.velocity.entry(name.clone()).or_insert_with(|| vec![0.0; gradient.len()]);
            let mut updated = current;
            for (position, value) in updated.iter_mut().enumerate() {
                velocity[position] = self.momentum * velocity[position] + gradient[position];
                *value -= self.learning_rate * velocity[position];
            }
            bindings.insert_parameter(name.clone(), Tensor::from_vec(updated, shape.clone())?);
        }
        Ok(loss)
    }
}

/// 训练配置。
#[derive(Clone, Copy, Debug)]
pub struct TrainConfig {
    /// 轮数。
    pub epochs: usize,
    /// 批大小。
    pub batch_size: usize,
}

impl TrainConfig {
    /// 创建配置。
    pub fn new(epochs: usize, batch_size: usize) -> Self {
        Self { epochs, batch_size }
    }
}

/// 单轮报告。
#[derive(Clone, Copy, Debug)]
pub struct EpochReport {
    /// 轮次，从 0 开始。
    pub epoch: usize,
    /// 本轮平均损失。
    pub mean_loss: f32,
}

/// 在给定批量数据上训练模型，返回每轮平均损失。
pub fn fit(
    model: &dyn Module,
    optimizer: &mut dyn Optimizer,
    inputs: &Tensor,
    targets: &Tensor,
    config: &TrainConfig,
) -> Result<Vec<EpochReport>> {
    if config.batch_size == 0 {
        return Err(DlError::Format { detail: "batch_size must be positive".to_owned() });
    }
    if inputs.rank() != 2 || targets.rank() != 2 {
        return Err(DlError::Format { detail: "fit expects rank-2 inputs and targets".to_owned() });
    }
    if inputs.shape()[0] != targets.shape()[0] {
        return Err(DlError::ShapeMismatch { op: "fit", lhs: inputs.shape().to_vec(), rhs: targets.shape().to_vec() });
    }
    let rows = inputs.shape()[0];
    let mut reports = Vec::with_capacity(config.epochs);
    for epoch in 0..config.epochs {
        let mut total_loss = 0.0f32;
        let mut batches = 0usize;
        let mut start = 0usize;
        while start < rows {
            let end = (start + config.batch_size).min(rows);
            let batch_inputs = Var::leaf(inputs.rows(start, end)?);
            let batch_targets = targets.rows(start, end)?;
            let prediction = model.forward(&batch_inputs)?;
            let loss = mse_loss(&prediction, &batch_targets)?;
            total_loss += loss.value().as_slice()?[0];
            loss.backward()?;
            optimizer.step()?;
            optimizer.zero_grad();
            batches += 1;
            start = end;
        }
        reports.push(EpochReport { epoch, mean_loss: total_loss / batches.max(1) as f32 });
    }
    Ok(reports)
}

/// 按批次来源训练模型；每次进入新轮次都会重新调用 `batches`。
///
/// 批次来源与 `ml-data` 的 [`Dataset`] 迭代器解耦：`dl-train` 只要求
/// `Result<(inputs, targets)>`，输入与目标均为 rank-2 且行数一致。
pub fn fit_iter<F, I>(model: &dyn Module, optimizer: &mut dyn Optimizer, epochs: usize, mut batches: F) -> Result<Vec<EpochReport>>
where
    F: FnMut() -> I,
    I: IntoIterator<Item = Result<(Tensor, Tensor)>>,
{
    let mut reports = Vec::with_capacity(epochs);
    for epoch in 0..epochs {
        let mut total_loss = 0.0f32;
        let mut count = 0usize;
        for batch in batches() {
            let (inputs, targets) = batch?;
            if inputs.rank() != 2 || targets.rank() != 2 {
                return Err(DlError::Format { detail: "fit_iter expects rank-2 inputs and targets".to_owned() });
            }
            if inputs.shape()[0] != targets.shape()[0] {
                return Err(DlError::ShapeMismatch { op: "fit_iter", lhs: inputs.shape().to_vec(), rhs: targets.shape().to_vec() });
            }
            let batch_inputs = Var::leaf(inputs);
            let prediction = model.forward(&batch_inputs)?;
            let loss = mse_loss(&prediction, &targets)?;
            total_loss += loss.value().as_slice()?[0];
            loss.backward()?;
            optimizer.step()?;
            optimizer.zero_grad();
            count += 1;
        }
        if count == 0 {
            return Err(DlError::Format { detail: "fit_iter received no batches".to_owned() });
        }
        reports.push(EpochReport { epoch, mean_loss: total_loss / count as f32 });
    }
    Ok(reports)
}

/// 用图训练在外部批次来源上训练。
///
/// 每个批次写入 `input` 与 `target` 两个输入绑定，然后执行一次 [`GraphSgd::step`]；
/// 其余输入/参数绑定由调用方在 [`Bindings`] 中预先放好。
pub fn fit_graph<F, I>(
    optimizer: &mut GraphSgd,
    bindings: &mut Bindings,
    input: &str,
    target: &str,
    epochs: usize,
    mut batches: F,
) -> Result<Vec<EpochReport>>
where
    F: FnMut() -> I,
    I: IntoIterator<Item = Result<(Tensor, Tensor)>>,
{
    let mut reports = Vec::with_capacity(epochs);
    for epoch in 0..epochs {
        let mut total_loss = 0.0f32;
        let mut count = 0usize;
        for batch in batches() {
            let (inputs, targets) = batch?;
            if inputs.rank() != 2 || targets.rank() != 2 {
                return Err(DlError::Format { detail: "fit_graph expects rank-2 inputs and targets".to_owned() });
            }
            bindings.insert_input(input, inputs);
            bindings.insert_input(target, targets);
            total_loss += optimizer.step(bindings)?;
            count += 1;
        }
        if count == 0 {
            return Err(DlError::Format { detail: "fit_graph received no batches".to_owned() });
        }
        reports.push(EpochReport { epoch, mean_loss: total_loss / count as f32 });
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_graph::GraphBuilder;
    use dl_nn::{mlp, seeded_random};
    use dl_tensor::Tensor;

    #[test]
    fn training_reduces_loss_and_updates_parameters() -> Result<()> {
        let mut random = seeded_random(20261119);
        let model = mlp(&[2, 8, 1], &mut random)?;
        let inputs = Tensor::from_vec(vec![0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0], [4, 2])?;
        let targets = Tensor::from_vec(vec![0.0, 1.0, 1.0, 2.0], [4, 1])?;

        let before = model.predict(&inputs)?;
        let mut optimizer = Sgd::new(model.parameters(), 0.05).with_momentum(0.9);
        let reports = fit(&model, &mut optimizer, &inputs, &targets, &TrainConfig::new(400, 4))?;

        let first = reports.first().expect("epochs").mean_loss;
        let last = reports.last().expect("epochs").mean_loss;
        assert!(last.is_finite() && first.is_finite());
        assert!(last < first * 0.5, "loss did not fall: {first} -> {last}");
        let after = model.predict(&inputs)?;
        assert_ne!(before.to_vec(), after.to_vec());
        Ok(())
    }

    #[test]
    fn adam_also_runs() -> Result<()> {
        let mut random = seeded_random(7);
        let model = mlp(&[2, 4, 1], &mut random)?;
        let inputs = Tensor::from_vec(vec![0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0], [4, 2])?;
        let targets = Tensor::from_vec(vec![0.0, 1.0, 1.0, 2.0], [4, 1])?;
        let mut optimizer = Adam::new(model.parameters(), 0.02);
        let reports = fit(&model, &mut optimizer, &inputs, &targets, &TrainConfig::new(200, 4))?;
        assert!(reports.last().expect("epochs").mean_loss.is_finite());
        Ok(())
    }

    #[test]
    fn fit_iter_trains_from_an_external_batch_source() -> Result<()> {
        let mut random = seeded_random(11);
        let model = mlp(&[2, 6, 1], &mut random)?;
        let inputs = Tensor::from_vec(vec![0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0], [4, 2])?;
        let targets = Tensor::from_vec(vec![0.0, 1.0, 1.0, 2.0], [4, 1])?;
        let first = (inputs.rows(0, 2)?, targets.rows(0, 2)?);
        let second = (inputs.rows(2, 4)?, targets.rows(2, 4)?);

        let mut optimizer = Sgd::new(model.parameters(), 0.05);
        let reports = fit_iter(&model, &mut optimizer, 100, || vec![Ok(first.clone()), Ok(second.clone())])?;
        let first_loss = reports.first().expect("epochs").mean_loss;
        let last_loss = reports.last().expect("epochs").mean_loss;
        assert!(last_loss.is_finite());
        assert!(last_loss < first_loss, "loss did not fall: {first_loss} -> {last_loss}");

        let mut empty = Sgd::new(model.parameters(), 0.05);
        let empty_batches: Vec<Result<(Tensor, Tensor)>> = Vec::new();
        let error = fit_iter(&model, &mut empty, 1, move || empty_batches.clone()).expect_err("no batches");
        assert!(matches!(error, DlError::Format { .. }));
        Ok(())
    }

    fn mlp_training_graph() -> Result<TrainingGraph> {
        let mut builder = GraphBuilder::new();
        let x = builder.input("x", [4, 2])?;
        let target = builder.input("target", [4, 1])?;
        let weight0 = builder.parameter("0.weight", [6, 2])?;
        let bias0 = builder.parameter("0.bias", [6])?;
        let weight1 = builder.parameter("2.weight", [1, 6])?;
        let bias1 = builder.parameter("2.bias", [1])?;

        let transposed0 = builder.transpose(weight0, 0, 1)?;
        let hidden = builder.matmul(x, transposed0)?;
        let hidden = builder.add(hidden, bias0)?;
        let hidden = builder.relu(hidden)?;
        let transposed1 = builder.transpose(weight1, 0, 1)?;
        let prediction = builder.matmul(hidden, transposed1)?;
        let prediction = builder.add(prediction, bias1)?;
        let difference = builder.sub(prediction, target)?;
        let squared = builder.mul(difference, difference)?;
        let loss = builder.mean_all(squared)?;
        let graph = builder.finish(vec![loss])?;
        graph.training(loss)
    }

    #[test]
    fn graph_optimizer_trains_through_the_training_graph() -> Result<()> {
        let mut random = seeded_random(31);
        let model = mlp(&[2, 6, 1], &mut random)?;
        let training = mlp_training_graph()?;

        let mut bindings = Bindings::new();
        for (name, parameter) in model.named_parameters() {
            bindings.insert_parameter(name, parameter.value());
        }
        let initial_weight = bindings.parameter("0.weight").expect("bound weight").to_vec();
        let inputs = Tensor::from_vec(vec![0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0], [4, 2])?;
        let targets = Tensor::from_vec(vec![0.0, 1.0, 1.0, 2.0], [4, 1])?;

        let mut optimizer = GraphSgd::new(&training, 0.05).with_momentum(0.9);
        let reports = fit_graph(&mut optimizer, &mut bindings, "x", "target", 400, || vec![Ok((inputs.clone(), targets.clone()))])?;

        let first = reports.first().expect("epochs").mean_loss;
        let last = reports.last().expect("epochs").mean_loss;
        assert!(last.is_finite() && last < first * 0.5, "graph training did not converge: {first} -> {last}");
        assert_ne!(bindings.parameter("0.weight").expect("updated weight").to_vec(), initial_weight);
        Ok(())
    }
}
