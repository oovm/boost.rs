#![warn(missing_docs)]

//! XPath subset DSL for Oak document queries.
//!
//! Parsing and AST ownership live here. Execution against XML trees is provided
//! by `oak-xml` using namespace-aware `ElementView` adapters.

mod ast;
mod diag;
mod parser;

pub use ast::{Axis, NodeTest, PathExpr, Predicate, QName, Step, XpathExpr};
pub use diag::XpathParseError;
pub use parser::parse_xpath;
