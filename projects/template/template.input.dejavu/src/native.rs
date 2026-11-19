//! Host-native T1 AST (Rust side) that round-trips with IR.

use crate::types::{IrNode, PartialParam, Span};
use serde_json::Value;

/// Host-native AST for one template file.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeTemplate {
    /// Top-level nodes, in source order.
    pub children: Vec<NativeNode>,
}

/// One template node and the source range it was parsed from.
///
/// Spans use the same UTF-8 byte offsets as [`IrNode`]. IR that carries no
/// spans decodes to `None`, so hosts that only feed IR still work unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeNode {
    /// Node payload (text, statement or expression form).
    pub kind: NativeKind,
    /// Byte range this node was parsed from; `None` for IR without spans.
    pub span: Option<Span>,
}

impl NativeNode {
    /// Pair a payload with its optional source span.
    pub fn new(kind: NativeKind, span: Option<Span>) -> Self {
        Self { kind, span }
    }
}

/// Payload of one native template node, mirroring [`crate::types::IrNode`].
#[derive(Debug, Clone, PartialEq)]
pub enum NativeKind {
    /// Literal text emitted verbatim.
    Text(String),
    /// Comment body; never rendered into the output.
    Comment(String),
    /// `{{ expression }}` interpolation.
    Interpolation {
        /// Expression whose value is stringified into the output.
        expression: NativeExpr,
        /// Trim level from the tag marker: `none`, `ws`, `nl`, `ws_nl` or `all`.
        trim: String,
        /// When true the value is written without HTML escaping.
        raw: bool,
    },
    /// `if` / `else if` / `else` statement.
    If {
        /// Condition evaluated before `consequent`.
        test: NativeExpr,
        /// Nodes rendered when `test` is truthy.
        consequent: Vec<NativeNode>,
        /// `else if` branches as `(condition, body)` pairs, tried in order.
        else_ifs: Vec<(NativeExpr, Vec<NativeNode>)>,
        /// `else` body; `None` when the statement has no `else` branch.
        alternate: Option<Vec<NativeNode>>,
        /// Trim level of the opening tag.
        trim: String,
    },
    /// `loop item in iterable` statement.
    For {
        /// Loop variable bound to the current element.
        item: String,
        /// Expression producing the sequence to iterate.
        iterable: NativeExpr,
        /// Body rendered once per element.
        body: Vec<NativeNode>,
        /// Trim level of the opening tag.
        trim: String,
    },
    /// Named `block` overridden by a child template via `extends`.
    Block {
        /// Block name matched against the parent template's block.
        name: String,
        /// Default body used when no child overrides this block.
        body: Vec<NativeNode>,
        /// Trim level of the opening tag.
        trim: String,
    },
    /// Inline `partial` declaration with its own parameter scope.
    Partial {
        /// Declared parameters; missing arguments fall back to their defaults.
        params: Vec<NativeParam>,
        /// Body rendered in the parameter scope.
        body: Vec<NativeNode>,
        /// Trim level of the opening tag.
        trim: String,
    },
    /// `macro name(params)` declaration invoked through a bare identifier call.
    Macro {
        /// Macro name bound in the declaring template's scope.
        name: String,
        /// Declared parameters; missing arguments fall back to their defaults.
        params: Vec<NativeParam>,
        /// Body rendered with only the macro parameters in scope.
        body: Vec<NativeNode>,
        /// Trim level of the opening tag.
        trim: String,
    },
    /// `from "<path>" import …` — static macro import.
    Import {
        /// Literal template path the macros are imported from.
        path: String,
        /// Imported macro names with their optional local aliases.
        names: Vec<NativeImportName>,
        /// Trim level of the import tag.
        trim: String,
    },
    /// `call name(args)` … `end call` — macro invocation with a caller block.
    Call {
        /// Name of the macro being invoked.
        name: String,
        /// Positional arguments passed to the macro.
        arguments: Vec<NativeExpr>,
        /// Formal parameters of the caller block, exposed via `caller()`.
        params: Vec<NativeParam>,
        /// Caller block rendered in the calling template's scope.
        body: Vec<NativeNode>,
        /// Trim level of the opening tag.
        trim: String,
    },
    /// `extends "<path>"` — single parent template.
    Extends {
        /// Literal path of the parent template.
        parent: NativeExpr,
        /// Trim level of the `extends` tag.
        trim: String,
    },
    /// `include "<path>"` — render another template in place.
    Include {
        /// Template path to include; must resolve through the loader.
        path: NativeExpr,
        /// Explicit child context merged over the inherited one.
        with: Option<NativeExpr>,
        /// `only` / `without context`: the child sees no inherited context.
        only: bool,
        /// Trim level of the include tag.
        trim: String,
    },
    /// `super()` — inlines the parent template's version of the current block.
    Super {
        /// Trim level of the `super` tag.
        trim: String,
    },
    /// `break` — terminates the nearest enclosing loop in the same lexical unit.
    Break {
        /// Trim level of the `break` tag.
        trim: String,
    },
    /// `continue` — skips the rest of the bound loop's current iteration.
    Continue {
        /// Trim level of the `continue` tag.
        trim: String,
    },
    /// Raw section; the body is emitted without evaluation.
    Raw {
        /// Verbatim text emitted without parsing or escaping.
        value: String,
        /// Trim level of the `raw` tag.
        trim: String,
    },
}

