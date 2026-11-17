//! 构造与形状校验的失败分层。

use core::fmt::{self, Display, Formatter};

/// 规则多维集合的失败原因。
///
/// 三类错误对应调用方不同的处置：[`ShapeOverflow`](Self::ShapeOverflow) 表示
/// 各 extent 的乘积在本机 `usize` 下不可表示，属于形状参数问题；
/// [`ElementCountMismatch`](Self::ElementCountMismatch) 表示已有一维缓冲的元素数
/// 与形状不符，属于数据与声明不一致；[`Allocation`](Self::Allocation) 表示连续
/// 存储无法分配，属于宿主资源故障。错误不携带元素内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CollectionError {
    /// 各 extent 的乘积超出 `usize`，无法表示逻辑元素数。
    ShapeOverflow,
    /// 已有一维缓冲的元素数量与形状要求的数量不一致。
    ElementCountMismatch {
        /// 形状要求的逻辑元素数。
        shape: usize,
        /// 实际提供的元素数量。
        elements: usize,
    },
    /// 连续存储分配失败：容量超出可分配范围或宿主内存不足。
    Allocation,
}

impl Display for CollectionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShapeOverflow => f.write_str("the extents multiply beyond the addressable element count"),
            Self::ElementCountMismatch { shape, elements } => {
                write!(f, "the shape requires {shape} elements but {elements} were provided")
            }
            Self::Allocation => f.write_str("the contiguous element storage could not be allocated"),
        }
    }
}

impl std::error::Error for CollectionError {}

/// 本 crate 的统一结果别名。
pub type Result<T> = core::result::Result<T, CollectionError>;
