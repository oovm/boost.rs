use serde::{Deserialize, Deserializer, Serialize};

/// The only IR contract version accepted on the wire (`/v0` namespace).
/// Deserialization of any other `irVersion` is rejected explicitly.
pub const IR_VERSION: &str = "v0";

/// Top-level IR document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IrDocument {
    /// Wire contract version; only [`IR_VERSION`] (`"v0"`) deserializes.
    #[serde(deserialize_with = "deserialize_ir_version")]
    pub ir_version: String,
    /// Delimiter and syntax-mode configuration used to produce the body.
    pub language: Language,
    /// Root node holding the document's top-level nodes.
    pub body: IrNode,
}

/// Language + delimiter configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Language {
    /// Syntax mode name; `"template"` selects the T1 template grammar.
    pub syntax_mode: String,
    /// Delimiter and feature flags for template constructs.
    pub template: TemplateConfig,
}

/// Template delimiter / feature flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateConfig {
    /// Opening delimiter of a code tag (default `<%`).
    pub code_start: String,
    /// Closing delimiter of a code tag (default `%>`).
    pub code_end: String,
    /// Opening delimiter of a comment tag (default `<#`).
    pub comment_start: String,
    /// Closing delimiter of a comment tag (default `#>`).
    pub comment_end: String,
    /// Whether `|` in an expression parses as a filter pipe.
    pub support_filter_pipe: bool,
    /// Whether the legacy `for … = … to …` loop form is accepted.
    pub legacy_for: bool,
}

/// Default T1 language config.
pub fn default_language() -> Language {
    Language {
        syntax_mode: "template".into(),
        template: TemplateConfig {
            code_start: "<%".into(),
            code_end: "%>".into(),
            comment_start: "<#".into(),
            comment_end: "#>".into(),
            support_filter_pipe: true,
            legacy_for: false,
        },
    }
}

