//! 事实合同的最小行为门：入口、能力与引用管理查询必须按提交事实返回。

use compiler_analysis::{EntryContract, FunctionAnalysis, ProgramFacts, TypeDefinitionFact, TypeDefinitionKind, TypeVariantFact};
use compiler_types::{CapabilityTag, Identifier, QualifiedName, ReferenceManagement};

fn symbol(name: &str) -> QualifiedName {
    QualifiedName::new(vec![Identifier::new(name)])
}

#[test]
fn program_facts_expose_submitted_entry_and_capability() {
    let facts = ProgramFacts {
        module_name: symbol("app"),
        entries: vec![EntryContract { symbol: symbol("main"), requires_wrapper: false }],
        capabilities: vec![CapabilityTag::new("host.fs")],
        ..ProgramFacts::default()
    };
    assert_eq!(facts.primary_entry().map(|entry| entry.symbol.to_string()), Some("main".to_string()));
    assert!(facts.requires_capability("host.fs"));
    assert!(!facts.requires_capability("host.net"));
    assert!(!facts.uses_imports());
}

#[test]
fn reference_management_hint_is_resolved_by_operation_symbol() {
    let facts = ProgramFacts {
        functions: vec![FunctionAnalysis {
            symbol: symbol("render"),
            is_external: false,
            can_suspend: false,
            is_async: false,
            uses_host_interop: false,
            external_import_link: None,
            host_provider_for: None,
            reference_management_hint: Some(ReferenceManagement::PerceusRc),
        }],
        ..ProgramFacts::default()
    };
    assert_eq!(facts.reference_management_for_operations(&[symbol("render")]), Some(ReferenceManagement::PerceusRc));
    assert_eq!(facts.reference_management_for_operations(&[symbol("missing")]), None);
}

#[test]
fn nominal_type_facts_keep_variant_tags_and_flag_members() {
    let fact = TypeDefinitionFact {
        name: "Color".into(),
        kind: TypeDefinitionKind::Enum,
        variants: vec![TypeVariantFact { name: "Red".into(), tag: 0, has_payload: false }],
        flag_members: Vec::new(),
    };
    assert_eq!(fact.variants[0].tag, 0);
    assert!(matches!(fact.kind, TypeDefinitionKind::Enum));
}
