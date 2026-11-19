use crate::parser::VampireElementType;
use data_text_core::tree::TypedNode;

/// Vampire root node.
#[derive(Debug, Clone)]
pub struct VampireRoot {
    /// The source span covered by this root node.
    pub span: data_text_core::Range<usize>,
    /// The collection of formulas in the Vampire document.
    pub formulas: Vec<()>,
}
