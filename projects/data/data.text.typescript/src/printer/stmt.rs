//! TypeScript statement → canonical text.

use crate::ast::{
    BlockStatement, ClassDeclaration, ClassMember, Decorator, DoWhileStatement, EnumDeclaration, ExportDeclaration, ExportSpecifier,
    FunctionDeclaration, ImportDeclaration, ImportSpecifier, InterfaceDeclaration, ModifierKind, NamespaceDeclaration, Statement, SwitchCase,
    SwitchStatement, TryStatement, TypeAliasDeclaration, VariableDeclaration, VariableKind, Visibility,
};

use super::expr::{print_expression, print_string, print_type_annotation, print_type_params};

/// Print one top-level or nested statement.
pub fn print_statement(stmt: &Statement) -> Option<String> {
    match stmt {
        Statement::VariableDeclaration(v) => print_variable(v),
        Statement::ExpressionStatement(es) => print_expression(&es.expression),
        Statement::ReturnStatement(r) => {
            let arg = match &r.argument {
                Some(e) => Some(print_expression(e)?),
                None => None,
            };
            Some(match arg {
                Some(e) => format!("return {e}"),
                None => "return".into(),
            })
        }
        Statement::ImportDeclaration(d) => print_import(d),
        Statement::ExportDeclaration(d) => print_export(d),
        Statement::FunctionDeclaration(d) => print_function(d),
        Statement::TypeAlias(d) => print_type_alias(d),
        Statement::BlockStatement(b) => print_block(b),
        Statement::IfStatement(s) => {
            let test = print_expression(&s.test)?;
            let cons = print_branch(&s.consequent)?;
            let alt = match &s.alternate {
                Some(a) => format!(" else {}", print_branch(a)?),
                None => String::new(),
            };
            Some(format!("if ({test}) {cons}{alt}"))
        }
        Statement::WhileStatement(s) => {
            let test = print_expression(&s.test)?;
            let body = print_branch(&s.body)?;
            Some(format!("while ({test}) {body}"))
        }
        Statement::DoWhileStatement(s) => print_do_while(s),
        Statement::TryStatement(s) => print_try(s),
        Statement::ThrowStatement(s) => Some(format!("throw {}", print_expression(&s.argument)?)),
        Statement::BreakStatement(s) => Some(match s.label.as_deref() {
            Some(label) => format!("break {label}"),
            None => "break".into(),
        }),
        Statement::ContinueStatement(s) => Some(match s.label.as_deref() {
            Some(label) => format!("continue {label}"),
            None => "continue".into(),
        }),
        Statement::ClassDeclaration(d) => print_class(d),
        Statement::ForStatement(s) => print_for(s),
        Statement::ForInStatement(s) => print_for_in(s),
        Statement::ForOfStatement(s) => print_for_of(s),
        Statement::SwitchStatement(s) => print_switch(s),
        Statement::Enum(d) => print_enum(d),
        Statement::Interface(d) => print_interface(d),
        Statement::Namespace(d) => print_namespace(d),
    }
}

/// 把非块语句包进 `{}` 作为分支/循环体,保证输出始终是可解析的合法文本。
fn print_branch(stmt: &Statement) -> Option<String> {
    match stmt {
        Statement::BlockStatement(b) => print_block(b),
        other => Some(format!("{{ {} }}", print_statement(other)?)),
    }
}

/// 打印装饰器序列,返回形如 `@expr1 @expr2 ` 的前缀(含尾随空格)。
fn print_decorators(decorators: &[Decorator]) -> Option<String> {
    let mut out = String::new();
    for decorator in decorators {
        let expr = print_expression(&decorator.expression)?;
        out.push_str(&format!("@{expr} "));
    }
    Some(out)
}

fn print_type_alias(d: &TypeAliasDeclaration) -> Option<String> {
    if !d.decorators.is_empty() {
        return None;
    }
    let declare_kw = if d.is_declare() { "declare " } else { "" };
    let type_params = print_type_params(&d.type_params)?;
    let ty = print_type_annotation(&d.ty)?;
    Some(format!("{declare_kw}type {}{type_params} = {ty};", d.name))
}

fn print_do_while(s: &DoWhileStatement) -> Option<String> {
    let body = print_branch(&s.body)?;
    let test = print_expression(&s.test)?;
    Some(format!("do {body} while ({test})"))
}

fn print_try(s: &TryStatement) -> Option<String> {
    let mut output = format!("try {}", print_block_body(&s.block)?);
    if let Some(handler) = &s.handler {
        let param = handler.param.as_deref().map(|name| format!(" ({name})")).unwrap_or_default();
        output.push_str(&format!(" catch{param} {}", print_block_body(&handler.body)?));
    }
    if let Some(finalizer) = &s.finalizer {
        output.push_str(&format!(" finally {}", print_block_body(finalizer)?));
    }
    Some(output)
}

