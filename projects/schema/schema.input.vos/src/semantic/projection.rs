use core::range::Range;
use data_text_vos::{VosDeclarationKind, VosField, VosFieldAttribute, VosRoot, VosServiceMethod, VosTypeArgument, VosTypeSyntax};
use schema_types::*;

/// Projects operation declarations without parsing their bodies.
pub fn project_operations(root: &VosRoot) -> Result<Vec<OperationProjection>, Vec<ContractDiagnostic>> {
    let mut namespace = Vec::new();
    let mut operations = Vec::new();
    let mut diagnostics = Vec::new();
    for declaration in &root.declarations {
        if declaration.kind == VosDeclarationKind::Namespace {
            namespace = declaration.path.clone().unwrap_or_default();
            continue;
        }
        let kind = match declaration.kind {
            VosDeclarationKind::Query => OperationKind::Query,
            VosDeclarationKind::Udf => OperationKind::Udf,
            VosDeclarationKind::Micro => OperationKind::Micro,
            VosDeclarationKind::Macro => OperationKind::Macro,
            _ => continue,
        };
        let Some(name) = declaration.name.clone()
        else {
            diagnostics.push(diagnostic("OPR001", "operation declaration is missing a name", Some(declaration.span)));
            continue;
        };
        let parameters = declaration
            .parameters
            .iter()
            .map(|parameter| OperationParameter {
                name: parameter.name.clone(),
                type_expr: canonical_type(&parameter.type_expr),
                span: parameter.span,
            })
            .collect::<Vec<_>>();
        let identity = match operation_identity(&namespace, kind, &name, &parameters, declaration.return_type_expr.as_ref()) {
            Ok(identity) => identity,
            Err(error) => {
                diagnostics.push(diagnostic(error.code.as_str(), &error.message, Some(declaration.span)));
                continue;
            }
        };
        let attributes = match declaration.attributes.iter().map(resolve_attribute).collect::<Result<Vec<_>, _>>() {
            Ok(attributes) => attributes,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                continue;
            }
        };
        operations.push(OperationProjection {
            kind,
            name,
            canonical_path: identity.canonical_path.clone(),
            operation_id: OperationId::derive(&identity),
            identity,
            parameters,
            return_type: declaration.return_type_expr.as_ref().map(canonical_type),
            attributes,
            signature: declaration.signature.as_ref().map(slice_contract),
            body: declaration.body.as_ref().map(slice_contract),
            span: declaration.span,
        });
    }
    if diagnostics.is_empty() { Ok(operations) } else { Err(diagnostics) }
}

fn operation_identity(
    namespace: &[String],
    kind: OperationKind,
    name: &str,
    parameters: &[OperationParameter],
    return_type: Option<&VosTypeSyntax>,
) -> Result<OperationIdentityKey, schema_types::ArtifactError> {
    let parameters = parameters
        .iter()
        .map(|parameter| OperationParameterKey::new(&parameter.name, &canonical_type_key(&parameter.type_expr)))
        .collect::<Result<Vec<_>, _>>()?;
    let result = return_type.map(|value| canonical_type_key(&canonical_type(value))).unwrap_or_else(|| "unit".to_owned());
    OperationIdentityKey::new(namespace, kind, name, parameters, &result)
}

/// Projects operation declarations into a strict versioned artifact.
pub fn project_operations_artifact(root: &VosRoot) -> Result<OperationProjectionArtifact, Vec<ContractDiagnostic>> {
    Ok(OperationProjectionArtifact {
        envelope: ContractEnvelope {
            stage: OPERATION_PROJECTION_STAGE.to_owned(),
            contract_format_version: OPERATION_CONTRACT_FORMAT_VERSION.to_owned(),
            language_version: LANGUAGE_VERSION.to_owned(),
            canonicalization_version: CANONICALIZATION_VERSION.to_owned(),
            schema_fingerprint: None,
            source_units: vec![SourceUnit { source_unit_id: 0, source: root.source.clone() }],
        },
        operations: project_operations(root)?,
    })
}

