//! 张量计算图 IR：构建期形状推导、校验与前向执行。
//!
//! 图只描述算子依赖和形状，不持有数据。运行期通过 [`Bindings`] 绑定输入与参数；图执行
//! 与 eager（`dl-autograd`）共用 `dl-ops` 内核，因此同一组权重必须给出相同结果。
//!
//! 前向与反向都是图节点：[`GraphBuilder::backward`] 按算子解析式追加梯度节点，因此训练图
//! 与前向图一样能被 `dl-plan` 放置、被 `dl-runtime` 流式执行。eager 自动微分在
//! `dl-autograd`，参数更新与 checkpoint 在 `dl-train`/`dl-model`；可视化投影不是执行证明。

#![warn(missing_docs)]

use std::collections::BTreeMap;

use dl_ops as ops;
use dl_tensor::Tensor;
use dl_types::{DlError, Result, broadcast_shape, numel, require_non_zero};

/// 图节点下标；按创建顺序递增，输入节点总是先于消费者出现。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub usize);

/// 节点算子。
///
/// 前向算子与反向算子共用同一 IR：反向建图只是把梯度计算追加成普通节点，因此放置与
/// 流式执行对训练图不需要任何特殊分支。
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// 运行期输入。
    Input {
        /// 绑定名。
        name: String,
    },
    /// 运行期参数（权重、偏置）。
    Parameter {
        /// 绑定名。
        name: String,
    },
    /// 标量常量；反向种子和数值常数用它。
    Constant {
        /// 常量值。
        value: f32,
    },
    /// 逐元素相加，支持广播。
    Add,
    /// 逐元素相减，支持广播。
    Sub,
    /// 逐元素相乘，支持广播。
    Mul,
    /// 二维矩阵乘。
    MatMul,
    /// ReLU 激活。
    Relu,
    /// 连续视图改形状。
    Reshape {
        /// 目标形状。
        shape: Vec<usize>,
    },
    /// 交换两个轴。
    Transpose {
        /// 第一个轴。
        first: usize,
        /// 第二个轴。
        second: usize,
    },
    /// 全部元素求和，输出标量。
    SumAll,
    /// 全部元素求平均，输出标量。
    MeanAll,
    /// 把广播后的梯度按目标形状求和。
    SumToShape {
        /// 目标形状。
        shape: Vec<usize>,
    },
    /// 逐元素取负。
    Neg,
    /// 把标量广播到目标形状；只接受标量输入。
    BroadcastTo {
        /// 目标形状。
        shape: Vec<usize>,
    },
    /// 全部元素乘以标量。
    Scale {
        /// 缩放系数。
        factor: f32,
    },
    /// ReLU 反向：`gradient × (input > 0)`，输入顺序为「原输入, 上游梯度」。
    ReluBackward,
    /// 多分类 softmax 交叉熵，输入顺序为「logits, one-hot 目标」，输出按行平均的标量。
    SoftmaxCrossEntropy,
    /// softmax 交叉熵对 logits 的梯度 `(softmax - target) / rows`。
    SoftmaxCrossEntropyBackward,
}

impl Op {
    /// 算子名。
    ///
    /// 这是算子在规划、签名和日志里的唯一名字：新增算子必须在这里登记，`dl-plan` 等下游
    /// 只按名字查询，不再各自维护一份算子清单。
    pub fn name(&self) -> &'static str {
        match self {
            Op::Input { .. } => "input",
            Op::Parameter { .. } => "parameter",
            Op::Constant { .. } => "constant",
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            Op::MatMul => "matmul",
            Op::Relu => "relu",
            Op::Reshape { .. } => "reshape",
            Op::Transpose { .. } => "transpose",
            Op::SumAll => "sum_all",
            Op::MeanAll => "mean_all",
            Op::SumToShape { .. } => "sum_to_shape",
            Op::Neg => "neg",
            Op::BroadcastTo { .. } => "broadcast_to",
            Op::Scale { .. } => "scale",
            Op::ReluBackward => "relu_backward",
            Op::SoftmaxCrossEntropy => "softmax_cross_entropy",
            Op::SoftmaxCrossEntropyBackward => "softmax_cross_entropy_backward",
        }
    }
}

#[derive(Clone, Debug)]
struct Node {
    op: Op,
    inputs: Vec<NodeId>,
    shape: Vec<usize>,
}

/// 运行期绑定：按名字提供输入张量与参数。
#[derive(Clone, Debug, Default)]
pub struct Bindings {
    inputs: BTreeMap<String, Tensor>,
    parameters: BTreeMap<String, Tensor>,
}

impl Bindings {
    /// 空绑定。
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入或替换一个输入张量。
    pub fn insert_input(&mut self, name: impl Into<String>, tensor: Tensor) {
        self.inputs.insert(name.into(), tensor);
    }

    /// 插入或替换一个参数张量。
    pub fn insert_parameter(&mut self, name: impl Into<String>, tensor: Tensor) {
        self.parameters.insert(name.into(), tensor);
    }

    /// 返回已绑定的输入。
    pub fn input(&self, name: &str) -> Option<&Tensor> {
        self.inputs.get(name)
    }

    /// 返回已绑定的参数。
    pub fn parameter(&self, name: &str) -> Option<&Tensor> {
        self.parameters.get(name)
    }
}

/// 反向建图得到的梯度节点。
///
/// 节点是按名字对应的参数/输入在反向图里的梯度，可直接作为 [`GraphBuilder::finish`] 的输出，
/// 与损失一起执行。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gradients {
    /// 参数名与梯度节点，按参数声明顺序。
    pub parameters: Vec<(String, NodeId)>,
    /// 输入名与梯度节点，按输入声明顺序。
    pub inputs: Vec<(String, NodeId)>,
}