fn print_variable(v: &VariableDeclaration) -> Option<String> {
    if !v.decorators.is_empty() || v.is_declare() {
        return None;
    }
    let kind = match v.kind {
        VariableKind::Const => "const",
        VariableKind::Let => "let",
        VariableKind::Var => "var",
    };
    let mut parts = Vec::with_capacity(v.declarators.len());
    for declarator in &v.declarators {
        let binding = match &declarator.pattern_text {
            Some(text) => text.clone(),
            None => declarator.binding_names.first().cloned().unwrap_or_default(),
        };
        let init = match &declarator.value {
            Some(e) => format!(" = {}", print_expression(e)?),
            None => String::new(),
        };
        let ty = match &declarator.ty {
            Some(t) => format!(": {}", print_type_annotation(t)?),
            None => String::new(),
        };
        parts.push(format!("{binding}{ty}{init}"));
    }
    Some(format!("{kind} {}", parts.join(", ")))
}

fn print_import(d: &ImportDeclaration) -> Option<String> {
    let type_kw = if d.is_type_only() { "type " } else { "" };
    let module = print_string(&d.module_specifier.value);
    let attrs = d.attributes.as_ref().map(|text| format!(" {text}")).unwrap_or_default();
    if d.specifiers.is_empty() {
        return Some(format!("import {type_kw}{module}{attrs};"));
    }
    let mut parts = Vec::with_capacity(d.specifiers.len());
    for spec in &d.specifiers {
        parts.push(match spec {
            ImportSpecifier::Default { local, .. } => local.clone(),
            ImportSpecifier::Namespace { local, .. } => format!("* as {local}"),
            ImportSpecifier::Named { local, imported, type_modifier, .. } => {
                let type_kw = if type_modifier.is_some() { "type " } else { "" };
                if local == imported { format!("{type_kw}{local}") } else { format!("{type_kw}{imported} as {local}") }
            }
        });
    }
    Some(format!("import {type_kw}{{ {} }} from {module}{attrs};", parts.join(", ")))
}

fn print_export(d: &ExportDeclaration) -> Option<String> {
    if !d.specifiers.is_empty() {
        let source = d.source.as_ref().map(|value| format!(" from {}", print_string(&value.value))).unwrap_or_default();
        let attrs = d.attributes.as_ref().map(|text| format!(" {text}")).unwrap_or_default();
        let type_kw = if d.is_type_only() { "type " } else { "" };
        if d.specifiers.len() == 1 {
            match &d.specifiers[0] {
                ExportSpecifier::All { .. } => return Some(format!("export {type_kw}*{source}{attrs};")),
                ExportSpecifier::Namespace { exported, .. } => return Some(format!("export {type_kw}* as {exported}{source}{attrs};")),
                ExportSpecifier::Named { .. } => {}
            }
        }
        let mut parts = Vec::with_capacity(d.specifiers.len());
        for specifier in &d.specifiers {
            let ExportSpecifier::Named { local, exported, type_modifier, .. } = specifier
            else {
                return None;
            };
            let type_kw = if type_modifier.is_some() { "type " } else { "" };
            let name = if local == exported { local.clone() } else { format!("{local} as {exported}") };
            parts.push(format!("{type_kw}{name}"));
        }
        return Some(format!("export {type_kw}{{ {} }}{source}{attrs};", parts.join(", ")));
    }
    if d.source.is_some() || d.is_type_only() {
        return None;
    }
    let default_kw = if d.is_default() { "default " } else { "" };
    let decl = d.declaration.as_ref()?;
    let body = print_statement(decl)?;
    Some(format!("export {default_kw}{body}"))
}

fn print_function(d: &FunctionDeclaration) -> Option<String> {
    if !d.decorators.is_empty() || d.is_declare() {
        return None;
    }
    let params = print_function_params(&d.params)?;
    let type_params = print_type_params(&d.type_params)?;
    let ret = match &d.return_type {
        Some(t) => format!(": {}", print_type_annotation(t)?),
        None => String::new(),
    };
    let body = print_block_body(&d.body)?;
    Some(format!("function {}{type_params}{params}{ret} {body}", d.name))
}

fn print_function_params(params: &[crate::ast::FunctionParam]) -> Option<String> {
    super::expr::print_params(params)
}

