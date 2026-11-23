//! Transitional TypeScript **AST print** (`source` → parse → AST → text).
//!
//! This is **not** a CST-faithful formatter and does not use `language-format` /
//! Returns [`Err`] when parse fails or the AST contains unprintable nodes.
//! Product contract tests live in downstream `nifty-formatter` / `vmz-formatter`.

mod expr;
mod jsx;
mod stmt;

use data_text_core::{Builder, ParseSession, SourceText};

use crate::{TypeScriptBuilder, TypeScriptLanguage};

/// Minimal formatter options for the print path.
#[derive(Debug, Clone, Default)]
pub struct FormatOptions {
    /// Erase TypeScript-only syntax while printing.
    pub type_erasure: bool,
}

/// Format TypeScript/JavaScript source via parse + AST print.
pub fn format_source(source: &str, options: &FormatOptions) -> Result<String, String> {
    if source.is_empty() {
        return Ok(String::new());
    }

    let text = SourceText::new(source);
    let language = TypeScriptLanguage::default();
    let builder = TypeScriptBuilder::new(&language).with_type_erasure(options.type_erasure);
    let mut cache = ParseSession::default();
    let built = Builder::build(&builder, &text, &[], &mut cache);

    if let Err(err) = &built.result {
        return Err(format!("oak parse failed: {err:?}"));
    }
    if !built.diagnostics.is_empty() {
        return Err(format!("oak diagnostics: {:?}", built.diagnostics));
    }

    let root = built.result.ok().ok_or_else(|| "oak build returned no root".to_string())?;
    let mut out = String::new();
    for (index, stmt) in root.statements.iter().enumerate() {
        if options.type_erasure && matches!(stmt, crate::ast::Statement::ImportDeclaration(import) if import.is_type_only()) {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&stmt::print_statement(stmt).ok_or_else(|| format!("oak print unsupported for statement: {stmt:?}"))?);
    }

    if source.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{FormatOptions, format_source, stmt::print_statement};
    use crate::ast::{ClassMember, FunctionParam, Statement, TemplateElement, TypeAliasDeclaration, TypeAnnotation};
    use core::range::Range;

    /// 打印源码并断言解析与打印均无失败,返回首次打印结果。
    fn print_once(source: &str) -> String {
        format_source(source, &FormatOptions::default()).unwrap_or_else(|err| panic!("打印 `{source}` 失败: {err}"))
    }

    /// 打印 → 重新解析 → 再次打印,断言输出文本幂等且重解析无诊断。
    fn assert_roundtrip(source: &str) {
        let first = print_once(source);
        let second = print_once(&first);
        assert_eq!(first, second, "打印输出不稳定: {source}");
    }

    #[test]
    fn prints_class_and_member_decorators() {
        let source = "@ccclass('X') class Foo extends Component { @property(Label) label: Label = null; @property({ type: Number }) private static readonly speed: number = 2; start() { this.enabled = true; } }";
        assert_roundtrip(source);
        assert_eq!(print_once("@ccclass('X')\nclass Foo {}\n"), "@ccclass('X') class Foo {}\n");
    }

    #[test]
    fn prints_property_definition_variants() {
        for source in [
            "class A { static x = 1; y?: number; z: string = 'a'; w; private p = 2; readonly q: number = 3; }",
            "class A { static readonly k = 1; }",
            "class A { optional?: string; typed: Array<number> = []; }",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_for_statements() {
        for source in [
            "for (let i = 0; i < 10; i++) { sum += i; }",
            "for (;;) { break; }",
            "for (const k in obj) { use(k); }",
            "for (const x of xs) { use(x); }",
            "for (let i = 0, n = xs.length; i < n; i++) { f(xs[i]); }",
            "for (target of list) { hit(target); }",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_switch_statement() {
        for source in [
            "switch (x) { case 1: a(); break; case 2: b(); break; default: c(); }",
            "switch (x) { case 1: case 2: shared(); break; }",
            "switch (kind) { }",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_enum_declaration() {
        assert_roundtrip("enum E { A, B = 2, C = 'x' };");
        assert_eq!(print_once("enum E { A, B = 2 };"), "enum E { A, B = 2 };");
    }

    #[test]
    fn prints_interface_declaration() {
        for source in [
            "interface I { a: string; }",
            "interface I<T> extends Base { a: string; b?: T[]; greet(name: string): void; }",
            "declare interface D { x: number; }",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_namespace_declaration() {
        assert_roundtrip("namespace N { let a = 1; }");
        assert_eq!(print_once("namespace N { let a = 1; }"), "namespace N { let a = 1 }");
    }

    #[test]
    fn prints_function_and_arrow_expressions() {
        for source in [
            "const f = function (a: number): number { return a; };",
            "const g = (x: number): number => x * 2;",
            "const h = async function* gen() { yield 1; };",
            "const i = async (x) => { await p(x); };",
            "const j = function named() { return 1; };",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_object_methods_and_accessors() {
        for source in [
            "const o = { m() { return 1; }, get v() { return 2; }, set v(x) { this.n = x; }, async n() { await p; }, ...rest };",
            "const o = { a: 1, b: [2, 3], c: { d: 4 } };",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_template_and_tagged_template() {
        assert_roundtrip("const s = `a${x}b`;");
        assert_roundtrip("const t = sql`SELECT * FROM t`;");
        assert_eq!(print_once("const s = `plain`;"), "const s = `plain`");
    }

    #[test]
    fn prints_await_spread_new_and_member_expressions() {
        for source in [
            "const a = x ? y : z;",
            "const b = (f(), g);",
            "const c = await p;",
            "const d = [...xs, ...ys];",
            "const e = new Map([['k', v]]);",
            "const i = o?.[key];",
            "const j = a.b.c(1)(2);",
            "class B extends A { constructor() { super(); } }",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_variable_kinds_faithfully() {
        assert_eq!(print_once("let a = 1;"), "let a = 1");
        assert_eq!(print_once("var b = 2;"), "var b = 2");
        assert_eq!(print_once("const c = 3;"), "const c = 3");
        assert_eq!(print_once("let x: number = 1, y = 2;"), "let x: number = 1, y = 2");
    }

    #[test]
    fn prints_type_annotation_variants() {
        for source in [
            "type T = keyof typeof E;",
            "type U = A extends B ? C : D;",
            "type M = { readonly [K in keyof T]?: T[K] };",
            "type F = <A>(x: A) => A;",
            "type R = Record<string, number>;",
            "type N = (A | B)[];",
            "type I = A & (B | C);",
            "type L = 'a' | 1 | true;",
            "type O = { a: string; greet(): void };",
        ] {
            assert_roundtrip(source);
        }
    }

    #[test]
    fn prints_control_flow_bodies_with_braces() {
        assert_eq!(print_once("if (a) return 1; else return 2;"), "if (a) { return 1 } else { return 2 }");
        assert_roundtrip("while (a) { b(); }");
        assert_roundtrip("do { a(); } while (b);");
        assert_roundtrip("try { a(); } catch (e) { b(e); } finally { c(); }");
        assert_roundtrip("try { a(); } catch { }");
        assert_roundtrip("throw new Error('x');");
    }

    /// 直接构造 `TypeAlias` 语句打印指定类型注解(构建器对高级类型只保留原文,
    /// 这些变体来自下游直接组装的 AST,需验证打印器分支本身)。
    fn print_ty(ty: TypeAnnotation) -> String {
        let stmt = Statement::TypeAlias(TypeAliasDeclaration {
            decorators: Vec::new(),
            modifiers: Vec::new(),
            type_params: Vec::new(),
            name: "T".into(),
            ty,
            span: Range { start: 0, end: 0 },
        });
        print_statement(&stmt).expect("type alias 应可打印")
    }

    #[test]
    fn prints_advanced_type_annotations_from_ast() {
        let id = |name: &str| TypeAnnotation::Reference { name: name.into(), args: Vec::new() };
        assert_eq!(print_ty(TypeAnnotation::KeyOf(Box::new(TypeAnnotation::Query("E".into())))), "type T = keyof typeof E;");
        assert_eq!(
            print_ty(TypeAnnotation::Conditional {
                check_type: Box::new(TypeAnnotation::Union(vec![id("A"), id("B")])),
                extends_type: Box::new(id("C")),
                true_type: Box::new(id("D")),
                false_type: Box::new(TypeAnnotation::Conditional {
                    check_type: Box::new(id("E")),
                    extends_type: Box::new(id("F")),
                    true_type: Box::new(id("G")),
                    false_type: Box::new(id("H")),
                }),
            }),
            "type T = (A | B) extends C ? D : (E extends F ? G : H);"
        );
        assert_eq!(
            print_ty(TypeAnnotation::Mapped {
                key_name: "K".into(),
                key_type: Box::new(TypeAnnotation::KeyOf(Box::new(id("T")))),
                value_type: Box::new(id("V")),
                readonly: Some(true),
                optional: Some(false),
            }),
            "type T = { readonly [K in keyof T]-?: V };"
        );
        assert_eq!(
            print_ty(TypeAnnotation::TemplateLiteral(vec![
                TemplateElement::String("a".into()),
                TemplateElement::Type(Box::new(id("b"))),
                TemplateElement::String("c".into()),
            ])),
            "type T = `a${b}c`;"
        );
        assert_eq!(print_ty(TypeAnnotation::Infer("U".into())), "type T = infer U;");
        assert_eq!(print_ty(TypeAnnotation::Array(Box::new(TypeAnnotation::Union(vec![id("A"), id("B")])))), "type T = (A | B)[];");
        assert_eq!(
            print_ty(TypeAnnotation::Intersection(vec![id("A"), TypeAnnotation::Union(vec![id("B"), id("C")])])),
            "type T = A & (B | C);"
        );
        assert_eq!(
            print_ty(TypeAnnotation::Function {
                type_params: vec![crate::ast::TypeParameter {
                    name: "A".into(),
                    constraint: None,
                    default: None,
                    span: Range { start: 0, end: 0 }
                }],
                args: vec![FunctionParam {
                    decorators: Vec::new(),
                    name: "x".into(),
                    binding_names: vec!["x".into()],
                    pattern_text: None,
                    default: None,
                    is_rest: false,
                    ty: Some(id("A")),
                    optional_marker: None,
                    span: Range { start: 0, end: 0 },
                }],
                return_type: Box::new(id("A")),
            }),
            "type T = <A>(x: A) => A;"
        );
        assert_eq!(
            print_ty(TypeAnnotation::Object(vec![
                ClassMember::Property {
                    decorators: Vec::new(),
                    name: "a".into(),
                    ty: Some(TypeAnnotation::Predefined("string".into())),
                    initializer: None,
                    visibility: None,
                    modifiers: Vec::new(),
                    optional_marker: None,
                    span: Range { start: 0, end: 0 },
                },
                ClassMember::Method {
                    decorators: Vec::new(),
                    name: "greet".into(),
                    type_params: Vec::new(),
                    params: Vec::new(),
                    return_type: Some(TypeAnnotation::Predefined("void".into())),
                    body: Vec::new(),
                    visibility: None,
                    modifiers: Vec::new(),
                    optional_marker: None,
                    span: Range { start: 0, end: 0 },
                },
            ])),
            "type T = { a: string; greet(): void; };"
        );
    }
}