/// One declared `partial` parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeParam {
    /// Parameter name bound in the callee scope.
    pub name: String,
    /// Default expression used when the caller omits the argument.
    pub default: Option<NativeExpr>,
}

/// One `from ... import name as alias` binding.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeImportName {
    /// Macro name as declared by the target template.
    pub name: String,
    /// Local alias bound in the importing template, when `as` was used.
    pub alias: Option<String>,
}

/// Native expression tree; mirrors the `Expr.*` IR variants.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeExpr {
    /// JSON literal value.
    Literal(Value),
    /// Bare identifier resolved against the render context and locals.
    Identifier(String),
    /// `object.property` member access.
    Member {
        /// Expression producing the object being read.
        object: Box<NativeExpr>,
        /// Property name to read from the object.
        property: String,
    },
    /// `object[index]` indexed access.
    Index {
        /// Expression producing the object or sequence being indexed.
        object: Box<NativeExpr>,
        /// Expression producing the key or position.
        index: Box<NativeExpr>,
    },
    /// `callee(args…)` — function, macro or `caller()` invocation.
    Call {
        /// Expression identifying what is called.
        callee: Box<NativeExpr>,
        /// Positional arguments passed to the callee.
        arguments: Vec<NativeExpr>,
    },
    /// `expression is [not] test(args)` predicate.
    Test {
        /// Expression the predicate is applied to.
        expression: Box<NativeExpr>,
        /// Name of the test in the built-in `test` namespace.
        test: String,
        /// Optional arguments passed to the test.
        arguments: Vec<NativeExpr>,
        /// Whether the result is inverted (`is not`).
        negated: bool,
    },
    /// Binary operator application.
    Binary {
        /// Operator token, e.g. `==`, `and` or `~`.
        operator: String,
        /// Left-hand operand.
        left: Box<NativeExpr>,
        /// Right-hand operand.
        right: Box<NativeExpr>,
    },
    /// Unary operator application (`not`, `-`).
    Unary {
        /// Operator token, e.g. `not` or `-`.
        operator: String,
        /// Operand the operator is applied to.
        argument: Box<NativeExpr>,
    },
    /// `expression | filter(args…)` — filter pipeline stage.
    Pipe {
        /// Input expression passed to the filter.
        expression: Box<NativeExpr>,
        /// Name of the built-in filter to apply.
        filter: String,
        /// Extra arguments passed after the piped value.
        arguments: Vec<NativeExpr>,
    },
    /// Object literal as ordered `(key, value)` pairs.
    Object {
        /// Entries in source order.
        entries: Vec<(String, NativeExpr)>,
    },
}

/// Encode a native template as an IR root node; spans are dropped.
pub fn encode_template(t: &NativeTemplate) -> IrNode {
    IrNode::Template { children: t.children.iter().map(encode_node).collect(), span: None }
}

/// Decode IR into a native template, wrapping non-template roots in one child.
pub fn decode_template(node: &IrNode) -> NativeTemplate {
    match node {
        IrNode::Template { children, .. } => NativeTemplate { children: children.iter().map(decode_node).collect() },
        other => NativeTemplate { children: vec![decode_node(other)] },
    }
}