/// IR node (structural / statement / expression).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IrNode {
    /// Root node; `children` render in order.
    Template {
        /// Top-level child nodes, rendered in source order.
        children: Vec<IrNode>,
        /// Source span of the whole template, when the producer kept one.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Literal text emitted verbatim (already HTML-escaped as authored).
    Text {
        /// Text content written to the output unchanged.
        value: String,
        /// Source span of the text run, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Template comment; the body is never rendered.
    Comment {
        /// Comment body carried for tooling only.
        value: String,
        /// Source span of the comment tag, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `{{ expression }}` — value interpolation.
    Interpolation {
        /// Expression whose value is stringified into the output.
        expression: Box<IrNode>,
        /// Trim level from the tag marker: `none`, `ws`, `nl`, `ws_nl` or `all`.
        trim: String,
        /// When true the value is written without HTML escaping.
        #[serde(default, skip_serializing_if = "is_false")]
        raw: bool,
        /// Source span of the interpolation, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `if test` … `else if` … `else` … `end if`.
    #[serde(rename = "Stmt.If")]
    StmtIf {
        /// Condition evaluated before `consequent`.
        test: Box<IrNode>,
        /// Nodes rendered when `test` is truthy.
        consequent: Vec<IrNode>,
        /// `else if` branches, tried in order after `test` fails.
        #[serde(rename = "elseIfs")]
        else_ifs: Vec<IrNode>,
        /// `else` body; `None` when the statement has no `else` branch.
        #[serde(skip_serializing_if = "Option::is_none")]
        alternate: Option<Vec<IrNode>>,
        /// Trim level of the opening tag.
        trim: String,
        /// Source span of the statement, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Inline `partial` declaration with its own parameter scope.
    #[serde(rename = "Stmt.Partial")]
    StmtPartial {
        /// Declared parameters; missing arguments fall back to their defaults.
        params: Vec<PartialParam>,
        /// Body rendered in the parameter scope.
        body: Vec<IrNode>,
        /// Trim level of the opening tag.
        trim: String,
        /// Source span of the declaration, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `macro name(params)` — a named template-local component. Invoked through
    /// a bare-identifier [`IrNode::ExprCall`]; its body sees its parameters
    /// only, never the caller scope.
    #[serde(rename = "Stmt.Macro")]
    StmtMacro {
        /// Macro name bound in the declaring template's scope.
        name: String,
        /// Declared parameters; missing arguments fall back to their defaults.
        params: Vec<PartialParam>,
        /// Body rendered with only the macro parameters in scope.
        body: Vec<IrNode>,
        /// Trim level of the opening tag.
        trim: String,
        /// Source span of the declaration, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `from "<path>" import a, b as c` — static macro import.
    ///
    /// The path must be a string literal so the dependency graph is fully
    /// static for both JIT loading and AOT catalog emission.
    #[serde(rename = "Stmt.Import")]
    StmtImport {
        /// Literal template path the macros are imported from.
        path: Box<IrNode>,
        /// Imported macro names with their optional local aliases.
        names: Vec<ImportName>,
        /// Trim level of the import tag.
        trim: String,
        /// Source span of the statement, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `call name(args)` … `end call` — invokes a macro with a caller block.
    ///
    /// The block is exposed to the macro body as `caller()` and renders in the
    /// calling template's scope, not the macro's. `params` are the caller
    /// block's formal parameters, declared as `call(a, b) name(args)`.
    #[serde(rename = "Stmt.Call")]
    StmtCall {
        /// Name of the macro being invoked.
        name: String,
        /// Positional arguments passed to the macro.
        arguments: Vec<IrNode>,
        /// Formal parameters of the caller block, exposed via `caller()`.
        #[serde(default)]
        params: Vec<PartialParam>,
        /// Caller block rendered in the calling template's scope.
        body: Vec<IrNode>,
        /// Trim level of the opening tag.
        trim: String,
        /// Source span of the call, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `else if test` branch nested inside [`IrNode::StmtIf`].
    #[serde(rename = "Stmt.ElseIf")]
    StmtElseIf {
        /// Condition evaluated before `consequent`.
        test: Box<IrNode>,
        /// Nodes rendered when `test` is truthy.
        consequent: Vec<IrNode>,
        /// Trim level of the `else if` tag.
        trim: String,
        /// Source span of the branch, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `loop item in iterable` — body iteration.
    ///
    /// Loop metadata (`index`, `index0`, `first`, `last`, `length`, `depth`)
    /// is bound implicitly as the `forloop` object inside every body
    /// iteration; it is not part of this node's shape.
    #[serde(rename = "Stmt.For")]
    StmtFor {
        /// Loop variable bound to the current element.
        item: String,
        /// Expression producing the sequence to iterate.
        iterable: Box<IrNode>,
        /// Body rendered once per element.
        body: Vec<IrNode>,
        /// Trim level of the opening tag.
        trim: String,
        /// Source span of the loop, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Named `block` overridden by a child template via `extends`.
    #[serde(rename = "Stmt.Block")]
    StmtBlock {
        /// Block name matched against the parent template's block.
        name: String,
        /// Default body used when no child overrides this block.
        body: Vec<IrNode>,
        /// Trim level of the opening tag.
        trim: String,
        /// Source span of the block, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `extends "<path>"` — single parent template.
    #[serde(rename = "Stmt.Extends")]
    StmtExtends {
        /// Literal path of the parent template.
        parent: Box<IrNode>,
        /// Trim level of the `extends` tag.
        trim: String,
        /// Source span of the statement, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `include "<path>"` — render another template in place.
    #[serde(rename = "Stmt.Include")]
    StmtInclude {
        /// Template path to include; must resolve through the loader.
        path: Box<IrNode>,
        /// Explicit child context merged over the inherited one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        with: Option<Box<IrNode>>,
        /// `only` / `without context`: the child sees no inherited context.
        #[serde(default, skip_serializing_if = "is_false")]
        only: bool,
        /// Trim level of the include tag.
        trim: String,
        /// Source span of the statement, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `super()` — inlines the parent template's version of the current block.
    #[serde(rename = "Stmt.Super")]
    StmtSuper {
        /// Trim level of the `super` tag.
        trim: String,
        /// Source span of the call, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `break` — terminates the nearest enclosing loop in the same lexical unit.
    #[serde(rename = "Stmt.Break")]
    StmtBreak {
        /// Trim level of the `break` tag.
        trim: String,
        /// Source span of the statement, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `continue` — skips the rest of the bound loop's current iteration.
    #[serde(rename = "Stmt.Continue")]
    StmtContinue {
        /// Trim level of the `continue` tag.
        trim: String,
        /// Source span of the statement, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Raw section; the body is emitted without evaluation.
    #[serde(rename = "Stmt.Raw")]
    StmtRaw {
        /// Verbatim text emitted without parsing or escaping.
        value: String,
        /// Trim level of the `raw` tag.
        trim: String,
        /// Source span of the section, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Literal expression; the JSON value is used as-is.
    #[serde(rename = "Expr.Literal")]
    ExprLiteral {
        /// JSON literal value of the expression.
        value: serde_json::Value,
        /// Source span of the literal, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Bare identifier; resolved against the render context and locals.
    #[serde(rename = "Expr.Identifier")]
    ExprIdentifier {
        /// Identifier text looked up at render time.
        name: String,
        /// Source span of the identifier, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `object.property` member access.
    #[serde(rename = "Expr.Member")]
    ExprMember {
        /// Expression producing the object being read.
        object: Box<IrNode>,
        /// Property name to read from the object.
        property: String,
        /// Source span of the access, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `object[index]` indexed access.
    #[serde(rename = "Expr.Index")]
    ExprIndex {
        /// Expression producing the object or sequence being indexed.
        object: Box<IrNode>,
        /// Expression producing the key or position.
        index: Box<IrNode>,
        /// Source span of the access, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `callee(args…)` — function, macro or `caller()` invocation.
    #[serde(rename = "Expr.Call")]
    ExprCall {
        /// Expression identifying what is called.
        callee: Box<IrNode>,
        /// Positional arguments passed to the callee.
        arguments: Vec<IrNode>,
        /// Source span of the call, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `expression is [not] test(args)` — predicate from the `test` namespace.
    #[serde(rename = "Expr.Test")]
    ExprTest {
        /// Expression the predicate is applied to.
        expression: Box<IrNode>,
        /// Name of the test in the built-in `test` namespace.
        test: String,
        /// Optional arguments passed to the test.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        arguments: Vec<IrNode>,
        /// Whether the result is inverted (`is not`).
        #[serde(default, skip_serializing_if = "is_false")]
        negated: bool,
        /// Source span of the test, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Binary operator application.
    #[serde(rename = "Expr.Binary")]
    ExprBinary {
        /// Operator token, e.g. `==`, `and` or `~`.
        operator: String,
        /// Left-hand operand.
        left: Box<IrNode>,
        /// Right-hand operand.
        right: Box<IrNode>,
        /// Source span of the expression, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Unary operator application (`not`, `-`).
    #[serde(rename = "Expr.Unary")]
    ExprUnary {
        /// Operator token, e.g. `not` or `-`.
        operator: String,
        /// Operand the operator is applied to.
        argument: Box<IrNode>,
        /// Source span of the expression, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// Object literal `{ key: value, … }`.
    #[serde(rename = "Expr.Object")]
    ExprObject {
        /// Entries in source order; later keys do not override earlier ones.
        entries: Vec<ObjectEntry>,
        /// Source span of the literal, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
    /// `expression | filter(args…)` — filter pipeline stage.
    #[serde(rename = "Expr.Pipe")]
    ExprPipe {
        /// Input expression passed to the filter.
        expression: Box<IrNode>,
        /// Name of the built-in filter to apply.
        filter: String,
        /// Extra arguments passed after the piped value.
        arguments: Vec<IrNode>,
        /// Source span of the pipe, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
    },
}

/// One `partial` / `macro` parameter: name plus an optional literal default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartialParam {
    /// Parameter name bound in the callee scope.
    pub name: String,
    /// Default expression used when the caller omits the argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<IrNode>,
}

/// One imported name: the macro declared by the target template plus the
/// local name it is bound to (`as` alias when present).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportName {
    /// Macro name as declared by the target template.
    pub name: String,
    /// Local alias bound in the importing template, when `as` was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl ImportName {
    /// Local name this import binds in the importing template.
    pub fn local(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// One `key: value` entry of an object literal expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectEntry {
    /// Property key written in the literal.
    pub key: String,
    /// Expression producing the property value.
    pub value: IrNode,
}

/// Source span in UTF-8 byte offsets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// Byte offset of the first byte (`0`-based).
    pub start: usize,
    /// Byte offset one past the last byte (exclusive).
    pub end: usize,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn deserialize_ir_version<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value == IR_VERSION {
        Ok(value)
    }
    else {
        Err(serde::de::Error::custom(format!("unsupported irVersion {value:?}, expected {IR_VERSION:?}")))
    }
}
