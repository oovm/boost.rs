pub(crate) mod argv_encoding;

/// OVM 目标私有编码服务（feature 门）；lane 结算已切到语言侧 `pipeline/ovm/`。
#[cfg(feature = "ovm-lane")]
pub mod ovm;

#[doc(hidden)]
pub mod wasm;

#[doc(hidden)]
pub mod clr;
#[doc(hidden)]
pub mod jvm;
#[doc(hidden)]
pub mod machine;