fn encode_node(n: &NativeNode) -> IrNode {
    let span = n.span.clone();
    match &n.kind {
        NativeKind::Text(v) => IrNode::Text { value: v.clone(), span },
        NativeKind::Comment(v) => IrNode::Comment { value: v.clone(), span },
        NativeKind::Interpolation { expression, trim, raw } => {
            IrNode::Interpolation { expression: Box::new(encode_expr(expression)), trim: trim.clone(), raw: *raw, span }
        }
        NativeKind::If { test, consequent, else_ifs, alternate, trim } => IrNode::StmtIf {
            test: Box::new(encode_expr(test)),
            consequent: consequent.iter().map(encode_node).collect(),
            else_ifs: else_ifs
                .iter()
                .map(|(t, body)| IrNode::StmtElseIf {
                    test: Box::new(encode_expr(t)),
                    consequent: body.iter().map(encode_node).collect(),
                    trim: "none".into(),
                    span: None,
                })
                .collect(),
            alternate: alternate.as_ref().map(|nodes| nodes.iter().map(encode_node).collect()),
            trim: trim.clone(),
            span,
        },
        NativeKind::For { item, iterable, body, trim } => IrNode::StmtFor {
            item: item.clone(),
            iterable: Box::new(encode_expr(iterable)),
            body: body.iter().map(encode_node).collect(),
            trim: trim.clone(),
            span,
        },
        NativeKind::Block { name, body, trim } => {
            IrNode::StmtBlock { name: name.clone(), body: body.iter().map(encode_node).collect(), trim: trim.clone(), span }
        }
        NativeKind::Partial { params, body, trim } => IrNode::StmtPartial {
            params: params
                .iter()
                .map(|p| crate::types::PartialParam { name: p.name.clone(), default: p.default.as_ref().map(encode_expr) })
                .collect(),
            body: body.iter().map(encode_node).collect(),
            trim: trim.clone(),
            span,
        },
        NativeKind::Import { path, names, trim } => IrNode::StmtImport {
            path: Box::new(IrNode::ExprLiteral { value: serde_json::Value::String(path.clone()), span: span.clone() }),
            names: names.iter().map(|n| crate::types::ImportName { name: n.name.clone(), alias: n.alias.clone() }).collect(),
            trim: trim.clone(),
            span,
        },
        NativeKind::Macro { name, params, body, trim } => IrNode::StmtMacro {
            name: name.clone(),
            params: params
                .iter()
                .map(|p| crate::types::PartialParam { name: p.name.clone(), default: p.default.as_ref().map(encode_expr) })
                .collect(),
            body: body.iter().map(encode_node).collect(),
            trim: trim.clone(),
            span,
        },
        NativeKind::Call { name, arguments, params, body, trim } => IrNode::StmtCall {
            name: name.clone(),
            arguments: arguments.iter().map(encode_expr).collect(),
            params: params.iter().map(|p| PartialParam { name: p.name.clone(), default: p.default.as_ref().map(encode_expr) }).collect(),
            body: body.iter().map(encode_node).collect(),
            trim: trim.clone(),
            span,
        },
        NativeKind::Extends { parent, trim } => IrNode::StmtExtends { parent: Box::new(encode_expr(parent)), trim: trim.clone(), span },
        NativeKind::Include { path, with, only, trim } => IrNode::StmtInclude {
            path: Box::new(encode_expr(path)),
            with: with.as_ref().map(|w| Box::new(encode_expr(w))),
            only: *only,
            trim: trim.clone(),
            span,
        },
        NativeKind::Super { trim } => IrNode::StmtSuper { trim: trim.clone(), span },
        NativeKind::Break { trim } => IrNode::StmtBreak { trim: trim.clone(), span },
        NativeKind::Continue { trim } => IrNode::StmtContinue { trim: trim.clone(), span },
        NativeKind::Raw { value, trim } => IrNode::StmtRaw { value: value.clone(), trim: trim.clone(), span },
    }
}

