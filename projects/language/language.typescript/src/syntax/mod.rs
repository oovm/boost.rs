//! TypeScript 子集的真前端：词法 + 递归下降语法 → `language.typescript.ir`。

pub mod error;
pub mod lexer;
pub mod parser;

pub use error::SyntaxError;
pub use lexer::{lex, Punct, Token, TokenKind};
pub use parser::{parse_program, Parser};

#[cfg(test)]
mod tests {
    use language_typescript_ir::{statement::Statement, Expression, Program};
    use language_typescript_types::TsValue;

    use super::error::SyntaxError;
    use super::{lexer::lex, parser::parse_program, Punct, TokenKind};

    #[test]
    fn lexes_string_literal_with_spaces_and_escapes() {
        let tokens = lex("let a = 'hello world \\n!';").expect("词法必须通过");
        let string = tokens.iter().find(|token| token.kind == TokenKind::Str).expect("必须有字符串 token");
        assert_eq!(string.text, "hello world \n!");
        assert_eq!(string.line, 1);
        assert_eq!(string.column, 9);
    }

    #[test]
    fn lexes_comments_and_tracks_lines() {
        let tokens = lex("// line comment\n/* block\ncomment */ 42").expect("注释必须被跳过");
        let number = tokens.iter().find(|token| matches!(token.kind, TokenKind::Number(_))).expect("数字必须存在");
        assert_eq!(number.line, 3);
    }

    #[test]
    fn lexes_multi_char_operators() {
        let tokens = lex("a === b !== c == d != e && f || g").expect("运算符词法");
        let puncts: Vec<_> = tokens.iter().filter_map(|token| match &token.kind {
            TokenKind::Punct(p) => Some(*p),
            _ => None,
        }).collect();
        assert_eq!(puncts, vec![Punct::StrictEq, Punct::StrictNeq, Punct::EqEq, Punct::NotEq, Punct::AmpAmp, Punct::PipePipe]);
    }

    #[test]
    fn unterminated_string_is_structured() {
        let error = lex("'abc").expect_err("未闭合字符串必须失败");
        assert_eq!(error.line, 1);
        assert_eq!(error.column, 1);
        assert!(error.message.contains("未闭合"), "{error}");
    }

    #[test]
    fn parses_nested_object_literal() {
        let program = parse_program("const o = { a: 1, b: { c: \"x\" } };").expect("嵌套对象必须解析");
        let Statement::VariableDeclaration { name, initializer, is_const, .. } = &program.statements[0] else {
            panic!("期望变量声明: {:?}", program.statements[0]);
        };
        assert_eq!(name, "o");
        assert!(is_const);
        let Expression::Object(fields) = initializer.as_deref().expect("必须有初始化表达式") else {
            panic!("期望对象字面量");
        };
        assert_eq!(fields.len(), 2);
        let Expression::Object(inner) = &fields[1].1 else { panic!("期望嵌套对象") };
        assert_eq!(inner.len(), 1);
    }

    #[test]
    fn parses_union_type_and_param_annotations() {
        let program = parse_program("export function f(a: number | string, b: { x: number }): string { return \"\" }").expect("联合类型解析");
        let Statement::ExportDeclaration { declaration, .. } = &program.statements[0] else { panic!("期望导出") };
        let Statement::FunctionDeclaration { params, param_types, return_type, .. } = declaration.as_ref() else { panic!("期望函数声明") };
        assert_eq!(params.as_slice(), ["a", "b"]);
        assert_eq!(param_types.len(), 2);
        assert!(matches!(param_types[0], Some(language_typescript_ir::TypeAnnotation::Union(_))));
        assert!(matches!(return_type, Some(language_typescript_ir::TypeAnnotation::Primitive(_))));
    }

    #[test]
    fn parses_interface_members() {
        let program = parse_program("interface Point { x: number; y: number; move(dx: number): void }").expect("接口解析");
        let Statement::InterfaceDeclaration { name, members, .. } = &program.statements[0] else { panic!("期望接口声明") };
        assert_eq!(name, "Point");
        assert_eq!(members.len(), 3);
    }

    #[test]
    fn syntax_error_reports_line_column_and_expectation() {
        let error = parse_program("let x = ;").expect_err("非法表达式必须失败");
        let SyntaxError { line, column, expected, .. } = &error;
        assert_eq!(*line, 1);
        assert_eq!(*column, 9);
        assert!(!expected.is_empty(), "错误必须给出期望");
    }

    #[test]
    fn parses_statements_subset() {
        let program: Program = parse_program(
            r#"
            const n = 3;
            export function g(x: number): number {
                if (x > 0) { return x; } else { return -x; }
            }
            let i = 0;
            while (i < n) { i = i + 1; }
            for (let j = 0; j < n; j = j + 1) { break; }
            "#,
        )
        .unwrap_or_else(|error| panic!("子集语句必须解析: {error}"));
        assert_eq!(program.statements.len(), 5);
    }

    #[test]
    fn literal_values_carry_kinds() {
        let program = parse_program("const a = 1.5; const b = \"s\"; const c = true; const d = null; const e = undefined;").expect("字面量解析");
        let values: Vec<TsValue> = program
            .statements
            .iter()
            .map(|statement| match statement {
                Statement::VariableDeclaration { initializer: Some(expr), .. } => match expr.as_ref() {
                    Expression::Literal(value) => value.clone(),
                    other => panic!("期望字面量: {other:?}"),
                },
                other => panic!("期望声明: {other:?}"),
            })
            .collect();
        assert!(matches!(values[0], TsValue::Number(_)));
        assert!(matches!(&values[1], TsValue::String(text) if text == "s"));
        assert_eq!(values[2], TsValue::Boolean(true));
        assert_eq!(values[3], TsValue::Null);
        assert_eq!(values[4], TsValue::Undefined);
    }
}