fn print_class(d: &ClassDeclaration) -> Option<String> {
    if d.is_declare() || d.is_abstract() {
        return None;
    }
    let decorators = print_decorators(&d.decorators)?;
    let type_params = print_type_params(&d.type_params)?;
    let extends = match d.extends.as_ref() {
        Some(ty) => format!(" extends {}", print_type_annotation(ty)?),
        None => String::new(),
    };
    let implements = if d.implements.is_empty() {
        String::new()
    }
    else {
        let names = d.implements.iter().map(print_type_annotation).collect::<Option<Vec<_>>>()?;
        format!(" implements {}", names.join(", "))
    };
    let mut members = Vec::with_capacity(d.body.len());
    for member in &d.body {
        members.push(print_class_member(member)?);
    }
    let body = if members.is_empty() { String::new() } else { format!(" {} ", members.join(" ")) };
    let name = if d.name.is_empty() { String::new() } else { format!(" {}", d.name) };
    Some(format!("{decorators}class{name}{type_params}{extends}{implements} {{{body}}}"))
}

pub(super) fn print_class_member(member: &ClassMember) -> Option<String> {
    match member {
        ClassMember::Property { decorators, name, ty, initializer, visibility, optional_marker, .. } => {
            if member.has_modifier(ModifierKind::Abstract) {
                return None;
            }
            let decorators = print_decorators(decorators)?;
            let static_kw = if member.has_modifier(ModifierKind::Static) { "static " } else { "" };
            let readonly_kw = if member.has_modifier(ModifierKind::Readonly) { "readonly " } else { "" };
            let visibility = print_visibility(visibility.as_ref());
            let optional = if optional_marker.is_some() { "?" } else { "" };
            let ty = match ty.as_ref() {
                Some(t) => format!(": {}", print_type_annotation(t)?),
                None => String::new(),
            };
            let initializer = match initializer.as_ref() {
                Some(expr) => format!(" = {}", print_expression(expr)?),
                None => String::new(),
            };
            Some(format!("{decorators}{visibility}{static_kw}{readonly_kw}{name}{optional}{ty}{initializer};"))
        }
        ClassMember::Method { decorators, name, type_params, params, return_type, body, visibility, optional_marker, .. } => {
            if member.has_modifier(ModifierKind::Abstract) || optional_marker.is_some() {
                return None;
            }
            let decorators = print_decorators(decorators)?;
            let static_kw = if member.has_modifier(ModifierKind::Static) { "static " } else { "" };
            let async_kw = if member.has_modifier(ModifierKind::Async) { "async " } else { "" };
            let accessor = if member.has_modifier(ModifierKind::Get) {
                "get "
            }
            else if member.has_modifier(ModifierKind::Set) {
                "set "
            }
            else {
                ""
            };
            let visibility = print_visibility(visibility.as_ref());
            let type_params = print_type_params(type_params)?;
            let params = print_function_params(params)?;
            let ret = match return_type.as_ref() {
                Some(t) => format!(": {}", print_type_annotation(t)?),
                None => String::new(),
            };
            let body = print_block_body(body)?;
            Some(format!("{decorators}{visibility}{static_kw}{async_kw}{accessor}{name}{type_params}{params}{ret} {body}"))
        }
    }
}

/// 打印成员可见性修饰词,无显式可见性时返回空字符串。
fn print_visibility(visibility: Option<&Visibility>) -> &'static str {
    match visibility {
        Some(Visibility::Public) => "public ",
        Some(Visibility::Private) => "private ",
        Some(Visibility::Protected) => "protected ",
        None => "",
    }
}

fn print_for(s: &crate::ast::ForStatement) -> Option<String> {
    let init = match s.initializer.as_deref() {
        Some(stmt) => print_statement(stmt)?,
        None => String::new(),
    };
    let test = match s.test.as_ref() {
        Some(expr) => print_expression(expr)?,
        None => String::new(),
    };
    let update = match s.incrementor.as_ref() {
        Some(expr) => print_expression(expr)?,
        None => String::new(),
    };
    let body = print_branch(&s.body)?;
    Some(format!("for ({init}; {test}; {update}) {body}"))
}

fn print_for_in(s: &crate::ast::ForInStatement) -> Option<String> {
    let left = print_for_binding(&s.left)?;
    let right = print_expression(&s.right)?;
    let body = print_branch(&s.body)?;
    Some(format!("for ({left} in {right}) {body}"))
}

fn print_for_of(s: &crate::ast::ForOfStatement) -> Option<String> {
    let left = print_for_binding(&s.left)?;
    let right = print_expression(&s.right)?;
    let body = print_branch(&s.body)?;
    let await_kw = if s.is_await() { "await " } else { "" };
    Some(format!("for {await_kw}({left} of {right}) {body}"))
}