impl Gradients {
    /// 查询某个参数的梯度节点。
    pub fn parameter(&self, name: &str) -> Option<NodeId> {
        self.parameters.iter().find(|(parameter, _)| parameter == name).map(|(_, node)| *node)
    }

    /// 查询某个输入的梯度节点。
    pub fn input(&self, name: &str) -> Option<NodeId> {
        self.inputs.iter().find(|(input, _)| input == name).map(|(_, node)| *node)
    }

    /// 按参数在前、输入在后的顺序遍历全部梯度节点。
    pub fn nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.parameters.iter().chain(self.inputs.iter()).map(|(_, node)| *node)
    }
}

/// 图构建器；形状在构建期推导，非法形状立即返回错误。
#[derive(Clone, Debug, Default)]
pub struct GraphBuilder {
    nodes: Vec<Node>,
}

impl GraphBuilder {
    /// 空构建器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 复制一张已有图，节点下标保持不变；用于在图上继续追加节点。
    pub fn from_graph(graph: &Graph) -> Self {
        Self { nodes: graph.nodes.clone() }
    }

    /// 声明一个运行期输入。
    pub fn input(&mut self, name: impl Into<String>, shape: impl Into<Vec<usize>>) -> Result<NodeId> {
        let shape = shape.into();
        require_non_zero(&shape, "input")?;
        Ok(self.push(Op::Input { name: name.into() }, Vec::new(), shape))
    }

    /// 声明一个运行期参数。
    pub fn parameter(&mut self, name: impl Into<String>, shape: impl Into<Vec<usize>>) -> Result<NodeId> {
        let shape = shape.into();
        require_non_zero(&shape, "parameter")?;
        Ok(self.push(Op::Parameter { name: name.into() }, Vec::new(), shape))
    }

    /// 逐元素相加，支持广播。
    pub fn add(&mut self, lhs: NodeId, rhs: NodeId) -> Result<NodeId> {
        self.binary("add", lhs, rhs, Op::Add)
    }

    /// 逐元素相减，支持广播。
    pub fn sub(&mut self, lhs: NodeId, rhs: NodeId) -> Result<NodeId> {
        self.binary("sub", lhs, rhs, Op::Sub)
    }

    /// 逐元素相乘，支持广播。
    pub fn mul(&mut self, lhs: NodeId, rhs: NodeId) -> Result<NodeId> {
        self.binary("mul", lhs, rhs, Op::Mul)
    }

    /// 二维矩阵乘 `[m, k] × [k, n]`。
    pub fn matmul(&mut self, lhs: NodeId, rhs: NodeId) -> Result<NodeId> {
        let left = self.shape_of(lhs)?;
        let right = self.shape_of(rhs)?;
        if left.len() != 2 || right.len() != 2 || left[1] != right[0] {
            return Err(DlError::ShapeMismatch { op: "matmul", lhs: left.to_vec(), rhs: right.to_vec() });
        }
        let shape = vec![left[0], right[1]];
        Ok(self.push(Op::MatMul, vec![lhs, rhs], shape))
    }

    /// ReLU 激活。
    pub fn relu(&mut self, input: NodeId) -> Result<NodeId> {
        let shape = self.shape_of(input)?.to_vec();
        Ok(self.push(Op::Relu, vec![input], shape))
    }

    /// 多分类 softmax 交叉熵；`logits` 与 `targets` 必须是同形状的 `[rows, classes]`。
    pub fn softmax_cross_entropy(&mut self, logits: NodeId, targets: NodeId) -> Result<NodeId> {
        let logits_shape = self.shape_of(logits)?.to_vec();
        let targets_shape = self.shape_of(targets)?.to_vec();
        if logits_shape.len() != 2 || logits_shape != targets_shape {
            return Err(DlError::ShapeMismatch { op: "softmax_cross_entropy", lhs: logits_shape, rhs: targets_shape });
        }
        Ok(self.push(Op::SoftmaxCrossEntropy, vec![logits, targets], Vec::new()))
    }

    /// 改变形状；元素总数必须一致。
    pub fn reshape(&mut self, input: NodeId, shape: impl Into<Vec<usize>>) -> Result<NodeId> {
        let shape = shape.into();
        let source = self.shape_of(input)?;
        if numel(source) != numel(&shape) {
            return Err(DlError::ElementCount { expected: numel(source), actual: numel(&shape) });
        }
        Ok(self.push(Op::Reshape { shape: shape.clone() }, vec![input], shape))
    }

    /// 交换两个轴。
    pub fn transpose(&mut self, input: NodeId, first: usize, second: usize) -> Result<NodeId> {
        let source = self.shape_of(input)?;
        let rank = source.len();
        if first >= rank {
            return Err(DlError::InvalidAxis { axis: first, rank });
        }
        if second >= rank {
            return Err(DlError::InvalidAxis { axis: second, rank });
        }
        let mut shape = source.to_vec();
        shape.swap(first, second);
        Ok(self.push(Op::Transpose { first, second }, vec![input], shape))
    }

    /// 全部元素求和，输出标量。
    pub fn sum_all(&mut self, input: NodeId) -> Result<NodeId> {
        self.shape_of(input)?;
        Ok(self.push(Op::SumAll, vec![input], Vec::new()))
    }

    /// 全部元素求平均，输出标量。
    pub fn mean_all(&mut self, input: NodeId) -> Result<NodeId> {
        self.shape_of(input)?;
        Ok(self.push(Op::MeanAll, vec![input], Vec::new()))
    }

