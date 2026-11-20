//! Formatter 幂等 / 往返断言（随 formatter 自 `data-text-typescript` 的解析器测试迁出，断言不变）。

use language_typescript::formatter::{FormatOptions, format_source};

fn print(source: &str) -> String {
    format_source(source, &FormatOptions::default()).expect("format")
}

#[test]
fn repro_vmz_formatter_fragments() {
    let options = FormatOptions::default();
    for source in [
        "export type ParsedOptions = Record<string, string | boolean | string[]> & { _: string[] };",
        "const FROM_SPEC_RE = /\\b(?:import|export)\\s+[^'\"\\n]*?\\s+from\\s+(['\"])([^'\"]+)\\1/g;",
        "if (/\\[[^\\]]+\\]/.test(href) || /\\/:[^/]+/.test(href)) return full;",
        "return /\\.(c?js|mjs|ts)$/i.test(token);",
    ] {
        let output = format_source(source, &options).expect("format");
        assert_eq!(format_source(&output, &options).expect("idempotent"), output, "source={source:?}");
    }
}

#[test]
fn class_expression_survives_the_formatter_round_trip() {
    for source in [
        "var X = class { };\n",
        "var X = class { a = 1; constructor(v) { this.a = v; } };\n",
        "const C = class Named extends Base { static s = 1; m() { } };\n",
        "export default class { c = 3; }\n",
    ] {
        let options = FormatOptions::default();
        let output = format_source(source, &options).expect("format");
        assert!(output.contains("class"), "class keyword lost: {output:?}");
        assert_eq!(format_source(&output, &options).expect("idempotent"), output, "not idempotent: {source:?}");
    }
}

#[test]
fn rest_parameter_is_printed_as_written() {
    assert_eq!(print("function f(...args) {}"), "function f(...args) {}");
}

#[test]
fn optional_and_typed_parameters_are_printed_as_written() {
    assert_eq!(print("function f(a?: string, b: number = 2) {}"), "function f(a?: string, b: number = 2) {}");
}

#[test]
fn arrow_destructured_and_rest_parameters_round_trip() {
    assert_eq!(print("const f = ({ a, b }) => a + b;"), "const f = ({ a, b }) => a + b;");
    assert_eq!(print("const g = (...xs) => xs;"), "const g = (...xs) => xs;");
    assert_eq!(print("const h = (x = 1) => x;"), "const h = (x = 1) => x;");
}

#[test]
fn declarators_are_printed_as_written() {
    assert_eq!(print("const a = 1;"), "const a = 1;", "printer must keep the single declarator");
    assert_eq!(print("const a = 1, b = 2;"), "const a = 1, b = 2;", "printer must print every declarator");
    assert_eq!(
        print("const { a, b: c, ...rest } = require('./m');"),
        "const { a, b: c, ...rest } = require('./m');",
        "the destructuring pattern must be printed as written"
    );
    assert_eq!(print("export const { a } = x;"), "export const { a } = x;");
}

/// The token-gap formatter collapses the space in an elision hole, but keeps the hole and the rest element.
#[test]
fn array_pattern_holes_collapse_but_survive() {
    assert_eq!(print("const [x, , ...ys] = arr;"), "const [x,, ...ys] = arr;");
}
