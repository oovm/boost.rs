use crate::{
    ast::{Program, PythonRoot},
    language::PythonLanguage,
};
use data_text_core::{GreenNode, SourceText, TextError};

impl<'config> super::PythonBuilder<'config> {
    /// Builds a Python root from a green tree.
    pub fn build_root(&self, green_tree: &GreenNode<PythonLanguage>, source: &SourceText) -> Result<PythonRoot, TextError> {
        let statements = self.build_statement_list(green_tree.children(), 0, source)?;
        Ok(PythonRoot { program: Program { statements }, span: (0..green_tree.text_len() as usize).into() })
    }
}
