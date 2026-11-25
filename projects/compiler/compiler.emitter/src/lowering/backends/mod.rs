// 仅 ovm lane（feature 门）经 `super::` 消费这两个 re-export；wasm lane 已改用
// 目标私有结算词汇，默认构建下不再引用。
#[cfg(feature = "ovm-lane")]
pub(crate) use crate::lowering::{sanitize_symbol, shared::executable};

pub(crate) mod argv_encoding;

#[cfg(feature = "ovm-lane")]
#[doc(hidden)]
pub mod ovm;
#[cfg(feature = "ovm-lane")]
#[path = "ovm/mir.rs"]
pub(crate) mod ovm_mir;

#[doc(hidden)]
pub mod wasm;

#[doc(hidden)]
pub mod clr;
#[doc(hidden)]
pub mod jvm;
#[doc(hidden)]
pub mod machine;
