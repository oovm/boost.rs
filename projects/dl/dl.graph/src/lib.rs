//! 张量计算图 IR：构建期形状推导、校验与前向执行。
//!
//! 图只描述算子依赖和形状，不持有数据。运行期通过 [`Bindings`] 绑定输入与参数；图执行
//! 与 eager（`dl-autograd`）共用 `dl-ops` 内核，因此同一组权重必须给出相同结果。
//!
//! 本 crate 只做前向。反向、参数更新与 checkpoint 由 `dl-autograd`、`dl-train` 和
//! `dl-model` 负责；可视化投影不是执行证明。

#![warn(missing_docs)]

use std::collections::BTreeMap;

use dl_ops as ops;
use dl_tensor::Tensor;
use dl_types::{DlError, Result, broadcast_shape, numel, require_non_zero};

/// 图节点下标；按创建顺序递增，输入节点总是先于消费者出现。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub usize);

/// 节点算子。
#[derive(Clone, Debug, PartialEq, Eq)]
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

impl Graph {
    /// 返回节点总数。
    pub fn node_count(&self) -> usize {
        self.nodes.len()
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
        let mut values: Vec<Option<Tensor>> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let value = eval(node, &values, bindings)?;
            debug_assert_eq!(value.shape(), node.shape.as_slice(), "inferred shape must match execution");
            values.push(Some(value));
        }
        self.outputs.iter().map(|id| values.get(id.0).and_then(Option::as_ref).cloned().ok_or(DlError::UnknownNode { id: id.0 })).collect()
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
}
