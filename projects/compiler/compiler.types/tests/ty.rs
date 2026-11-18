use compiler_types::*;

#[test]
fn display_scalars_and_helpers() {
    assert_eq!(SemanticType::Unit.to_string(), "unit");
    assert!(SemanticType::Bottom.is_unitish());
    assert!(SemanticType::Float64.is_float());
    assert!(SemanticType::Integer64 { signed: true }.is_i64_width());
    assert_eq!(SemanticType::Array(Box::new(SemanticType::Boolean)).to_string(), "[bool]");
}

#[test]
fn named_and_apply_display() {
    let named = SemanticType::Named(Identifier::new("Point"));
    assert_eq!(named.to_string(), "Point");
    let applied = SemanticType::Apply(Box::new(named), vec![SemanticType::Integer32 { signed: true }]);
    assert_eq!(applied.to_string(), "Point<i32>");
}
