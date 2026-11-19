//! 反向模式自动微分：`Var` 计算图、梯度累积与广播归约。
//!
//! 一次前向构建由 [`Var`] 组成的图；[`Var::backward`] 从标量种子出发做拓扑逆序传播。
//! 图只记录反向所需的值与形状，不持有设备句柄；设备执行在 `hardware.*` 接入后由计划层调度。
//!
//! 梯度默认累积到 `.grad`，训练循环应在每一步后调用 [`Var::zero_grad`]。

#![deny(missing_docs)]

use std::{cell::RefCell, collections::HashSet, rc::Rc};

use dl_ops as ops;
use dl_tensor::Tensor;
use dl_types::{DlError, Result, numel};

type BackwardFn = dyn Fn(&Tensor) -> Result<Vec<Option<Tensor>>>;

struct VarInner {
    value: RefCell<Tensor>,
    grad: RefCell<Option<Tensor>>,
    parents: Vec<Var>,
    backward: Option<Box<BackwardFn>>,
}

/// 自动微分变量；克隆共享同一份值与梯度。
#[derive(Clone)]
pub struct Var(Rc<VarInner>);

impl Var {
    /// 构造叶子变量（参数或输入）。
    pub fn leaf(value: Tensor) -> Self {
        Self(Rc::new(VarInner { value: RefCell::new(value), grad: RefCell::new(None), parents: Vec::new(), backward: None }))
    }

    fn from_parts(value: Tensor, parents: Vec<Var>, backward: impl Fn(&Tensor) -> Result<Vec<Option<Tensor>>> + 'static) -> Self {
        Self(Rc::new(VarInner { value: RefCell::new(value), grad: RefCell::new(None), parents, backward: Some(Box::new(backward)) }))
    }

    /// 返回当前值快照。
    pub fn value(&self) -> Tensor {
        self.0.value.borrow().clone()
    }

    /// 返回形状。
    pub fn shape(&self) -> Vec<usize> {
        self.0.value.borrow().shape().to_vec()
    }

    /// 返回已累积的梯度。
    pub fn grad(&self) -> Option<Tensor> {
        self.0.grad.borrow().clone()
    }

    /// 清空梯度。
    pub fn zero_grad(&self) {
        *self.0.grad.borrow_mut() = None;
    }

    /// 原地覆写参数值；形状必须一致。
    pub fn set_values(&self, values: &[f32]) -> Result<()> {
        self.0.value.borrow_mut().copy_values(values)
    }

    /// 从全 1 种子反向传播。
    pub fn backward(&self) -> Result<()> {
        let seed = Tensor::full(self.shape(), 1.0)?;
        self.backward_with(seed)
    }

    /// 从给定种子反向传播；种子形状必须与当前值一致。
    pub fn backward_with(&self, seed: Tensor) -> Result<()> {
        if seed.shape() != self.shape() {
            return Err(DlError::ShapeMismatch { op: "backward_with", lhs: seed.shape().to_vec(), rhs: self.shape() });
        }
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        collect(self, &mut visited, &mut order);

        *self.0.grad.borrow_mut() = Some(seed);
        for node in order.iter().rev() {
            let gradient = match node.0.grad.borrow().clone() {
                Some(gradient) => gradient,
                None => continue,
            };
            if !gradient.to_vec().iter().all(|value| value.is_finite()) {
                return Err(DlError::NonFiniteGradient);
            }
            let backward = match &node.0.backward {
                Some(backward) => backward,
                None => continue,
            };
            let gradients = backward(&gradient)?;
            for (parent, incoming) in node.0.parents.iter().zip(gradients) {
                if let Some(incoming) = incoming {
                    let mut slot = parent.0.grad.borrow_mut();
                    let combined = match slot.as_ref() {
                        Some(existing) => ops::add(existing, &incoming)?,
                        None => incoming,
                    };
                    *slot = Some(combined);
                }
            }
        }
        Ok(())
    }

    /// 逐元素相加，支持广播。
    pub fn add(&self, other: &Var) -> Result<Var> {
        let left_shape = self.shape();
        let right_shape = other.shape();
        let value = ops::add(&self.value(), &other.value())?;
        Ok(Self::from_parts(value, vec![self.clone(), other.clone()], move |gradient| {
            Ok(vec![Some(ops::sum_to_shape(gradient, &left_shape)?), Some(ops::sum_to_shape(gradient, &right_shape)?)])
        }))
    }

    /// 逐元素相减，支持广播。
    pub fn sub(&self, other: &Var) -> Result<Var> {
        let left_shape = self.shape();
        let right_shape = other.shape();
        let value = ops::sub(&self.value(), &other.value())?;
        Ok(Self::from_parts(value, vec![self.clone(), other.clone()], move |gradient| {
            Ok(vec![Some(ops::sum_to_shape(gradient, &left_shape)?), Some(ops::neg(&ops::sum_to_shape(gradient, &right_shape)?)?)])
        }))
    }