    /// 标量常量节点；反向种子用它，不占任何绑定。
    pub fn constant(&mut self, value: f32) -> NodeId {
        self.push(Op::Constant { value }, Vec::new(), Vec::new())
    }

    /// 把广播后的梯度归约回目标形状。
    pub fn sum_to_shape(&mut self, input: NodeId, shape: impl Into<Vec<usize>>) -> Result<NodeId> {
        let target = shape.into();
        let source = self.shape_of(input)?;
        let merged = broadcast_shape(&target, source, "sum_to_shape")?;
        if merged.as_slice() != source {
            return Err(DlError::ShapeMismatch { op: "sum_to_shape", lhs: source.to_vec(), rhs: target });
        }
        Ok(self.push(Op::SumToShape { shape: target.clone() }, vec![input], target))
    }

    /// 逐元素取负。
    pub fn neg(&mut self, input: NodeId) -> Result<NodeId> {
        let shape = self.shape_of(input)?.to_vec();
        Ok(self.push(Op::Neg, vec![input], shape))
    }

    /// 把标量广播到目标形状。
    pub fn broadcast_to(&mut self, input: NodeId, shape: impl Into<Vec<usize>>) -> Result<NodeId> {
        let target = shape.into();
        let source = self.shape_of(input)?.to_vec();
        if !source.is_empty() {
            return Err(DlError::ShapeMismatch { op: "broadcast_to", lhs: source, rhs: target });
        }
        require_non_zero(&target, "broadcast_to")?;
        Ok(self.push(Op::BroadcastTo { shape: target.clone() }, vec![input], target))
    }

    /// 全部元素乘以标量。
    pub fn scale(&mut self, input: NodeId, factor: f32) -> Result<NodeId> {
        let shape = self.shape_of(input)?.to_vec();
        Ok(self.push(Op::Scale { factor }, vec![input], shape))
    }

    /// ReLU 反向：梯度只通过正区间。
    pub fn relu_backward(&mut self, input: NodeId, gradient: NodeId) -> Result<NodeId> {
        let shape = self.shape_of(input)?.to_vec();
        let gradient_shape = self.shape_of(gradient)?.to_vec();
        if gradient_shape != shape {
            return Err(DlError::ShapeMismatch { op: "relu_backward", lhs: shape, rhs: gradient_shape });
        }
        Ok(self.push(Op::ReluBackward, vec![input, gradient], shape))
    }

    /// 从标量输出反向建图，种子为标量 1。
    pub fn backward(&mut self, output: NodeId) -> Result<Gradients> {
        let shape = self.shape_of(output)?.to_vec();
        if !shape.is_empty() {
            return Err(DlError::ShapeMismatch { op: "backward", lhs: shape, rhs: Vec::new() });
        }
        let seed = self.constant(1.0);
        self.backward_with_seed(output, seed)
    }

