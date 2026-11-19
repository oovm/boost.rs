#![doc = include_str!("readme.md")]

use core::range::Range;

/// JavaScript root node.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JavaScriptRoot {
    /// The span of the root node.
    #[cfg_attr(feature = "serde", serde(with = "data_text_core::serde_range"))]
    pub span: Range<usize>,
}
