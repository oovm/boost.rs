use compiler_optimize::{
    ConstantInterpreter, EGraphHost, Identifier, QualifiedName, RewriteEquation, RewritePhase, StructuredRewrite, Term, TermPattern,
    filter_equations, filter_term_rewrites, parse_pattern, parse_term, qualified_from_dotted,
};

/// 测试用中立算术解释：`math.add/sub/mul` 与一元 `math.neg`。
struct MathInterpreter;

impl ConstantInterpreter for MathInterpreter {
    fn fold_binary(&self, operator: &QualifiedName, left: i64, right: i64) -> Option<i64> {
        let name = operator.parts().get(1)?.as_str();
        match (operator.parts().first()?.as_str(), name) {
            ("math", "add") => Some(left.saturating_add(right)),
            ("math", "sub") => Some(left.saturating_sub(right)),
            ("math", "mul") => Some(left.saturating_mul(right)),
            _ => None,
        }
    }

    fn fold_unary(&self, operator: &QualifiedName, operand: i64) -> Option<i64> {
        if operator.parts().len() == 2 && operator.parts()[0].as_str() == "math" && operator.parts()[1].as_str() == "neg" {
            Some(-operand)
        }
        else {
            None
        }
    }
}

fn identity_rewrites() -> Vec<StructuredRewrite> {
    vec![
        StructuredRewrite {
            name: Identifier::new("math.add-identity-right"),
            left: parse_pattern("math.add(?x, 0)").expect("pattern"),
            right: parse_pattern("?x").expect("pattern"),
        },
        StructuredRewrite {
            name: Identifier::new("math.sub-self"),
            left: parse_pattern("math.sub(?x, ?x)").expect("pattern"),
            right: parse_pattern("0").expect("pattern"),
        },
    ]
}

#[test]
fn constant_fold_extracts_literal() {
    let mut host = EGraphHost::default();
    let root = host.add_term(&parse_term("math.add(2, 3)").expect("term"));
    let stats = host.saturate(&[], &[], Some(&MathInterpreter));
    assert!(stats.constant_fold_count >= 1);
    assert!(stats.saturated);
    assert_eq!(host.extract_term(root), Term::Literal(5));
}

#[test]
fn nested_constant_fold_and_unary_fold() {
    let mut host = EGraphHost::default();
    let root = host.add_term(&parse_term("math.add(math.mul(2, 3), 4)").expect("term"));
    let negated = host.add_term(&parse_term("math.neg(7)").expect("term"));
    let stats = host.saturate(&[], &[], Some(&MathInterpreter));
    assert!(stats.saturated);
    assert_eq!(host.extract_term(root), Term::Literal(10));
    assert_eq!(host.extract_term(negated), Term::Literal(-7));
}

#[test]
fn identity_rewrite_extracts_symbol() {
    let mut host = EGraphHost::default();
    let x = qualified_from_dotted("demo.x");
    let root = host.add_term(&Term::apply(qualified_from_dotted("math.add"), vec![Term::symbol(x.clone()), Term::literal(0)]));
    let stats = host.saturate(&identity_rewrites(), &[], None);
    assert!(stats.applied_rewrite_count >= 1);
    assert_eq!(host.extract_term(root), Term::Symbol(x));
}

#[test]
fn congruence_merges_equal_arguments() {
    let mut host = EGraphHost::default();
    let a = qualified_from_dotted("demo.a");
    let b = qualified_from_dotted("demo.b");
    let f_a = host.add_term(&Term::apply(qualified_from_dotted("demo.f"), vec![Term::symbol(a.clone())]));
    let f_b = host.add_term(&Term::apply(qualified_from_dotted("demo.f"), vec![Term::symbol(b.clone())]));
    let equations = [(a.clone(), b.clone())];
    let stats = host.saturate(&[], &equations, None);
    assert!(stats.applied_equation_count >= 1);
    // 参数合并后，同 operator 的 apply 经 congruence 进入同一等价类。
    assert_eq!(host.extract_term(f_a), host.extract_term(f_b));
}

#[test]
fn flat_union_then_class_of_symbol_resolves_representative() {
    let mut host = EGraphHost::default();
    let left = qualified_from_dotted("graphic.dot");
    let right = qualified_from_dotted("graphic.dot_commutative");
    assert!(host.union_flat(&left, &right));
    // 合并经 pending 队列延迟生效，saturate 完成 congruence rebuild。
    let stats = host.saturate(&[], &[], None);
    assert!(stats.saturated);
    assert!(!host.union_flat(&left, &right));
    let class = host.class_of_symbol(&left).expect("class");
    // 同成本符号平局按字典序取代表元。
    assert_eq!(host.extract_term(class), Term::Symbol(left));
    assert_eq!(host.class_of_symbol(&right), Some(class));
}

#[test]
fn filters_gate_by_phase_and_capabilities() {
    let rewrite = compiler_optimize::TermRewrite::new(
        "needs-gpu",
        parse_pattern("math.add(?x, 0)").expect("pattern"),
        parse_pattern("?x").expect("pattern"),
        RewritePhase::Saturate,
    )
    .with_capabilities(vec!["gpu-shader".into()]);
    let equation = RewriteEquation {
        left: qualified_from_dotted("graphic.dot"),
        right: qualified_from_dotted("graphic.dot_alt"),
        phase: RewritePhase::Saturate,
        required_capabilities: vec!["gpu-shader".into()],
    };

    let empty: Vec<compiler_optimize::CapabilityTag> = Vec::new();
    assert!(filter_term_rewrites(std::slice::from_ref(&rewrite), RewritePhase::Saturate, &empty).is_empty());
    assert!(filter_equations(std::slice::from_ref(&equation), RewritePhase::Saturate, &empty).is_empty());

    let provided = vec!["gpu-shader".into()];
    assert_eq!(filter_term_rewrites(std::slice::from_ref(&rewrite), RewritePhase::Saturate, &provided).len(), 1);
    assert_eq!(filter_equations(std::slice::from_ref(&equation), RewritePhase::Saturate, &provided).len(), 1);
    assert!(filter_term_rewrites(std::slice::from_ref(&rewrite), RewritePhase::Normalize, &provided).is_empty());
}

#[test]
fn term_pattern_exact_roundtrips_through_parse() {
    let term = parse_term("math.mul(math.add(1, 2), 3)").expect("term");
    let pattern = TermPattern::exact(&term);
    assert_eq!(pattern, parse_pattern("math.mul(math.add(1, 2), 3)").expect("pattern"));
}