/// Projects language service declarations without lowering their bodies.
pub fn project_services(root: &VosRoot) -> Result<Vec<ServiceProjection>, Vec<ContractDiagnostic>> {
    let mut namespace = Vec::new();
    let mut services = Vec::new();
    let mut diagnostics = Vec::new();
    for declaration in &root.declarations {
        match declaration.kind {
            VosDeclarationKind::Namespace => namespace = declaration.path.clone().unwrap_or_default(),
            VosDeclarationKind::Service => {
                let Some(name) = declaration.name.clone()
                else {
                    diagnostics.push(diagnostic("SRV001", "service declaration is missing a name", Some(declaration.span)));
                    continue;
                };
                let mut canonical_path = namespace.clone();
                canonical_path.push(name.clone());
                if services.iter().any(|item: &ServiceProjection| item.canonical_path == canonical_path) {
                    diagnostics.push(diagnostic("SRV002", "duplicate service declaration", Some(declaration.span)));
                    continue;
                }
                let identity = match ServiceIdentityKey::new(canonical_path.clone()) {
                    Ok(identity) => identity,
                    Err(error) => {
                        diagnostics.push(diagnostic(error.code.as_str(), &error.message, Some(declaration.span)));
                        continue;
                    }
                };
                let attributes = match declaration.attributes.iter().map(resolve_attribute).collect::<Result<Vec<_>, _>>() {
                    Ok(attributes) => attributes,
                    Err(diagnostic) => {
                        diagnostics.push(diagnostic);
                        continue;
                    }
                };
                let methods = match declaration.methods.iter().map(project_service_method).collect::<Result<Vec<_>, _>>() {
                    Ok(methods) => methods,
                    Err(diagnostic) => {
                        diagnostics.push(diagnostic);
                        continue;
                    }
                };
                services.push(ServiceProjection {
                    name,
                    canonical_path,
                    service_id: ServiceId::derive(&identity),
                    attributes,
                    methods,
                    body: declaration.body.as_ref().map(slice_contract),
                    span: declaration.span,
                });
            }
            _ => continue,
        }
    }
    if diagnostics.is_empty() { Ok(services) } else { Err(diagnostics) }
}

/// Projects language service declarations into a strict versioned artifact.
pub fn project_services_artifact(root: &VosRoot) -> Result<ServiceProjectionArtifact, Vec<ContractDiagnostic>> {
    Ok(ServiceProjectionArtifact {
        envelope: ContractEnvelope {
            stage: SERVICE_PROJECTION_STAGE.to_owned(),
            contract_format_version: SERVICE_CONTRACT_FORMAT_VERSION.to_owned(),
            language_version: LANGUAGE_VERSION.to_owned(),
            canonicalization_version: CANONICALIZATION_VERSION.to_owned(),
            schema_fingerprint: None,
            source_units: vec![SourceUnit { source_unit_id: 0, source: root.source.clone() }],
        },
        services: project_services(root)?,
    })
}

fn slice_contract(slice: &data_text_vos::VosSyntaxSlice) -> SyntaxSliceContract {
    SyntaxSliceContract { text: slice.text.clone(), span: slice.span }
}

fn project_service_method(method: &VosServiceMethod) -> Result<ServiceMethodProjection, ContractDiagnostic> {
    let parameters = method
        .parameters
        .iter()
        .map(|parameter| OperationParameter {
            name: parameter.name.clone(),
            type_expr: canonical_type(&parameter.type_expr),
            span: parameter.span,
        })
        .collect();
    let (return_type, stream) = match method.return_type.as_ref() {
        Some(return_type) => (Some(canonical_type(&return_type.type_expr)), return_type.stream),
        None => (None, false),
    };
    let attributes = method.attributes.iter().map(resolve_attribute).collect::<Result<Vec<_>, _>>()?;
    Ok(ServiceMethodProjection { name: method.name.clone(), parameters, return_type, stream, attributes, span: method.span })
}