    /// 从给定种子节点反向建图。
    ///
    /// 梯度计算以普通节点追加进图，因此训练图可以直接交给 `dl-plan` 放置、交给
    /// `dl-runtime` 流式执行。只有真正收到梯度的参数与输入会出现在返回值里。
    pub fn backward_with_seed(&mut self, output: NodeId, seed: NodeId) -> Result<Gradients> {
        let output_shape = self.shape_of(output)?.to_vec();
        let seed_shape = self.shape_of(seed)?.to_vec();
        if seed_shape != output_shape {
            return Err(DlError::ShapeMismatch { op: "backward_seed", lhs: seed_shape, rhs: output_shape });
        }

        let count = self.nodes.len();
        let mut pending: Vec<Option<NodeId>> = vec![None; count];
        pending[output.0] = Some(seed);

        for index in (0..count).rev() {
            let gradient = match pending[index] {
                Some(node) => node,
                None => continue,
            };
            let node = self.nodes[index].clone();
            match node.op {
                Op::Input { .. } | Op::Parameter { .. } | Op::Constant { .. } => {}
                Op::Add => {
                    for parent in node.inputs {
                        let shape = self.shape_of(parent)?.to_vec();
                        let contribution = self.sum_to_shape(gradient, shape)?;
                        self.accumulate(&mut pending[parent.0], contribution)?;
                    }
                }
                Op::Sub => {
                    let (left, right) = (node.inputs[0], node.inputs[1]);
                    let left_shape = self.shape_of(left)?.to_vec();
                    let right_shape = self.shape_of(right)?.to_vec();
                    let left_gradient = self.sum_to_shape(gradient, left_shape)?;
                    let folded = self.sum_to_shape(gradient, right_shape)?;
                    let right_gradient = self.neg(folded)?;
                    self.accumulate(&mut pending[left.0], left_gradient)?;
                    self.accumulate(&mut pending[right.0], right_gradient)?;
                }
                Op::Mul => {
                    let (left, right) = (node.inputs[0], node.inputs[1]);
                    let left_shape = self.shape_of(left)?.to_vec();
                    let right_shape = self.shape_of(right)?.to_vec();
                    let left_product = self.mul(gradient, right)?;
                    let left_gradient = self.sum_to_shape(left_product, left_shape)?;
                    let right_product = self.mul(gradient, left)?;
                    let right_gradient = self.sum_to_shape(right_product, right_shape)?;
                    self.accumulate(&mut pending[left.0], left_gradient)?;
                    self.accumulate(&mut pending[right.0], right_gradient)?;
                }
                Op::MatMul => {
                    let (left, right) = (node.inputs[0], node.inputs[1]);
                    let right_transposed = self.transpose(right, 0, 1)?;
                    let left_transposed = self.transpose(left, 0, 1)?;
                    let left_gradient = self.matmul(gradient, right_transposed)?;
                    let right_gradient = self.matmul(left_transposed, gradient)?;
                    self.accumulate(&mut pending[left.0], left_gradient)?;
                    self.accumulate(&mut pending[right.0], right_gradient)?;
                }
                Op::Relu => {
                    let input = node.inputs[0];
                    let contribution = self.relu_backward(input, gradient)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::Reshape { .. } => {
                    let input = node.inputs[0];
                    let original = self.shape_of(input)?.to_vec();
                    let contribution = self.reshape(gradient, original)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::Transpose { first, second } => {
                    let input = node.inputs[0];
                    let contribution = self.transpose(gradient, first, second)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::SumAll => {
                    let input = node.inputs[0];
                    let shape = self.shape_of(input)?.to_vec();
                    let contribution = self.broadcast_to(gradient, shape)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::MeanAll => {
                    let input = node.inputs[0];
                    let shape = self.shape_of(input)?.to_vec();
                    let count = numel(&shape).max(1) as f32;
                    let broadcast = self.broadcast_to(gradient, shape)?;
                    let contribution = self.scale(broadcast, 1.0 / count)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::SumToShape { .. } => {
                    let input = node.inputs[0];
                    let shape = self.shape_of(input)?.to_vec();
                    let contribution = self.broadcast_to(gradient, shape)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::Neg => {
                    let input = node.inputs[0];
                    let contribution = self.neg(gradient)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::BroadcastTo { .. } => {
                    let input = node.inputs[0];
                    let contribution = self.sum_to_shape(gradient, Vec::<usize>::new())?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::Scale { factor } => {
                    let input = node.inputs[0];
                    let contribution = self.scale(gradient, factor)?;
                    self.accumulate(&mut pending[input.0], contribution)?;
                }
                Op::ReluBackward => {
                    // d/d(upstream) = mask(input)；ReLU 二阶导几乎处处为 0，不再回传 orig input。
                    let (input, upstream) = (node.inputs[0], node.inputs[1]);
                    let contribution = self.relu_backward(input, gradient)?;
                    self.accumulate(&mut pending[upstream.0], contribution)?;
                }
                Op::SoftmaxCrossEntropy => {
                    // 目标是一次性标签输入，只把梯度回传给 logits。
                    let (logits, targets) = (node.inputs[0], node.inputs[1]);
                    let shape = self.shape_of(logits)?.to_vec();
                    let contribution = self.push(Op::SoftmaxCrossEntropyBackward, vec![logits, targets], shape);
                    self.accumulate(&mut pending[logits.0], contribution)?;
                }
                Op::SoftmaxCrossEntropyBackward => {}
            }
        }

        let mut gradients = Gradients::default();
        for (index, node) in self.nodes.iter().enumerate().take(count) {
            let Some(gradient) = pending[index]
            else {
                continue;
            };
            match &node.op {
                Op::Parameter { name } => gradients.parameters.push((name.clone(), gradient)),
                Op::Input { name } => gradients.inputs.push((name.clone(), gradient)),
                _ => {}
            }
        }
        Ok(gradients)
    }

    fn accumulate(&mut self, slot: &mut Option<NodeId>, contribution: NodeId) -> Result<()> {
        *slot = Some(match *slot {
            Some(existing) => self.add(existing, contribution)?,
            None => contribution,
        });
        Ok(())
    }

    /// 结束构建；输出节点必须存在。
    pub fn finish(self, outputs: Vec<NodeId>) -> Result<Graph> {
        for id in &outputs {
            if id.0 >= self.nodes.len() {
                return Err(DlError::UnknownNode { id: id.0 });
            }
        }
        let graph = Graph { nodes: self.nodes, outputs };
        graph.validate()?;
        Ok(graph)
    }

    fn binary(&mut self, op: &'static str, lhs: NodeId, rhs: NodeId, kind: Op) -> Result<NodeId> {
        let shape = broadcast_shape(self.shape_of(lhs)?, self.shape_of(rhs)?, op)?;
        Ok(self.push(kind, vec![lhs, rhs], shape))
    }

    fn shape_of(&self, id: NodeId) -> Result<&[usize]> {
        self.nodes.get(id.0).map(|node| node.shape.as_slice()).ok_or(DlError::UnknownNode { id: id.0 })
    }

    fn push(&mut self, op: Op, inputs: Vec<NodeId>, shape: Vec<usize>) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node { op, inputs, shape });
        id
    }
}

/// capture 闭包中的图句柄；只引用节点，不持有数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TNode(NodeId);

impl TNode {
    /// 返回底层节点下标。
    pub fn id(self) -> NodeId {
        self.0
    }
}

/// capture 期建图上下文；算子与 [`GraphBuilder`] 共用同一套形状推导。
///
/// 与 eager 路径不同，这里只记录节点。闭包结束后 [`Trace::finish`] 产出可执行
/// [`Graph`]，与 `dl-ops` 对同一组输入必须给出相同结果。
#[derive(Clone, Debug, Default)]
pub struct Trace {
    builder: GraphBuilder,
}

impl Trace {
    /// 空 trace。
    pub fn new() -> Self {
        Self::default()
    }

    /// 声明一个运行期输入。
    pub fn input(&mut self, name: impl Into<String>, shape: impl Into<Vec<usize>>) -> Result<TNode> {
        self.builder.input(name, shape).map(TNode)
    }

    /// 声明一个运行期参数。
    pub fn parameter(&mut self, name: impl Into<String>, shape: impl Into<Vec<usize>>) -> Result<TNode> {
        self.builder.parameter(name, shape).map(TNode)
    }

    /// 逐元素相加，支持广播。
    pub fn add(&mut self, lhs: TNode, rhs: TNode) -> Result<TNode> {
        self.builder.add(lhs.0, rhs.0).map(TNode)
    }

    /// 逐元素相减，支持广播。
    pub fn sub(&mut self, lhs: TNode, rhs: TNode) -> Result<TNode> {
        self.builder.sub(lhs.0, rhs.0).map(TNode)
    }

    /// 逐元素相乘，支持广播。
    pub fn mul(&mut self, lhs: TNode, rhs: TNode) -> Result<TNode> {
        self.builder.mul(lhs.0, rhs.0).map(TNode)
    }

    /// 二维矩阵乘。
    pub fn matmul(&mut self, lhs: TNode, rhs: TNode) -> Result<TNode> {
        self.builder.matmul(lhs.0, rhs.0).map(TNode)
    }

    /// ReLU 激活。
    pub fn relu(&mut self, input: TNode) -> Result<TNode> {
        self.builder.relu(input.0).map(TNode)
    }

    /// 多分类 softmax 交叉熵。
    pub fn softmax_cross_entropy(&mut self, logits: TNode, targets: TNode) -> Result<TNode> {
        self.builder.softmax_cross_entropy(logits.0, targets.0).map(TNode)
    }

    /// 改变形状。
    pub fn reshape(&mut self, input: TNode, shape: impl Into<Vec<usize>>) -> Result<TNode> {
        self.builder.reshape(input.0, shape).map(TNode)
    }

    /// 交换两个轴。
    pub fn transpose(&mut self, input: TNode, first: usize, second: usize) -> Result<TNode> {
        self.builder.transpose(input.0, first, second).map(TNode)
    }

    /// 全部元素求和。
    pub fn sum_all(&mut self, input: TNode) -> Result<TNode> {
        self.builder.sum_all(input.0).map(TNode)
    }

    /// 全部元素求平均。
    pub fn mean_all(&mut self, input: TNode) -> Result<TNode> {
        self.builder.mean_all(input.0).map(TNode)
    }

    /// 结束捕获；输出节点必须存在。
    pub fn finish(self, outputs: Vec<TNode>) -> Result<Graph> {
        self.builder.finish(outputs.into_iter().map(|node| node.0).collect())
    }
}

/// 在闭包中捕获一张前向图。
///
/// ```
/// use dl_graph::{Bindings, capture};
/// use dl_tensor::Tensor;
///
/// let graph = capture(|trace| {
///     let x = trace.input("x", [2, 2])?;
///     let w = trace.parameter("w", [2, 2])?;
///     let h = trace.matmul(x, w)?;
///     let h = trace.relu(h)?;
///     Ok(vec![h])
/// })
/// .unwrap();
///
/// let mut bindings = Bindings::new();
/// bindings.insert_input("x", Tensor::zeros([2, 2]).unwrap());
/// bindings.insert_parameter("w", Tensor::zeros([2, 2]).unwrap());
/// assert_eq!(graph.run(&bindings).unwrap().len(), 1);
/// ```
pub fn capture<F>(body: F) -> Result<Graph>
where
    F: FnOnce(&mut Trace) -> Result<Vec<TNode>>,
{
    let mut trace = Trace::new();
    let outputs = body(&mut trace)?;
    trace.finish(outputs)
}

/// 可执行前向图。
#[derive(Clone, Debug)]
pub struct Graph {
    nodes: Vec<Node>,
    outputs: Vec<NodeId>,
}

/// 训练图：前向图加上反向节点。
///
/// [`Graph::training`] 从任意既有图（包括 `capture` 出来的图）复制出训练图，输出顺序为
/// 损失在前、随后是各参数与输入的梯度，可直接被 `dl-plan` 放置、被 `dl-runtime` 流式执行。
#[derive(Clone, Debug)]
pub struct TrainingGraph {
    /// 含反向节点的图；输出是损失与梯度。
    pub graph: Graph,
    /// 梯度节点索引。
    pub gradients: Gradients,
    /// 损失节点。
    pub loss: NodeId,
}

impl Graph {
    /// 返回节点总数。
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// 以本图的标量输出为损失，复制出带反向节点的训练图。
    pub fn training(&self, output: NodeId) -> Result<TrainingGraph> {
        let mut builder = GraphBuilder::from_graph(self);
        let gradients = builder.backward(output)?;
        let mut outputs = vec![output];
        outputs.extend(gradients.nodes());
        let graph = builder.finish(outputs)?;
        Ok(TrainingGraph { graph, gradients, loss: output })
    }

    /// 返回是否为空图。
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 返回输出节点。
    pub fn outputs(&self) -> &[NodeId] {
        &self.outputs
    }

    /// 返回节点形状。
    pub fn shape(&self, id: NodeId) -> Option<&[usize]> {
        self.nodes.get(id.0).map(|node| node.shape.as_slice())
    }

    /// 返回节点算子。
    pub fn op(&self, id: NodeId) -> Option<&Op> {
        self.nodes.get(id.0).map(|node| &node.op)
    }

    /// 是否为不参与执行的叶子节点（输入、参数、常量），放置计划不必为其分配后端。
    pub fn is_binding(&self, id: NodeId) -> bool {
        matches!(self.op(id), Some(Op::Input { .. } | Op::Parameter { .. } | Op::Constant { .. }))
    }

    /// 返回节点的输入节点；未知节点返回 `None`。
    pub fn node_inputs(&self, id: NodeId) -> Option<&[NodeId]> {
        self.nodes.get(id.0).map(|node| node.inputs.as_slice())
    }

    /// 按声明顺序遍历运行期输入。
    pub fn inputs(&self) -> impl Iterator<Item = (&str, &[usize])> {
        self.nodes.iter().filter_map(|node| match &node.op {
            Op::Input { name } => Some((name.as_str(), node.shape.as_slice())),
            _ => None,
        })
    }

    /// 按声明顺序遍历运行期参数。
    pub fn parameters(&self) -> impl Iterator<Item = (&str, &[usize])> {
        self.nodes.iter().filter_map(|node| match &node.op {
            Op::Parameter { name } => Some((name.as_str(), node.shape.as_slice())),
            _ => None,
        })
    }

    /// 校验绑定名唯一且节点引用只指向更早的节点。
    pub fn validate(&self) -> Result<()> {
        let mut inputs = BTreeMap::new();
        let mut parameters = BTreeMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            for input in &node.inputs {
                if input.0 >= index {
                    return Err(DlError::UnknownNode { id: input.0 });
                }
            }
            match &node.op {
                Op::Input { name } => {
                    if inputs.insert(name.as_str(), index).is_some() {
                        return Err(DlError::Format { detail: format!("duplicate input {name:?}") });
                    }
                }
                Op::Parameter { name } => {
                    if parameters.insert(name.as_str(), index).is_some() {
                        return Err(DlError::Format { detail: format!("duplicate parameter {name:?}") });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 按绑定执行前向，返回输出张量。
    pub fn run(&self, bindings: &Bindings) -> Result<Vec<Tensor>> {
        let mut interpreter = self.interpreter(bindings)?;
        while interpreter.step()?.is_some() {}
        interpreter.outputs()
    }

    /// 创建逐节点执行器；用于按放置计划分步执行。
    pub fn interpreter<'a>(&'a self, bindings: &'a Bindings) -> Result<Interpreter<'a>> {
        self.validate()?;
        Ok(Interpreter { graph: self, bindings, values: Vec::with_capacity(self.nodes.len()), cursor: 0 })
    }
}

/// 逐节点执行器；每次 [`Interpreter::step`] 只计算下一个节点。
///
/// 执行顺序固定为节点创建顺序，与 [`Graph::run`] 共用同一 `dl-ops` 内核，因此分步结果
/// 必须与整图执行一致。
pub struct Interpreter<'a> {
    graph: &'a Graph,
    bindings: &'a Bindings,
    values: Vec<Option<Tensor>>,
    cursor: usize,
}

impl<'a> Interpreter<'a> {
    /// 是否还有未执行节点。
    pub fn is_finished(&self) -> bool {
        self.cursor >= self.graph.nodes.len()
    }

    /// 下一个待执行节点。
    pub fn next_node(&self) -> Option<NodeId> {
        if self.is_finished() { None } else { Some(NodeId(self.cursor)) }
    }

    /// 执行下一个节点并返回其值；已执行完时返回 `None`。
    pub fn step(&mut self) -> Result<Option<Tensor>> {
        let Some(node) = self.graph.nodes.get(self.cursor)
        else {
            return Ok(None);
        };
        let value = eval(node, &self.values, self.bindings)?;
        debug_assert_eq!(value.shape(), node.shape.as_slice(), "inferred shape must match execution");
        self.values.push(Some(value.clone()));
        self.cursor += 1;
        Ok(Some(value))
    }

    /// 已执行节点的值。
    pub fn value(&self, id: NodeId) -> Option<&Tensor> {
        self.values.get(id.0).and_then(Option::as_ref)
    }

    /// 收集图输出；未执行完时返回未知节点错误。
    pub fn outputs(&self) -> Result<Vec<Tensor>> {
        self.graph.outputs.iter().map(|id| self.value(*id).cloned().ok_or(DlError::UnknownNode { id: id.0 })).collect()
    }
}

fn eval(node: &Node, values: &[Option<Tensor>], bindings: &Bindings) -> Result<Tensor> {
    match &node.op {
        Op::Input { name } => binding(bindings.input(name), "input", name, &node.shape),
        Op::Parameter { name } => binding(bindings.parameter(name), "parameter", name, &node.shape),
        Op::Add => ops::add(parent(values, node, 0)?, parent(values, node, 1)?),
        Op::Sub => ops::sub(parent(values, node, 0)?, parent(values, node, 1)?),
        Op::Mul => ops::mul(parent(values, node, 0)?, parent(values, node, 1)?),
        Op::MatMul => ops::matmul(parent(values, node, 0)?, parent(values, node, 1)?),
        Op::Relu => ops::relu(parent(values, node, 0)?),
        Op::Reshape { shape } => parent(values, node, 0)?.reshape(shape.clone()),
        Op::Transpose { first, second } => parent(values, node, 0)?.transpose(*first, *second),
        Op::SumAll => ops::sum_all(parent(values, node, 0)?),
        Op::MeanAll => ops::mean_all(parent(values, node, 0)?),
        Op::Constant { value } => Tensor::full(Vec::<usize>::new(), *value),
        Op::SumToShape { shape } => ops::sum_to_shape(parent(values, node, 0)?, shape),
        Op::Neg => ops::neg(parent(values, node, 0)?),
        Op::BroadcastTo { shape } => {
            let scalar = parent(values, node, 0)?.as_slice()?[0];
            Tensor::full(shape.clone(), scalar)
        }
        Op::Scale { factor } => ops::mul_scalar(parent(values, node, 0)?, *factor),
        Op::ReluBackward => {
            let input = parent(values, node, 0)?;
            let gradient = parent(values, node, 1)?;
            let mask = ops::map(input, |value| if value > 0.0 { 1.0 } else { 0.0 })?;
            ops::mul(gradient, &mask)
        }
        Op::SoftmaxCrossEntropy => ops::softmax_cross_entropy(parent(values, node, 0)?, parent(values, node, 1)?),
        Op::SoftmaxCrossEntropyBackward => ops::softmax_cross_entropy_backward(parent(values, node, 0)?, parent(values, node, 1)?),
    }
}

fn parent<'a>(values: &'a [Option<Tensor>], node: &Node, index: usize) -> Result<&'a Tensor> {
    let id = node.inputs[index].0;
    values.get(id).and_then(Option::as_ref).ok_or(DlError::UnknownNode { id })
}

fn binding(slot: Option<&Tensor>, kind: &'static str, name: &str, expected: &[usize]) -> Result<Tensor> {
    let tensor = slot.ok_or_else(|| DlError::MissingBinding { kind, name: name.to_owned() })?;
    if tensor.shape() != expected {
        return Err(DlError::BindingShape { kind, name: name.to_owned(), expected: expected.to_vec(), actual: tensor.shape().to_vec() });
    }
    Ok(tensor.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_broadcast_add() {
        let mut builder = GraphBuilder::new();
        let matrix = builder.input("x", [2, 2]).unwrap();
        let bias = builder.parameter("b", [2]).unwrap();
        let sum = builder.add(matrix, bias).unwrap();
        let graph = builder.finish(vec![sum]).unwrap();
        assert_eq!(graph.shape(sum), Some([2, 2].as_slice()));

        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0], [2, 2]).unwrap());
        bindings.insert_parameter("b", Tensor::from_vec(vec![10.0, 20.0], [2]).unwrap());
        let output = graph.run(&bindings).unwrap();
        assert_eq!(output[0].to_vec(), vec![11.0, 22.0, 13.0, 24.0]);
    }

    #[test]
    fn rejects_incompatible_broadcast() {
        let mut builder = GraphBuilder::new();
        let left = builder.input("a", [2, 3]).unwrap();
        let right = builder.input("b", [4]).unwrap();
        assert!(matches!(builder.add(left, right), Err(DlError::ShapeMismatch { .. })));
    }

    #[test]
    fn rejects_bad_matmul_shape() {
        let mut builder = GraphBuilder::new();
        let left = builder.input("a", [2, 3]).unwrap();
        let right = builder.input("b", [4, 2]).unwrap();
        assert!(matches!(builder.matmul(left, right), Err(DlError::ShapeMismatch { .. })));
    }

    #[test]
    fn reports_missing_binding_and_shape() {
        let mut builder = GraphBuilder::new();
        let input = builder.input("x", [2]).unwrap();
        let graph = builder.finish(vec![input]).unwrap();

        assert!(matches!(graph.run(&Bindings::new()), Err(DlError::MissingBinding { .. })));

        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::zeros([3]).unwrap());
        assert!(matches!(graph.run(&bindings), Err(DlError::BindingShape { .. })));
    }

    #[test]
    fn rejects_duplicate_names() {
        let mut builder = GraphBuilder::new();
        builder.input("x", [2]).unwrap();
        builder.input("x", [2]).unwrap();
        assert!(matches!(builder.finish(Vec::new()), Err(DlError::Format { .. })));
    }

    #[test]
    fn training_graph_backpropagates_softmax_cross_entropy() {
        let mut builder = GraphBuilder::new();
        let x = builder.input("x", [2, 2]).unwrap();
        let target = builder.input("y", [2, 2]).unwrap();
        let weight = builder.parameter("w", [2, 2]).unwrap();
        let logits = builder.matmul(x, weight).unwrap();
        let loss = builder.softmax_cross_entropy(logits, target).unwrap();
        let graph = builder.finish(vec![loss]).unwrap();
        let training = graph.training(loss).unwrap();

        let inputs = Tensor::from_vec(vec![1.0, 0.0, 0.0, 1.0], [2, 2]).unwrap();
        let targets = Tensor::from_vec(vec![1.0, 0.0, 0.0, 1.0], [2, 2]).unwrap();
        let weights = Tensor::from_vec(vec![0.2, -0.1, 0.4, 0.3], [2, 2]).unwrap();
        let mut bindings = Bindings::new();
        bindings.insert_input("x", inputs.clone());
        bindings.insert_input("y", targets.clone());
        bindings.insert_parameter("w", weights.clone());

        let values = training.graph.run(&bindings).unwrap();
        let loss_value = values[0].as_slice().unwrap()[0];
        assert!(loss_value.is_finite() && loss_value > 0.0);

        let logits = ops::matmul(&inputs, &weights).unwrap();
        let logits_gradient = ops::softmax_cross_entropy_backward(&logits, &targets).unwrap();
        let expected = ops::matmul(&inputs.transpose(0, 1).unwrap(), &logits_gradient).unwrap();
        let (_, gradient_node) = training.gradients.parameters.first().expect("weight gradient");
        let index = training.graph.outputs().iter().position(|node| node == gradient_node).expect("gradient output");
        for (actual, expected) in values[index].to_vec().iter().zip(expected.to_vec()) {
            assert!((actual - expected).abs() < 1e-5, "gradient mismatch: {actual} vs {expected}");
        }
    }

    #[test]
    fn captured_graph_matches_eager_ops() {
        let graph = capture(|trace| {
            let x = trace.input("x", [2, 3])?;
            let w = trace.parameter("w", [3, 2])?;
            let product = trace.matmul(x, w)?;
            let activated = trace.relu(product)?;
            Ok(vec![activated])
        })
        .expect("capture");

        let x = Tensor::from_vec(vec![1.0, -2.0, 3.0, 4.0, 5.0, -6.0], [2, 3]).unwrap();
        let w = Tensor::from_vec(vec![1.0, 2.0, -1.0, 0.0, 0.5, -0.5], [3, 2]).unwrap();
        let mut bindings = Bindings::new();
        bindings.insert_input("x", x.clone());
        bindings.insert_parameter("w", w.clone());

        let captured = graph.run(&bindings).expect("run captured graph");
        let eager = ops::relu(&ops::matmul(&x, &w).expect("eager matmul")).expect("eager relu");
        assert_eq!(captured[0].to_vec(), eager.to_vec());
        assert_eq!(captured[0].shape(), eager.shape());
    }

    #[test]
    fn capture_reports_shape_errors_immediately() {
        let error = capture(|trace| {
            let x = trace.input("x", [2, 3])?;
            let w = trace.parameter("w", [4, 2])?;
            let product = trace.matmul(x, w)?;
            Ok(vec![product])
        })
        .expect_err("incompatible matmul");
        assert!(matches!(error, DlError::ShapeMismatch { op: "matmul", .. }));
    }

    fn mlp_with_backward() -> (Graph, Gradients) {
        let mut builder = GraphBuilder::new();
        let x = builder.input("x", [2, 2]).unwrap();
        let w = builder.parameter("w", [2, 2]).unwrap();
        let b = builder.parameter("b", [2]).unwrap();
        let product = builder.matmul(x, w).unwrap();
        let biased = builder.add(product, b).unwrap();
        let loss = builder.mean_all(biased).unwrap();
        let gradients = builder.backward(loss).unwrap();
        let mut outputs = vec![loss];
        outputs.extend(gradients.nodes());
        (builder.finish(outputs).unwrap(), gradients)
    }

    #[test]
    fn backward_matches_closed_form_and_folds_broadcast() {
        let (graph, gradients) = mlp_with_backward();
        assert_eq!(gradients.parameters.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), vec!["w", "b"]);
        assert_eq!(gradients.inputs.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), vec!["x"]);

        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0], [2, 2]).unwrap());
        bindings.insert_parameter("w", Tensor::from_vec(vec![0.5, -0.5, 1.0, 2.0], [2, 2]).unwrap());
        bindings.insert_parameter("b", Tensor::zeros([2]).unwrap());