/// 打印 `for-in` / `for-of` 头部左侧绑定(声明或目标表达式)。
fn print_for_binding(left: &Statement) -> Option<String> {
    match left {
        Statement::VariableDeclaration(v) => print_variable(v),
        Statement::ExpressionStatement(es) => print_expression(&es.expression),
        _ => print_statement(left),
    }
}

fn print_switch(s: &SwitchStatement) -> Option<String> {
    let discriminant = print_expression(&s.discriminant)?;
    let mut parts = Vec::with_capacity(s.cases.len());
    for case in &s.cases {
        parts.push(print_switch_case(case)?);
    }
    let body = if parts.is_empty() { String::new() } else { format!(" {} ", parts.join(" ")) };
    Some(format!("switch ({discriminant}) {{{body}}}"))
}

fn print_switch_case(case: &SwitchCase) -> Option<String> {
    let head = match case.test.as_ref() {
        Some(test) => format!("case {}: ", print_expression(test)?),
        None => "default: ".to_string(),
    };
    let mut parts = Vec::with_capacity(case.consequent.len());
    for stmt in &case.consequent {
        parts.push(print_statement(stmt)?);
    }
    if parts.is_empty() {
        return Some(head.trim_end().to_string());
    }
    Some(format!("{head}{};", parts.join("; ")))
}

fn print_enum(d: &EnumDeclaration) -> Option<String> {
    if !d.decorators.is_empty() {
        return None;
    }
    let declare_kw = if d.is_declare() { "declare " } else { "" };
    let mut parts = Vec::with_capacity(d.members.len());
    for member in &d.members {
        let initializer = match member.initializer.as_ref() {
            Some(expr) => format!(" = {}", print_expression(expr)?),
            None => String::new(),
        };
        parts.push(format!("{}{initializer}", member.name));
    }
    let body = if parts.is_empty() { String::new() } else { format!(" {} ", parts.join(", ")) };
    Some(format!("{declare_kw}enum {} {{{body}}};", d.name))
}

fn print_interface(d: &InterfaceDeclaration) -> Option<String> {
    if !d.decorators.is_empty() {
        return None;
    }
    let declare_kw = if d.is_declare() { "declare " } else { "" };
    let type_params = print_type_params(&d.type_params)?;
    let extends = if d.extends.is_empty() {
        String::new()
    }
    else {
        format!(" extends {}", d.extends.iter().map(print_type_annotation).collect::<Option<Vec<_>>>()?.join(", "))
    };
    let mut members = Vec::with_capacity(d.body.len());
    for member in &d.body {
        members.push(print_member_signature(member)?);
    }
    let body = if members.is_empty() { String::new() } else { format!(" {} ", members.join(" ")) };
    Some(format!("{declare_kw}interface {}{type_params}{extends} {{{body}}}", d.name))
}

/// 打印接口/对象类型成员签名(属性带类型、方法不带实现体)。
pub(super) fn print_member_signature(member: &ClassMember) -> Option<String> {
    match member {
        ClassMember::Property { decorators, name, ty, optional_marker, .. } => {
            if !decorators.is_empty() {
                return None;
            }
            let optional = if optional_marker.is_some() { "?" } else { "" };
            let ty = match ty.as_ref() {
                Some(t) => format!(": {}", print_type_annotation(t)?),
                None => String::new(),
            };
            Some(format!("{name}{optional}{ty};"))
        }
        ClassMember::Method { decorators, name, type_params, params, return_type, optional_marker, .. } => {
            if !decorators.is_empty() {
                return None;
            }
            let optional = if optional_marker.is_some() { "?" } else { "" };
            let type_params = print_type_params(type_params)?;
            let params = print_function_params(params)?;
            let ret = match return_type.as_ref() {
                Some(t) => format!(": {}", print_type_annotation(t)?),
                None => String::new(),
            };
            Some(format!("{name}{optional}{type_params}{params}{ret};"))
        }
    }
}

fn print_namespace(d: &NamespaceDeclaration) -> Option<String> {
    if !d.decorators.is_empty() {
        return None;
    }
    let declare_kw = if d.is_declare() { "declare " } else { "" };
    let mut parts = Vec::with_capacity(d.body.len());
    for stmt in &d.body {
        parts.push(print_statement(stmt)?);
    }
    let body = if parts.is_empty() { String::new() } else { format!(" {} ", parts.join("; ")) };
    Some(format!("{declare_kw}namespace {} {{{body}}}", d.name))
}

fn print_block(b: &BlockStatement) -> Option<String> {
    print_block_body(&b.statements)
}

pub(super) fn print_block_body(statements: &[Statement]) -> Option<String> {
    let mut parts = Vec::with_capacity(statements.len());
    for s in statements {
        parts.push(print_statement(s)?);
    }
    Some(format!("{{ {} }}", parts.join("; ")))
}
