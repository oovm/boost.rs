//! `data.text` adapters and VOS semantic processing.

#![deny(missing_docs)]

use data_text_vos::{VosDeclaration, VosField, VosRoot, VosSyntaxNode};

mod contract;
mod identity;
mod projection;
mod provider;
pub mod resolve;

pub use contract::{
    OperationSemantics, ServiceContractDecl, ServiceMethodDecl, build_operation_contracts, build_service_contracts,
    parse_operation_semantics_from_attributes,
};
pub use identity::{
    bind_identity, compare_identity, evolve_identity, resolve_contract, resolve_contract_from_bound, resolve_contract_from_units,
};
pub use projection::{project_operations, project_operations_artifact, project_services, project_services_artifact};
pub use provider::{
    IDENTITY_PROVIDER_FILE_NAME, IDENTITY_PROVIDER_FORMAT_VERSION, IdentityManifestProvider, IdentityProviderState, ProvidedField,
    ProvidedType, RetiredSlot,
};
pub use resolve::{DeclKind, SourceDecls, SourceId, SymbolTable, collect_decls, extract_using_path_with_literals, resolve_using_tree};
pub use schema_types::*;

/// Parses VOS source through `data.text` and wraps the resulting root for semantic use.
pub fn parse_schema(source: &str) -> Result<ContractInput, String> {
    data_text_vos::parse(source).map(ContractInput::from_oak)
}

/// parser output accepted by the VOS semantic pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractInput {
    root: VosRoot,
}

impl ContractInput {
    /// Wraps an parser-built VOS root without reparsing its source.
    pub fn from_oak(root: VosRoot) -> Self {
        Self { root }
    }

    /// Returns the exact source owned by the parser.
    pub fn source(&self) -> &str {
        &self.root.source
    }

    /// Returns the lossless CST for semantic traversal.
    pub fn syntax(&self) -> &VosSyntaxNode {
        &self.root.syntax
    }

    /// Returns top-level declarations emitted by builder.
    pub fn declarations(&self) -> &[VosDeclaration] {
        &self.root.declarations
    }

