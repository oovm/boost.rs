//! `OrderedF64` 的线格式：沿用 `f64`，不额外包一层对象。

use boost_math::numeric::OrderedF64;
use serde::{Deserialize, Deserializer, Serializer};

/// 以原始 `f64` 序列化，保持既有 HIR/MIR 常量形状。
pub(crate) fn serialize<S: Serializer>(value: &OrderedF64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(value.get())
}

/// 从原始 `f64` 反序列化。
pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<OrderedF64, D::Error> {
    f64::deserialize(deserializer).map(OrderedF64::new)
}