        let values = graph.run(&bindings).unwrap();
        let value_of = |node: NodeId| values[graph.outputs().iter().position(|id| *id == node).unwrap()].clone();

        assert_eq!(value_of(graph.outputs()[0]).to_vec(), vec![4.5]);
        // dL/dproduct = 1/4
        assert_eq!(value_of(gradients.parameter("w").unwrap()).to_vec(), vec![1.0, 1.0, 1.5, 1.5]);
        assert_eq!(value_of(gradients.parameter("b").unwrap()).to_vec(), vec![0.5, 0.5]);
        assert_eq!(value_of(gradients.input("x").unwrap()).to_vec(), vec![0.0, 0.75, 0.0, 0.75]);
    }

    #[test]
    fn backward_accumulates_repeated_uses_and_masks_relu() {
        let mut builder = GraphBuilder::new();
        let x = builder.input("x", [2]).unwrap();
        let squared = builder.mul(x, x).unwrap();
        let positive = builder.relu(x).unwrap();
        let combined = builder.add(squared, positive).unwrap();
        let loss = builder.sum_all(combined).unwrap();
        let gradients = builder.backward(loss).unwrap();
        let mut outputs = vec![loss];
        outputs.extend(gradients.nodes());
        let graph = builder.finish(outputs).unwrap();

        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![3.0, -4.0], [2]).unwrap());
        let values = graph.run(&bindings).unwrap();
        // d/dx (x^2 + relu(x)) = 2x + mask(x)；x 被两处使用，梯度必须累加。
        assert_eq!(values[1].to_vec(), vec![7.0, -8.0]);
    }

    #[test]
    fn backward_requires_a_scalar_loss_or_matching_seed() {
        let mut builder = GraphBuilder::new();
        let x = builder.input("x", [2]).unwrap();
        let vector_loss = builder.mul(x, x).unwrap();
        assert!(matches!(builder.backward(vector_loss), Err(DlError::ShapeMismatch { op: "backward", .. })));

        let seed = builder.input("seed", [1]).unwrap();
        assert!(matches!(builder.backward_with_seed(vector_loss, seed), Err(DlError::ShapeMismatch { op: "backward_seed", .. })));
    }

    #[test]
    fn backward_nodes_are_placeable_like_forward_nodes() {
        let (graph, gradients) = mlp_with_backward();
        assert!(graph.is_binding(NodeId(0)));
        assert!(!graph.is_binding(gradients.parameter("w").unwrap()));
        for node in gradients.nodes() {
            assert!(graph.shape(node).is_some(), "gradient node {} must carry a shape", node.0);
        }
    }

    #[test]
    fn captured_graph_can_be_turned_into_a_training_graph() {
        let forward = capture(|trace| {
            let x = trace.input("x", [2])?;
            let w = trace.parameter("w", [2])?;
            let product = trace.mul(x, w)?;
            let loss = trace.sum_all(product)?;
            Ok(vec![loss])
        })
        .expect("capture");
        let training = forward.training(forward.outputs()[0]).expect("training graph");
        assert_eq!(training.graph.outputs().len(), 3, "loss plus parameter and input gradients");

        let mut bindings = Bindings::new();
        bindings.insert_input("x", Tensor::from_vec(vec![2.0, -3.0], [2]).unwrap());
        bindings.insert_parameter("w", Tensor::from_vec(vec![5.0, 7.0], [2]).unwrap());
        let values = training.graph.run(&bindings).unwrap();
        assert_eq!(values[0].to_vec(), vec![-11.0]);
        assert_eq!(values[1].to_vec(), vec![2.0, -3.0], "d sum(x*w)/dw = x");
    }
}
