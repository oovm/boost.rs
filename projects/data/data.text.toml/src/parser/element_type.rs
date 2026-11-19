use crate::lexer::token_type::TomlTokenKind;
use data_text_core::{ElementType, UniversalElementRole};

/// Alias for `TomlTokenKind` to conform to the shared naming conventions for element types.
pub type TomlElementType = TomlTokenKind;

impl ElementType for TomlTokenKind {
    type Role = UniversalElementRole;

    fn role(&self) -> Self::Role {
        match self {
            Self::Eof => UniversalElementRole::None,
            _ => UniversalElementRole::Value,
        }
    }
}
