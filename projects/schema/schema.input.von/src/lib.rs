//! VON (Virtual Object Notation) 值/文档输入。
//!
//! 表示 null、布尔、有符号 64 位整数、字符串、数组与字符串键对象。该格式
//! 不带运行时、场景或数据库 schema 语义：文档含义由消费者拥有。解析成功
//! 只说明输入是合法 VON，不说明满足任何 schema。
#[cfg(feature = "serde")]
mod deserializer;
#[cfg(feature = "serde")]
mod error;
#[cfg(feature = "serde")]
mod format;
#[cfg(feature = "serde")]
mod serializer;

pub use schema_input_von_ast::VonValue;
pub use schema_input_von_parser::{
    Lexer, Token, TokenKind, VonCstElement, VonCstParser, VonCstRoot, VonParseError, VonParser,
};

#[cfg(feature = "serde")]
pub use deserializer::{VonDeserializer, from_str, from_value};
#[cfg(feature = "serde")]
pub use error::{VonError, VonSerdeError};
#[cfg(feature = "serde")]
pub use format::{to_string, to_string_indented};
#[cfg(feature = "serde")]
pub use serializer::{VonSerializer, to_value};