fn encode_expr(e: &NativeExpr) -> IrNode {
    match e {
        NativeExpr::Literal(v) => IrNode::ExprLiteral { value: v.clone(), span: None },
        NativeExpr::Identifier(name) => IrNode::ExprIdentifier { name: name.clone(), span: None },
        NativeExpr::Member { object, property } => {
            IrNode::ExprMember { object: Box::new(encode_expr(object)), property: property.clone(), span: None }
        }
        NativeExpr::Index { object, index } => {
            IrNode::ExprIndex { object: Box::new(encode_expr(object)), index: Box::new(encode_expr(index)), span: None }
        }
        NativeExpr::Call { callee, arguments } => {
            IrNode::ExprCall { callee: Box::new(encode_expr(callee)), arguments: arguments.iter().map(encode_expr).collect(), span: None }
        }
        NativeExpr::Test { expression, test, arguments, negated } => IrNode::ExprTest {
            expression: Box::new(encode_expr(expression)),
            test: test.clone(),
            arguments: arguments.iter().map(encode_expr).collect(),
            negated: *negated,
            span: None,
        },
        NativeExpr::Binary { operator, left, right } => IrNode::ExprBinary {
            operator: operator.clone(),
            left: Box::new(encode_expr(left)),
            right: Box::new(encode_expr(right)),
            span: None,
        },
        NativeExpr::Unary { operator, argument } => {
            IrNode::ExprUnary { operator: operator.clone(), argument: Box::new(encode_expr(argument)), span: None }
        }
        NativeExpr::Object { entries } => IrNode::ExprObject {
            entries: entries.iter().map(|(key, value)| crate::types::ObjectEntry { key: key.clone(), value: encode_expr(value) }).collect(),
            span: None,
        },
        NativeExpr::Pipe { expression, filter, arguments } => IrNode::ExprPipe {
            expression: Box::new(encode_expr(expression)),
            filter: filter.clone(),
            arguments: arguments.iter().map(encode_expr).collect(),
            span: None,
        },
    }
}

fn decode_node(n: &IrNode) -> NativeNode {
    match n {
        IrNode::Text { value, span } => NativeNode::new(NativeKind::Text(value.clone()), span.clone()),
        IrNode::Comment { value, span } => NativeNode::new(NativeKind::Comment(value.clone()), span.clone()),
        IrNode::Interpolation { expression, trim, raw, span } => {
            NativeNode::new(NativeKind::Interpolation { expression: decode_expr(expression), trim: trim.clone(), raw: *raw }, span.clone())
        }
        IrNode::StmtIf { test, consequent, else_ifs, alternate, trim, span } => NativeNode::new(
            NativeKind::If {
                test: decode_expr(test),
                consequent: consequent.iter().map(decode_node).collect(),
                else_ifs: else_ifs
                    .iter()
                    .map(|ei| match ei {
                        IrNode::StmtElseIf { test, consequent, .. } => (decode_expr(test), consequent.iter().map(decode_node).collect()),
                        _ => (NativeExpr::Literal(Value::Bool(false)), vec![]),
                    })
                    .collect(),
                alternate: alternate.as_ref().map(|nodes| nodes.iter().map(decode_node).collect()),
                trim: trim.clone(),
            },
            span.clone(),
        ),
        IrNode::StmtFor { item, iterable, body, trim, span } => NativeNode::new(
            NativeKind::For {
                item: item.clone(),
                iterable: decode_expr(iterable),
                body: body.iter().map(decode_node).collect(),
                trim: trim.clone(),
            },
            span.clone(),
        ),
        IrNode::StmtBlock { name, body, trim, span } => NativeNode::new(
            NativeKind::Block { name: name.clone(), body: body.iter().map(decode_node).collect(), trim: trim.clone() },
            span.clone(),
        ),
        IrNode::StmtPartial { params, body, trim, span } => NativeNode::new(
            NativeKind::Partial {
                params: params.iter().map(|p| NativeParam { name: p.name.clone(), default: p.default.as_ref().map(decode_expr) }).collect(),
                body: body.iter().map(decode_node).collect(),
                trim: trim.clone(),
            },
            span.clone(),
        ),
        IrNode::StmtImport { path, names, trim, span } => {
            let literal = match path.as_ref() {
                IrNode::ExprLiteral { value: serde_json::Value::String(s), .. } => Some(s.clone()),
                _ => None,
            };
            match literal {
                Some(lit) => NativeNode::new(
                    NativeKind::Import {
                        path: lit,
                        names: names.iter().map(|n| NativeImportName { name: n.name.clone(), alias: n.alias.clone() }).collect(),
                        trim: trim.clone(),
                    },
                    span.clone(),
                ),
                // Non-literal paths cannot be represented in the native AST;
                // the renderer/codegen reject them with a structured error.
                None => NativeNode::new(NativeKind::Text(format!("/* unsupported dynamic import {path:?} */")), span.clone()),
            }
        }
        IrNode::StmtMacro { name, params, body, trim, span } => NativeNode::new(
            NativeKind::Macro {
                name: name.clone(),
                params: params.iter().map(|p| NativeParam { name: p.name.clone(), default: p.default.as_ref().map(decode_expr) }).collect(),
                body: body.iter().map(decode_node).collect(),
                trim: trim.clone(),
            },
            span.clone(),
        ),
        IrNode::StmtCall { name, arguments, params, body, trim, span } => NativeNode::new(
            NativeKind::Call {
                name: name.clone(),
                arguments: arguments.iter().map(decode_expr).collect(),
                params: params.iter().map(|p| NativeParam { name: p.name.clone(), default: p.default.as_ref().map(decode_expr) }).collect(),
                body: body.iter().map(decode_node).collect(),
                trim: trim.clone(),
            },
            span.clone(),
        ),
        IrNode::StmtExtends { parent, trim, span } => {
            NativeNode::new(NativeKind::Extends { parent: decode_expr(parent), trim: trim.clone() }, span.clone())
        }
        IrNode::StmtInclude { path, with, only, trim, span } => NativeNode::new(
            NativeKind::Include { path: decode_expr(path), with: with.as_ref().map(|w| decode_expr(w)), only: *only, trim: trim.clone() },
            span.clone(),
        ),
        IrNode::StmtSuper { trim, span } => NativeNode::new(NativeKind::Super { trim: trim.clone() }, span.clone()),
        IrNode::StmtBreak { trim, span } => NativeNode::new(NativeKind::Break { trim: trim.clone() }, span.clone()),
        IrNode::StmtContinue { trim, span } => NativeNode::new(NativeKind::Continue { trim: trim.clone() }, span.clone()),
        IrNode::StmtRaw { value, trim, span } => NativeNode::new(NativeKind::Raw { value: value.clone(), trim: trim.clone() }, span.clone()),
        other => NativeNode::new(NativeKind::Text(format!("/* unsupported {other:?} */")), None),
    }
}