    /// 逐元素相乘，支持广播。
    pub fn mul(&self, other: &Var) -> Result<Var> {
        let left = self.value();
        let right = other.value();
        let lhs = left.clone();
        let rhs = right.clone();
        let value = ops::mul(&left, &right)?;
        Ok(Self::from_parts(value, vec![self.clone(), other.clone()], move |gradient| {
            Ok(vec![Some(ops::mul(gradient, &rhs)?), Some(ops::mul(gradient, &lhs)?)])
        }))
    }

    /// 二维矩阵乘梯度。
    pub fn matmul(&self, other: &Var) -> Result<Var> {
        let left = self.value();
        let right = other.value();
        let lhs = left.clone();
        let rhs = right.clone();
        let value = ops::matmul(&left, &right)?;
        Ok(Self::from_parts(value, vec![self.clone(), other.clone()], move |gradient| {
            let grad_left = ops::matmul(gradient, &rhs.transpose(0, 1)?)?;
            let grad_right = ops::matmul(&lhs.transpose(0, 1)?, gradient)?;
            Ok(vec![Some(grad_left), Some(grad_right)])
        }))
    }

    /// ReLU 激活。
    pub fn relu(&self) -> Result<Var> {
        let input = self.value();
        let mask = ops::map(&input, |value| if value > 0.0 { 1.0 } else { 0.0 })?;
        let value = ops::relu(&input)?;
        Ok(Self::from_parts(value, vec![self.clone()], move |gradient| Ok(vec![Some(ops::mul(gradient, &mask)?)])))
    }

    /// 改变形状。
    pub fn reshape(&self, shape: impl Into<Vec<usize>>) -> Result<Var> {
        let shape = shape.into();
        let original = self.shape();
        let value = self.value().reshape(shape)?;
        Ok(Self::from_parts(value, vec![self.clone()], move |gradient| Ok(vec![Some(gradient.reshape(original.clone())?)])))
    }

    /// 交换两个轴。
    pub fn transpose(&self, first: usize, second: usize) -> Result<Var> {
        let value = self.value().transpose(first, second)?;
        Ok(Self::from_parts(value, vec![self.clone()], move |gradient| Ok(vec![Some(gradient.transpose(first, second)?)])))
    }

    /// 全部元素求和。
    pub fn sum(&self) -> Result<Var> {
        let shape = self.shape();
        let value = ops::sum_all(&self.value())?;
        Ok(Self::from_parts(value, vec![self.clone()], move |gradient| {
            let scalar = gradient.as_slice()?[0];
            Ok(vec![Some(Tensor::full(shape.clone(), scalar)?)])
        }))
    }

    /// 全部元素求平均。
    pub fn mean(&self) -> Result<Var> {
        let shape = self.shape();
        let count = numel(&shape).max(1) as f32;
        let value = ops::mean_all(&self.value())?;
        Ok(Self::from_parts(value, vec![self.clone()], move |gradient| {
            let scalar = gradient.as_slice()?[0] / count;
            Ok(vec![Some(Tensor::full(shape.clone(), scalar)?)])
        }))
    }
}

/// 均方误差：`mean((prediction - target)^2)`，返回标量变量。
pub fn mse_loss(prediction: &Var, target: &Tensor) -> Result<Var> {
    let target = Var::leaf(target.clone());
    let difference = prediction.sub(&target)?;
    difference.mul(&difference)?.mean()
}

fn collect(node: &Var, visited: &mut HashSet<usize>, order: &mut Vec<Var>) {
    let key = Rc::as_ptr(&node.0) as usize;
    if !visited.insert(key) {
        return;
    }
    for parent in &node.0.parents {
        collect(parent, visited, order);
    }
    order.push(node.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relu_masks_negative_gradient() {
        let weight = Var::leaf(Tensor::from_vec(vec![3.0, -4.0], [2]).unwrap());
        let input = Var::leaf(Tensor::from_vec(vec![1.0, 2.0], [2]).unwrap());
        let activated = weight.mul(&input).unwrap().relu().unwrap();
        let loss = activated.sum().unwrap();
        loss.backward().unwrap();
        assert_eq!(input.grad().unwrap().to_vec(), vec![3.0, 0.0]);
        assert_eq!(weight.grad().unwrap().to_vec(), vec![1.0, 0.0]);
    }

    #[test]
    fn broadcast_bias_gradient_folds_rows() {
        let input = Var::leaf(Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0], [2, 2]).unwrap());
        let bias = Var::leaf(Tensor::from_vec(vec![0.5, -0.5], [2]).unwrap());
        input.add(&bias).unwrap().sum().unwrap().backward().unwrap();
        assert_eq!(bias.grad().unwrap().to_vec(), vec![2.0, 2.0]);
    }

    #[test]
    fn matmul_gradients_match_closed_form() {
        let left = Var::leaf(Tensor::from_vec(vec![1.0, 2.0], [1, 2]).unwrap());
        let right = Var::leaf(Tensor::from_vec(vec![3.0, 4.0], [2, 1]).unwrap());
        left.matmul(&right).unwrap().sum().unwrap().backward().unwrap();
        assert_eq!(left.grad().unwrap().to_vec(), vec![3.0, 4.0]);
        assert_eq!(right.grad().unwrap().to_vec(), vec![1.0, 2.0]);
    }
}
