//! VOS schema input chain of the Boost `schema.*` tree.
//!
//! the grammar crate owns VOS lexing, parsing, CST and syntax projection (`data-text-vos`). This
//! crate owns name resolution, canonical types, durable identity, the resolved
//! contract and structured diagnostics. Foundational models are exposed through
//! [`types`].
//!
//! Hosts such as YYDB, YYDS and Iris must call [`parse_schema`] and consume the
//! resolved contract APIs below. This crate never owns a parser, an AST, a CST
//! or a lexical layer of its own.

#![warn(missing_docs)]

pub use schema_types as types;

/// syntax surface consumed by VOS hosts.
pub use data_text_vos as syntax;

pub mod registry;
pub mod vfs;

mod semantic;
pub use semantic::*;

/// Parses a VOS expression program (statements, `micro` declarations and an
/// optional trailing result expression) through the `data.text` parser.
///
/// This is the structured program projection consumed by hosts such as Iris to
/// lower query and update bodies. It performs no name resolution or type
/// checking.
pub fn parse_program(source: &str) -> Result<syntax::VosProgram, String> {
    data_text_vos::parse_program(source)
}

/// Parses and validates a schema through `data.text` and the VOS schema contract.
pub fn validate_schema(source: &str) -> Result<schema_types::SchemaProjection, String> {
    let input = parse_schema(source)?;
    let projection = input.project_schema().map_err(|diagnostics| format!("VOS semantic projection failed: {diagnostics:?}"))?;
    for item in &projection.types {
        if item.kind == schema_types::TypeContractKind::Table {
            let primary_count =
                item.fields.iter().flat_map(|field| field.attributes.iter()).filter(|attribute| attribute.name == "primary").count();
            if primary_count != 1 {
                return Err(format!("table `{}` requires exactly one primary field", item.canonical_path.join("::")));
            }
        }
    }
    Ok(projection)
}

pub mod uuid;
pub use uuid::{is_v7 as uuid_is_v7, uuid};