fn decode_expr(n: &IrNode) -> NativeExpr {
    match n {
        IrNode::ExprLiteral { value, .. } => NativeExpr::Literal(value.clone()),
        IrNode::ExprIdentifier { name, .. } => NativeExpr::Identifier(name.clone()),
        IrNode::ExprMember { object, property, .. } => NativeExpr::Member { object: Box::new(decode_expr(object)), property: property.clone() },
        IrNode::ExprIndex { object, index, .. } => {
            NativeExpr::Index { object: Box::new(decode_expr(object)), index: Box::new(decode_expr(index)) }
        }
        IrNode::ExprCall { callee, arguments, .. } => {
            NativeExpr::Call { callee: Box::new(decode_expr(callee)), arguments: arguments.iter().map(decode_expr).collect() }
        }
        IrNode::ExprTest { expression, test, arguments, negated, .. } => NativeExpr::Test {
            expression: Box::new(decode_expr(expression)),
            test: test.clone(),
            arguments: arguments.iter().map(decode_expr).collect(),
            negated: *negated,
        },
        IrNode::ExprBinary { operator, left, right, .. } => {
            NativeExpr::Binary { operator: operator.clone(), left: Box::new(decode_expr(left)), right: Box::new(decode_expr(right)) }
        }
        IrNode::ExprUnary { operator, argument, .. } => {
            NativeExpr::Unary { operator: operator.clone(), argument: Box::new(decode_expr(argument)) }
        }
        IrNode::ExprObject { entries, .. } => {
            NativeExpr::Object { entries: entries.iter().map(|e| (e.key.clone(), decode_expr(&e.value))).collect() }
        }
        IrNode::ExprPipe { expression, filter, arguments, .. } => NativeExpr::Pipe {
            expression: Box::new(decode_expr(expression)),
            filter: filter.clone(),
            arguments: arguments.iter().map(decode_expr).collect(),
        },
        _ => NativeExpr::Literal(Value::Null),
    }
}