/// Projects the supported syntax without claiming symbol resolution.
pub fn project_schema(root: &VosRoot) -> Result<SchemaProjection, Vec<ContractDiagnostic>> {
    let mut namespace = Vec::new();
    let mut types = Vec::new();
    let mut diagnostics = Vec::new();
    for declaration in &root.declarations {
        match declaration.kind {
            VosDeclarationKind::Namespace => namespace = declaration.path.clone().unwrap_or_default(),
            VosDeclarationKind::Table | VosDeclarationKind::Class | VosDeclarationKind::Enums | VosDeclarationKind::Flags => {
                let Some(name) = declaration.name.clone()
                else {
                    diagnostics.push(diagnostic("VOS001", "type declaration is missing a name", Some(declaration.span)));
                    continue;
                };
                let mut canonical_path = namespace.clone();
                canonical_path.push(name);
                if types.iter().any(|item: &TypeContract| item.canonical_path == canonical_path) {
                    diagnostics.push(diagnostic("VOS002", "duplicate type declaration", Some(declaration.span)));
                    continue;
                }
                if matches!(declaration.kind, VosDeclarationKind::Enums | VosDeclarationKind::Flags) {
                    let mut member_names = std::collections::BTreeSet::new();
                    for member in &declaration.members {
                        if !member_names.insert(&member.name) {
                            diagnostics.push(diagnostic("VOS006", "duplicate numeric member declaration", Some(member.name_span)));
                        }
                    }
                    match declaration.members.iter().map(resolve_member).collect::<Result<Vec<_>, _>>() {
                        Ok(members) => {
                            let numeric_kind = declaration.kind;
                            let mut values = std::collections::BTreeMap::new();
                            let mut valid = true;
                            for member in &members {
                                if numeric_kind == VosDeclarationKind::Flags && (member.value <= 0 || !(member.value as u64).is_power_of_two())
                                {
                                    diagnostics.push(diagnostic(
                                        "VOS009",
                                        &format!("`flags` member `{}` must declare exactly one nonzero bit", member.canonical_name),
                                        Some(member.span.clone()),
                                    ));
                                    valid = false;
                                }
                                if values.insert(member.value, member.canonical_name.as_str()).is_some() {
                                    diagnostics.push(diagnostic(
                                        "VOS010",
                                        &format!("numeric member `{}` reuses the value `{}`", member.canonical_name, member.value),
                                        Some(member.span.clone()),
                                    ));
                                    valid = false;
                                }
                            }
                            if valid {
                                types.push(TypeContract {
                                    canonical_path,
                                    kind: if numeric_kind == VosDeclarationKind::Enums { TypeContractKind::Enums } else { TypeContractKind::Flags },
                                    fields: Vec::new(),
                                    members,
                                    span: declaration.span,
                                });
                            }
                        }
                        Err(error) => diagnostics.push(error),
                    }
                    continue;
                }
                let mut field_names = std::collections::BTreeSet::new();
                for field in &declaration.fields {
                    if !field_names.insert(&field.name) {
                        diagnostics.push(diagnostic("VOS003", "duplicate field declaration", Some(field.name_span)));
                    }
                }
                let fields = declaration.fields.iter().map(resolve_field).collect::<Result<Vec<_>, _>>();
                match fields {
                    Ok(fields) => types.push(TypeContract {
                        canonical_path,
                        kind: if declaration.kind == VosDeclarationKind::Table { TypeContractKind::Table } else { TypeContractKind::Class },
                        fields,
                        members: Vec::new(),
                        span: declaration.span,
                    }),
                    Err(error) => diagnostics.push(error),
                }
            }
            // Service and operation-like declarations have their own projection
            // functions and are intentionally skipped here.
            //
            // `using` is an import statement: it names another source unit, which a
            // single-source projection cannot resolve, so it is skipped instead of
            // failing the schema contract. Cross-unit resolution stays in
            // [`super::resolve::resolve_using_tree`].
            VosDeclarationKind::Service
            | VosDeclarationKind::Query
            | VosDeclarationKind::Udf
            | VosDeclarationKind::Micro
            | VosDeclarationKind::Macro
            | VosDeclarationKind::Using => continue,
            _ => diagnostics.push(diagnostic("VOS004", "declaration is not supported by this contract projection", Some(declaration.span))),
        }
    }
    if diagnostics.is_empty() {
        Ok(SchemaProjection {
            envelope: ContractEnvelope {
                stage: PROJECTION_STAGE.to_owned(),
                contract_format_version: CONTRACT_FORMAT_VERSION.to_owned(),
                language_version: LANGUAGE_VERSION.to_owned(),
                canonicalization_version: CANONICALIZATION_VERSION.to_owned(),
                schema_fingerprint: None,
                source_units: vec![SourceUnit { source_unit_id: 0, source: root.source.clone() }],
            },
            types,
        })
    }
    else {
        Err(diagnostics)
    }
}

