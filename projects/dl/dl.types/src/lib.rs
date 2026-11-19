//! DL 基础合同：dtype、设备种类、形状推导与统一错误。
//!
//! 本 crate 是 `dl.*` 的最底层，只依赖标准库。张量存储、算子、自动微分和模型都必须复用
//! 这里的 [`DType`]、[`DeviceKind`]、[`DlError`] 与形状工具，不得各自复制一套。

#![deny(missing_docs)]

use core::fmt;

/// 张量元素类型。
///
/// D1 CPU 纵切只启用 `F32`；后续扩展必须在唯一实现上增加，不允许并行第二套张量。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DType {
    /// 32 位浮点。
    F32,
}

impl DType {
    /// 返回 safetensors 兼容的类型名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::F32 => "F32",
        }
    }

    /// 返回单个元素占用的字节数。
    pub const fn element_size(self) -> usize {
        match self {
            Self::F32 => 4,
        }
    }
}

impl fmt::Display for DType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// 张量所在设备种类。
///
/// D1 只实现 CPU；硬件后端接入后由 `hardware.*` 提供真实设备标识，这里的枚举只描述种类，
/// 不承载队列、缓冲区或生命周期。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    /// 主机 CPU。
    Cpu,
}

/// DL 统一错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DlError {
    /// 两个形状无法广播。
    ShapeMismatch {
        /// 触发错误的算子名。
        op: &'static str,
        /// 左操作数形状。
        lhs: Vec<usize>,
        /// 右操作数形状。
        rhs: Vec<usize>,
    },
    /// 元素数量与形状不一致。
    ElementCount {
        /// 期望数量。
        expected: usize,
        /// 实际数量。
        actual: usize,
    },
    /// 轴下标越界。
    InvalidAxis {
        /// 请求的轴。
        axis: usize,
        /// 张量阶数。
        rank: usize,
    },
    /// 需要连续布局的操作收到非连续视图。
    NotContiguous {
        /// 触发错误的算子名。
        op: &'static str,
    },
    /// 形状含 0 维，无法完成该操作。
    ZeroDimension {
        /// 触发错误的算子名。
        op: &'static str,
    },
    /// 反向传播得到非有限梯度。
    NonFiniteGradient,
    /// 随机源不可用。
    Random {
        /// 具体原因。
        detail: String,
    },
    /// 外部格式（checkpoint、模型文件）不合法。
    Format {
        /// 具体原因。
        detail: String,
    },
    /// 引用了不存在的图节点。
    UnknownNode {
        /// 请求的节点下标。
        id: usize,
    },
    /// 运行期缺少输入或参数绑定。
    MissingBinding {
        /// `input` 或 `parameter`。
        kind: &'static str,
        /// 绑定名。
        name: String,
    },
    /// 绑定张量的形状与图声明不一致。
    BindingShape {
        /// `input` 或 `parameter`。
        kind: &'static str,
        /// 绑定名。
        name: String,
        /// 图声明的形状。
        expected: Vec<usize>,
        /// 实际绑定形状。
        actual: Vec<usize>,
    },
}

impl fmt::Display for DlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShapeMismatch { op, lhs, rhs } => {
                write!(formatter, "{op}: cannot broadcast {lhs:?} with {rhs:?}")
            }
            Self::ElementCount { expected, actual } => {
                write!(formatter, "element count mismatch: expected {expected}, got {actual}")
            }
            Self::InvalidAxis { axis, rank } => {
                write!(formatter, "axis {axis} out of range for rank {rank}")
            }
            Self::NotContiguous { op } => write!(formatter, "{op}: tensor view is not contiguous"),
            Self::ZeroDimension { op } => write!(formatter, "{op}: zero-sized dimension is not supported"),
            Self::NonFiniteGradient => formatter.write_str("gradient contains NaN or infinity"),
            Self::Random { detail } => write!(formatter, "random source failed: {detail}"),
            Self::Format { detail } => write!(formatter, "invalid model format: {detail}"),
            Self::UnknownNode { id } => write!(formatter, "graph node {id} does not exist"),
            Self::MissingBinding { kind, name } => write!(formatter, "missing {kind} binding {name:?}"),
            Self::BindingShape { kind, name, expected, actual } => {
                write!(formatter, "{kind} {name:?} must have shape {expected:?}, got {actual:?}")
            }
        }
    }
}

impl std::error::Error for DlError {}

/// DL 统一结果类型。
pub type Result<T> = core::result::Result<T, DlError>;

/// 计算形状的元素总数；空形状是标量，总数为 1。
pub fn numel(shape: &[usize]) -> usize {
    shape.iter().copied().product()
}

/// 计算行主序连续布局的步长。
pub fn contiguous_strides(shape: &[usize]) -> Vec<usize> {
    let mut strides = vec![1; shape.len()];
    let mut accumulator = 1usize;
    for axis in (0..shape.len()).rev() {
        strides[axis] = accumulator;
        accumulator *= shape[axis];
    }
    strides
}

/// 推导两个形状按 NumPy 规则广播后的形状。
pub fn broadcast_shape(lhs: &[usize], rhs: &[usize], op: &'static str) -> Result<Vec<usize>> {
    let rank = lhs.len().max(rhs.len());
    let mut output = vec![1usize; rank];
    for offset in 0..rank {
        let left = aligned_dim(lhs, offset);
        let right = aligned_dim(rhs, offset);
        output[rank - 1 - offset] = match (left, right) {
            (left, right) if left == right => left,
            (1, right) => right,
            (left, 1) => left,
            _ => {
                return Err(DlError::ShapeMismatch { op, lhs: lhs.to_vec(), rhs: rhs.to_vec() });
            }
        };
    }
    Ok(output)
}

/// 取从右侧对齐的第 `offset` 维；缺省维按 1 处理。
pub fn aligned_dim(shape: &[usize], offset: usize) -> usize {
    let position = shape.len().checked_sub(1 + offset);
    match position {
        Some(index) => shape[index],
        None => 1,
    }
}

/// 校验形状不含 0 维。
pub fn require_non_zero(shape: &[usize], op: &'static str) -> Result<()> {
    if shape.contains(&0) {
        return Err(DlError::ZeroDimension { op });
    }
    Ok(())
}