    /// Returns syntax-projected fields without reparsing the source.
    pub fn fields<'a>(&self, declaration: &'a VosDeclaration) -> &'a [VosField] {
        &declaration.fields
    }

    /// Consumes the wrapper and returns the syntax root.
    pub fn into_oak(self) -> VosRoot {
        self.root
    }

    /// Projects the syntax root into the first VOS schema artifact.
    pub fn project_schema(&self) -> Result<SchemaProjection, Vec<ContractDiagnostic>> {
        projection::project_schema(&self.root)
    }

    /// Projects only namespace, table and class declarations through the schema contract.
    ///
    /// Single entry for hosts consuming a schema-only projection: operation-like
    /// and other non-schema declarations are tolerated and skipped before
    /// projection instead of failing the schema contract.
    pub fn project_schema_only(&self) -> Result<SchemaProjection, Vec<ContractDiagnostic>> {
        projection::project_schema_only(&self.root)
    }

    /// Projects operation declarations without reparsing their bodies.
    pub fn project_operations(&self) -> Result<Vec<OperationProjection>, Vec<ContractDiagnostic>> {
        projection::project_operations(&self.root)
    }

    /// Projects operation declarations into a versioned syntax artifact.
    pub fn project_operations_artifact(&self) -> Result<OperationProjectionArtifact, Vec<ContractDiagnostic>> {
        projection::project_operations_artifact(&self.root)
    }

    /// Projects language service declarations without lowering their bodies.
    pub fn project_services(&self) -> Result<Vec<ServiceProjection>, Vec<ContractDiagnostic>> {
        projection::project_services(&self.root)
    }

    /// Projects language service declarations into a versioned syntax artifact.
    pub fn project_services_artifact(&self) -> Result<ServiceProjectionArtifact, Vec<ContractDiagnostic>> {
        projection::project_services_artifact(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CANONICALIZATION_VERSION, CONTRACT_FORMAT_VERSION, CanonicalType, LANGUAGE_VERSION, OPERATION_CONTRACT_FORMAT_VERSION,
        OPERATION_PROJECTION_STAGE, OperationKind, OperationProjectionArtifact, PROJECTION_STAGE, SERVICE_CONTRACT_FORMAT_VERSION,
        SERVICE_PROJECTION_STAGE, ServiceProjectionArtifact, parse_schema,
    };
    use data_text_vos::VosDeclarationKind;

    #[test]
    fn consumes_text_root_without_reparsing() {
        let input = parse_schema("table User { @@id: uuid, email: utf8? = null, }").expect("VOS parses");

        assert_eq!(input.declarations().len(), 1);
        assert_eq!(input.declarations()[0].kind, VosDeclarationKind::Table);
        let fields = input.fields(&input.declarations()[0]);
        assert_eq!(fields[0].name, "id");
        assert_eq!(fields[0].attributes[0].text, "@@");
        assert_eq!(fields[0].type_syntax.text, "uuid");
        assert_eq!(fields[1].type_syntax.text, "utf8?");
        assert_eq!(fields[1].default_value.as_ref().unwrap().text, "null");
        assert!(input.syntax().span.end > input.syntax().span.start);
    }

    #[test]
    fn consumes_oak_field_spans_without_reparsing_source() {
        let source = "class User { [primary] id: uuid, manager: &User?, }";
        let input = parse_schema(source).expect("VOS parses");
        let fields = input.fields(&input.declarations()[0]);
        assert_eq!(&source[fields[0].name_span], "id");
        assert_eq!(&source[fields[0].type_syntax.span], "uuid");
        assert_eq!(&source[fields[1].type_syntax.span], "&User?");
        assert_eq!(&source[fields[0].attributes[0].span], "[primary]");
    }

    #[test]
    fn projects_operation_parameters_and_return_type_without_reparsing() {
        let source = "query active(status: utf8, limit: i64) -> [User] { User.filter(x => x.active) }";
        let operations = parse_schema(source).unwrap().project_operations().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].name, "active");
        assert_eq!(operations[0].parameters[0].name, "status");
        assert_eq!(operations[0].parameters[1].name, "limit");
        assert!(matches!(operations[0].return_type, Some(CanonicalType::List(_))));
        assert_eq!(operations[0].body.as_ref().unwrap().text, "{ User.filter(x => x.active) }");
        assert_eq!(&source[operations[0].span], source);
    }

    #[test]
    fn operation_artifact_is_strict_and_source_backed() {
        let input = parse_schema("query active(status: utf8) -> [User] { User.filter(x => x.active) }").unwrap();
        let artifact = input.project_operations_artifact().unwrap();
        assert_eq!(artifact.envelope.stage, OPERATION_PROJECTION_STAGE);
        assert_eq!(artifact.envelope.contract_format_version, OPERATION_CONTRACT_FORMAT_VERSION);
        let json = artifact.to_json().unwrap();
        assert_eq!(OperationProjectionArtifact::from_json(&json).unwrap(), artifact);

        let mut invalid = serde_json::from_str::<serde_json::Value>(&json).unwrap();
        invalid["operations"][0]["body"]["text"] = serde_json::Value::String("{ altered }".to_owned());
        assert_eq!(OperationProjectionArtifact::from_json(&invalid.to_string()).unwrap_err().code, "OPR014");
    }

    #[test]
    fn projects_durable_macro_without_reparsing() {
        let input = parse_schema("macro public_name(value: utf8) -> utf8 { value.trim() }").unwrap();
        let operations = input.project_operations().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].kind, OperationKind::Macro);
        assert_eq!(operations[0].name, "public_name");
        assert_eq!(operations[0].parameters[0].name, "value");
        assert_eq!(operations[0].identity.name, "public_name");
        assert_eq!(operations[0].identity.parameters[0].name, "value");
        assert_eq!(operations[0].identity.parameters[0].type_key, "utf8");
        assert_eq!(operations[0].identity.result, "utf8");
        assert!(operations[0].operation_id.as_str().starts_with("op_"));
        assert_eq!(operations[0].operation_id.as_str().len(), 67);
    }

    #[test]
    fn operation_identity_key_ignores_formatting_and_source_order() {
        let first = parse_schema("query active(status: utf8, limit: i64) -> [User] { User }").unwrap().project_operations().unwrap();
        let second = parse_schema("\nquery active( status : utf8 , limit:i64 )->[User]{User}\n").unwrap().project_operations().unwrap();
        assert_eq!(first[0].identity, second[0].identity);
        assert_eq!(first[0].operation_id, second[0].operation_id);
        assert!(first[0].operation_id.as_str().starts_with("op_"));
    }

    #[test]
    fn different_operations_get_different_operation_ids() {
        let source = "macro public_name(value: utf8) -> utf8 { value.trim() }\n\
                      macro private_name(value: utf8) -> utf8 { value.trim() }\n\
                      query active(status: utf8) -> [User] { User }";
        let operations = parse_schema(source).unwrap().project_operations().unwrap();
        assert_eq!(operations.len(), 3);
        let ids: Vec<&str> = operations.iter().map(|op| op.operation_id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2]);
        assert_ne!(ids[1], ids[2]);
    }

    #[test]
    fn operation_projection_artifact_is_roundtrip_under_strict_validation() {
        let artifact =
            parse_schema("query active(status: utf8) -> [User] { User.filter(x => x.active) }").unwrap().project_operations_artifact().unwrap();
        let json = artifact.to_json().unwrap();
        let roundtrip = OperationProjectionArtifact::from_json(&json).unwrap();
        assert_eq!(roundtrip, artifact);
    }

    #[test]
    fn strict_reader_rejects_empty_or_mismatched_operation_id() {
        let artifact =
            parse_schema("query active(status: utf8) -> [User] { User.filter(x => x.active) }").unwrap().project_operations_artifact().unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&artifact.to_json().unwrap()).unwrap();

        json["operations"][0]["operationId"] = serde_json::Value::String("".to_owned());
        let error = OperationProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "OPR016");

        json["operations"][0]["operationId"] = serde_json::Value::String("op_deadbeef".to_owned());
        json["operations"][0]["identity"]["result"] = serde_json::Value::String("[other]".to_owned());
        let error = OperationProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "OPR016");

        json["operations"][0]["operationId"] = serde_json::Value::String(format!("op_{}", "deadbeef".repeat(8)));
        let error = OperationProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "OPR017");
    }

    #[test]
    fn strict_reader_rejects_unknown_fields_on_operation_projection() {
        let artifact =
            parse_schema("query active(status: utf8) -> [User] { User.filter(x => x.active) }").unwrap().project_operations_artifact().unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&artifact.to_json().unwrap()).unwrap();
        json["operations"][0]["unexpectedField"] = serde_json::Value::Bool(true);
        let error = OperationProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "OPR010");
    }

    #[test]
    fn service_projection_artifact_is_roundtrip_under_strict_validation() {
        let artifact = parse_schema("namespace demo::auth\nservice IdentityService { create(request: CreateRequest) -> Identity }")
            .unwrap()
            .project_services_artifact()
            .unwrap();
        assert_eq!(artifact.envelope.stage, SERVICE_PROJECTION_STAGE);
        assert_eq!(artifact.envelope.contract_format_version, SERVICE_CONTRACT_FORMAT_VERSION);
        let json = artifact.to_json().unwrap();
        let roundtrip = ServiceProjectionArtifact::from_json(&json).unwrap();
        assert_eq!(roundtrip, artifact);
    }

    #[test]
    fn strict_reader_rejects_empty_service_name() {
        let artifact =
            parse_schema("service IdentityService { create(request: CreateRequest) -> Identity }").unwrap().project_services_artifact().unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&artifact.to_json().unwrap()).unwrap();
        json["services"][0]["name"] = serde_json::Value::String("".to_owned());
        let error = ServiceProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "SRV013");
    }

    #[test]
    fn strict_reader_rejects_empty_or_mismatched_service_id() {
        let artifact = parse_schema("namespace demo::auth\nservice IdentityService { create(request: CreateRequest) -> Identity }")
            .unwrap()
            .project_services_artifact()
            .unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&artifact.to_json().unwrap()).unwrap();

        json["services"][0]["serviceId"] = serde_json::Value::String("".to_owned());
        let error = ServiceProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "SRV015");

        json["services"][0]["serviceId"] = serde_json::Value::String(format!("svc_{}", "deadbeef".repeat(8)));
        json["services"][0]["canonicalPath"] = serde_json::Value::Array(vec![serde_json::Value::String("other".to_owned())]);
        let error = ServiceProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "SRV016");
    }

    #[test]
    fn strict_reader_rejects_unknown_fields_on_service_projection() {
        let artifact =
            parse_schema("service IdentityService { create(request: CreateRequest) -> Identity }").unwrap().project_services_artifact().unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&artifact.to_json().unwrap()).unwrap();
        json["services"][0]["unexpectedField"] = serde_json::Value::Bool(true);
        let error = ServiceProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "SRV010");
    }

    #[test]
    fn strict_reader_rejects_service_span_mismatch() {
        let artifact =
            parse_schema("service IdentityService { create(request: CreateRequest) -> Identity }").unwrap().project_services_artifact().unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&artifact.to_json().unwrap()).unwrap();
        json["services"][0]["span"]["end"] = serde_json::Value::Number(9999.into());
        let error = ServiceProjectionArtifact::from_json(&json.to_string()).unwrap_err();
        assert_eq!(error.code, "ART004");
    }

    #[test]
    fn projects_oak_output_into_versioned_artifact_without_reparsing() {
        let input =
            parse_schema("namespace demo::identity\ntable User { @@id: uuid, manager: &User?, tags: [utf8]? = null, }").expect("VOS parses");
        let contract = input.project_schema().expect("VOS projects Oak output");

        assert_eq!(contract.envelope.contract_format_version, CONTRACT_FORMAT_VERSION);
        assert_eq!(contract.envelope.stage, PROJECTION_STAGE);
        assert_eq!(contract.envelope.language_version, LANGUAGE_VERSION);
        assert_eq!(contract.envelope.canonicalization_version, CANONICALIZATION_VERSION);
        assert_eq!(contract.envelope.schema_fingerprint, None);
        assert_eq!(contract.envelope.source_units.len(), 1);
        assert_eq!(contract.types[0].canonical_path, ["demo", "identity", "User"]);
        assert_eq!(contract.types[0].fields[0].attributes[0].name, "primary");
        assert!(
            matches!(&contract.types[0].fields[1].canonical_type, CanonicalType::Optional(inner) if matches!(inner.as_ref(), CanonicalType::Reference(_)))
        );
        assert!(
            matches!(&contract.types[0].fields[2].canonical_type, CanonicalType::Optional(inner) if matches!(inner.as_ref(), CanonicalType::List(_)))
        );
        assert_eq!(contract.types[0].fields[2].default_value.as_deref(), Some("null"));
    }

    #[test]
    fn projection_reports_duplicate_canonical_types() {
        let source = "namespace demo\ntable User { id: uuid, }\ntable User { id: uuid, }";
        let input = parse_schema(source).expect("Oak parses duplicate declarations");
        let diagnostics = input.project_schema().expect_err("duplicate type must be diagnosed");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "VOS002");
        assert_eq!(diagnostics[0].span.as_ref().unwrap().start, source.rfind("table User").unwrap());
    }

    #[test]
    fn schema_only_projection_tolerates_non_schema_declarations() {
        let source = "table User { @@id: uuid, }\nusing shared::UserId;\nmacro label(value: utf8) -> utf8 { value.trim() }";
        let input = parse_schema(source).expect("Oak parses schema and non-schema declarations");
        let projection = input.project_schema_only().expect("schema-only projection skips non-schema declarations");
        assert_eq!(projection.types.len(), 1);
        assert_eq!(projection.types[0].canonical_path, ["User"]);
        // `using` imports and durable operation declarations are skipped by the
        // single-source schema projection, not reported as unsupported.
        let plain = input.project_schema().expect("plain projection skips imports and operations");
        assert_eq!(plain.types.len(), 1);
        assert_eq!(plain.types[0].canonical_path, ["User"]);
    }

}
