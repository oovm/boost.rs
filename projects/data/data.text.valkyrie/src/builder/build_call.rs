use crate::{
    ValkyrieLanguage,
    ast::*,
    builder::{ValkyrieBuilder, utils},
    lexer::token_type::ValkyrieTokenType,
};
use data_text_core::{RedNode, Source, TextError};

impl<'config> ValkyrieBuilder<'config> {
    pub(crate) fn build_call<S: Source + ?Sized>(&self, node: RedNode<ValkyrieLanguage>, source: &S) -> Result<TermExpression, TextError> {
        let span = node.span();
        let (callee, args) = utils::build_call_expr(&node, source, |n, s| self.build_expr(*n, s))?;

        Ok(TermExpression::ApplyCall { callee, args, span })
    }
}