/// Projects only schema declarations, tolerating everything else.
///
/// Single filtering entry for hosts that consume a schema-only projection:
/// operation-like and other non-schema declarations are skipped before
/// projection instead of failing the schema contract with `VOS004`.
pub fn project_schema_only(root: &VosRoot) -> Result<SchemaProjection, Vec<ContractDiagnostic>> {
    let mut schema_root = root.clone();
    schema_root.declarations.retain(|declaration| {
        matches!(
            declaration.kind,
            VosDeclarationKind::Namespace
                | VosDeclarationKind::Table
                | VosDeclarationKind::Class
                | VosDeclarationKind::Enums
                | VosDeclarationKind::Flags
        )
    });
    project_schema(&schema_root)
}

/// Resolves one `enums` or `flags` member into its stable numeric identity.
fn resolve_member(member: &data_text_vos::VosDeclarationMember) -> Result<MemberContract, ContractDiagnostic> {
    let Some(text) = &member.value
    else {
        return Err(diagnostic("VOS007", "numeric member requires an explicit value", Some(member.name_span)));
    };
    let value = parse_member_number(text)
        .ok_or_else(|| diagnostic("VOS008", &format!("member value `{text}` is not a 64-bit integer"), member.value_span))?;
    Ok(MemberContract { canonical_name: member.name.clone(), value, span: member.span })
}

/// Reads a decimal or `0x` hexadecimal member value.
fn parse_member_number(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let magnitude = match body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => body.parse::<i64>().ok()?,
    };
    Some(if negative { magnitude.checked_neg()? } else { magnitude })
}

fn resolve_field(field: &VosField) -> Result<FieldContract, ContractDiagnostic> {
    Ok(FieldContract {
        canonical_name: field.name.clone(),
        canonical_type: canonical_type(&field.type_expr),
        attributes: field.attributes.iter().map(resolve_attribute).collect::<Result<Vec<_>, _>>()?,
        default_value: field.default_value.as_ref().map(|value| value.text.clone()),
        span: field.span,
    })
}

fn resolve_attribute(attribute: &VosFieldAttribute) -> Result<AttributeContract, ContractDiagnostic> {
    let name = if attribute.text == "@@" {
        "primary".to_owned()
    }
    else if attribute.text == "@" {
        "unique".to_owned()
    }
    else {
        let Some(name) = &attribute.name
        else {
            return Err(diagnostic("VOS005", "attribute has no structured name", Some(attribute.span)));
        };
        // builder exposes the first identifier as `name` for both plain
        // `[foo]` and argument-bearing `[foo(bar)]` attributes, so we accept
        // either shape — the full source text lives in `syntax` for downstream
        // consumers that need the arguments.
        name.clone()
    };
    Ok(AttributeContract { name, syntax: attribute.text.clone(), span: attribute.span })
}

fn canonical_type(value: &VosTypeSyntax) -> CanonicalType {
    match value {
        VosTypeSyntax::Named { path, .. } => CanonicalType::Named(path.clone()),
        VosTypeSyntax::Reference { target, .. } => CanonicalType::Reference(Box::new(canonical_type(target))),
        VosTypeSyntax::Optional { inner, .. } => CanonicalType::Optional(Box::new(canonical_type(inner))),
        VosTypeSyntax::List { element, .. } => CanonicalType::List(Box::new(canonical_type(element))),
        VosTypeSyntax::Generic { path, arguments, .. } => CanonicalType::Generic {
            path: path.clone(),
            arguments: arguments
                .iter()
                .map(|argument| match argument {
                    VosTypeArgument::Type(value) => CanonicalTypeArgument::Type(canonical_type(value)),
                    VosTypeArgument::Literal(value) => CanonicalTypeArgument::Literal(value.text.clone()),
                })
                .collect(),
        },
    }
}

fn diagnostic(code: &str, message: &str, span: Option<Range<usize>>) -> ContractDiagnostic {
    ContractDiagnostic { code: code.to_owned(), message: message.to_owned(), span }
}
