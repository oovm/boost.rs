//! GLSL input: `data-text-glsl` syntax tree to neutral shader syntax.
#![warn(missing_docs)]

use data_text_glsl::ast as g;
use hardware_frontend::{
    FrontendError, LoweredShader,
    ast::{
        AccessMode, AddressSpace, AssignOp, Attribute, BinaryOp, ConstDef, Expr, FunctionDef, GlobalVar, ModuleItem, Param, ShaderModule, Stmt,
        StructDef, StructField, TypeAlias, TypeRef, UnaryOp,
    },
    lower_module,
};

/// Name of the synthetic parameter that carries `gl_GlobalInvocationID`.
const GLOBAL_INVOCATION: &str = "__global_invocation_id";

/// Parses GLSL text and lowers it into UCF IR.
pub fn lower_glsl(source: &str) -> Result<LoweredShader, FrontendError> {
    let root = data_text_glsl::parse_glsl(source).map_err(FrontendError::Parse)?;
    lower_module(&convert_module(&root)?)
}

/// Parses GLSL text into the language-neutral shader module.
pub fn parse_glsl_module(source: &str) -> Result<ShaderModule, FrontendError> {
    let root = data_text_glsl::parse_glsl(source).map_err(FrontendError::Parse)?;
    convert_module(&root)
}

fn unsupported(message: impl Into<String>) -> FrontendError {
    FrontendError::Unsupported(message.into())
}

struct Converter {
    workgroup: [i64; 3],
}

fn convert_module(root: &g::GlslRoot) -> Result<ShaderModule, FrontendError> {
    let mut converter = Converter { workgroup: [1, 1, 1] };
    for item in &root.items {
        if let g::GlslItem::GlobalVar(global) = item {
            converter.absorb_execution_mode(global);
        }
    }
    let mut items = Vec::new();
    for item in &root.items {
        if matches!(item, g::GlslItem::GlobalVar(global) if global.name.is_empty()) {
            continue;
        }
        if let g::GlslItem::GlobalVar(global) = item
            && is_uniform_block(global)
        {
            items.push(ModuleItem::Struct(uniform_block_struct(global)));
        }
        items.push(converter.convert_item(item)?);
    }
    Ok(ShaderModule { items })
}

/// Returns `true` for a `uniform` interface block with scalar members.
fn is_uniform_block(global: &g::GlslGlobalVar) -> bool {
    global.qualifiers.contains(&g::GlslQualifier::Uniform) && !global.members.is_empty()
}

/// Builds the struct that backs a uniform block so the neutral lowering can
/// address each member through the block name.
fn uniform_block_struct(global: &g::GlslGlobalVar) -> StructDef {
    StructDef {
        name: global.name.clone(),
        fields: global
            .members
            .iter()
            .map(|member| StructField {
                attributes: layout_attributes(&member.layout),
                name: member.name.clone(),
                ty: convert_type(&member.ty),
            })
            .collect(),
    }
}

impl Converter {
    fn absorb_execution_mode(&mut self, global: &g::GlslGlobalVar) {
        if !global.name.is_empty() {
            return;
        }
        for layout in &global.layout {
            let slot = match layout.name.as_str() {
                "local_size_x" => 0,
                "local_size_y" => 1,
                "local_size_z" => 2,
                _ => continue,
            };
            if let Some(value) = layout.value.as_ref().and_then(layout_integer) {
                self.workgroup[slot] = value;
            }
        }
    }

    fn convert_item(&self, item: &g::GlslItem) -> Result<ModuleItem, FrontendError> {
        match item {
            g::GlslItem::Function(function) => Ok(ModuleItem::Function(self.convert_function(function)?)),
            g::GlslItem::Struct(structure) => Ok(ModuleItem::Struct(StructDef {
                name: structure.name.clone(),
                fields: structure
                    .members
                    .iter()
                    .map(|member| StructField {
                        attributes: layout_attributes(&member.layout),
                        name: member.name.clone(),
                        ty: convert_type(&member.ty),
                    })
                    .collect(),
            })),
            g::GlslItem::GlobalVar(global) => self.convert_global(global),
            g::GlslItem::Const(constant) => Ok(ModuleItem::Const(ConstDef {
                name: constant.name.clone(),
                ty: constant.ty.as_ref().map(convert_type),
                init: convert_expr(&constant.init)?,
            })),
            g::GlslItem::TypeAlias(alias) => Ok(ModuleItem::TypeAlias(TypeAlias { name: alias.name.clone(), ty: convert_type(&alias.ty) })),
        }
    }

    fn convert_global(&self, global: &g::GlslGlobalVar) -> Result<ModuleItem, FrontendError> {
        if global.qualifiers.contains(&g::GlslQualifier::Uniform) {
            return self.convert_uniform(global);
        }
        if !global.qualifiers.contains(&g::GlslQualifier::Buffer) {
            return Err(unsupported(format!("interface variable `{}`", global.name)));
        }
        let member = match global.members.as_slice() {
            [member] => member,
            _ => return Err(unsupported(format!("interface block `{}` with multiple members", global.name))),
        };
        let access = if global.qualifiers.contains(&g::GlslQualifier::Readonly) {
            AccessMode::Read
        }
        else if global.qualifiers.contains(&g::GlslQualifier::Writeonly) {
            AccessMode::Write
        }
        else {
            AccessMode::ReadWrite
        };
        let binding = global
            .layout
            .iter()
            .find(|layout| layout.name == "binding")
            .and_then(|layout| layout.value.as_ref())
            .and_then(layout_integer)
            .unwrap_or(0);
        Ok(ModuleItem::GlobalVar(GlobalVar {
            attributes: vec![
                Attribute { name: "group".into(), args: vec![Expr::IntegerLiteral(0)] },
                Attribute { name: "binding".into(), args: vec![Expr::IntegerLiteral(binding)] },
            ],
            address_space: Some(AddressSpace::Storage),
            access: Some(access),
            name: member.name.clone(),
            ty: Some(TypeRef { name: "array".into(), args: vec![convert_type(&member.ty)] }),
            init: None,
        }))
    }

    /// Maps a `uniform` declaration or interface block to a read-only uniform
    /// binding. A block is typed by the struct synthesized for its members.
    fn convert_uniform(&self, global: &g::GlslGlobalVar) -> Result<ModuleItem, FrontendError> {
        let binding = global
            .layout
            .iter()
            .find(|layout| layout.name == "binding")
            .and_then(|layout| layout.value.as_ref())
            .and_then(layout_integer)
            .unwrap_or(0);
        let ty = match global.ty.as_ref() {
            Some(ty) => convert_type(ty),
            None if is_uniform_block(global) => TypeRef { name: global.name.clone(), args: Vec::new() },
            None => return Err(unsupported(format!("uniform `{}` has no type", global.name))),
        };
        Ok(ModuleItem::GlobalVar(GlobalVar {
            attributes: vec![
                Attribute { name: "group".into(), args: vec![Expr::IntegerLiteral(0)] },
                Attribute { name: "binding".into(), args: vec![Expr::IntegerLiteral(binding)] },
            ],
            address_space: Some(AddressSpace::Uniform),
            access: Some(AccessMode::Read),
            name: global.name.clone(),
            ty: Some(ty),
            init: None,
        }))
    }

    fn convert_function(&self, function: &g::GlslFunction) -> Result<FunctionDef, FrontendError> {
        let mut attributes = if function.name == "main" {
            vec![
                Attribute { name: "compute".into(), args: Vec::new() },
                Attribute { name: "workgroup_size".into(), args: self.workgroup.iter().map(|value| Expr::IntegerLiteral(*value)).collect() },
            ]
        }
        else {
            Vec::new()
        };
        attributes.extend(layout_attributes(&function.layout));
        let mut params: Vec<Param> = function.params.iter().map(|param| self.convert_param(param)).collect::<Result<_, _>>()?;
        let body: Vec<Stmt> = function.body.iter().map(convert_statement).collect::<Result<_, _>>()?;
        if uses_global_invocation_id(&body) {
            params.push(Param {
                attributes: vec![Attribute { name: "builtin".into(), args: vec![Expr::Ident("global_invocation_id".into())] }],
                name: GLOBAL_INVOCATION.into(),
                ty: TypeRef { name: "vec3".into(), args: vec![TypeRef { name: "u32".into(), args: Vec::new() }] },
            });
        }
        Ok(FunctionDef {
            attributes,
            name: function.name.clone(),
            params,
            return_type: function.return_type.as_ref().filter(|ty| ty.name != "void").map(convert_type),
            body,
        })
    }

    fn convert_param(&self, param: &g::GlslParam) -> Result<Param, FrontendError> {
        Ok(Param { attributes: Vec::new(), name: param.name.clone(), ty: convert_type(&param.ty) })
    }
}

fn layout_attributes(layout: &[g::GlslLayoutQualifier]) -> Vec<Attribute> {
    layout
        .iter()
        .filter_map(|qualifier| {
            let value = qualifier.value.as_ref().and_then(|value| convert_expr(value).ok());
            Some(Attribute { name: qualifier.name.clone(), args: value.into_iter().collect() })
        })
        .collect()
}

fn layout_integer(value: &g::GlslExpression) -> Option<i64> {
    match value {
        g::GlslExpression::IntegerLiteral(value) => Some(*value),
        g::GlslExpression::Paren(inner) => layout_integer(inner),
        _ => None,
    }
}

fn convert_type(ty: &g::GlslType) -> TypeRef {
    TypeRef { name: convert_type_name(&ty.name), args: ty.args.iter().map(convert_type).collect() }
}

fn convert_type_name(name: &str) -> String {
    match name {
        "float" => "f32",
        "double" => "f64",
        "uint" => "u32",
        "int" => "i32",
        "bool" => "bool",
        other => other,
    }
    .to_owned()
}

fn uses_global_invocation_id(body: &[Stmt]) -> bool {
    body.iter().any(statement_uses_global_invocation_id)
}

fn statement_uses_global_invocation_id(statement: &Stmt) -> bool {
    match statement {
        Stmt::Var { init, .. } => init.as_ref().is_some_and(expr_uses_global_invocation_id),
        Stmt::Assign { target, value, .. } => expr_uses_global_invocation_id(target) || expr_uses_global_invocation_id(value),
        Stmt::If { cond, then_body, else_body } => {
            expr_uses_global_invocation_id(cond) || uses_global_invocation_id(then_body) || uses_global_invocation_id(else_body)
        }
        Stmt::For { init, cond, update, body } => {
            init.as_ref().is_some_and(|statement| statement_uses_global_invocation_id(statement))
                || cond.as_ref().is_some_and(expr_uses_global_invocation_id)
                || update.as_ref().is_some_and(|statement| statement_uses_global_invocation_id(statement))
                || uses_global_invocation_id(body)
        }
        Stmt::While { cond, body } | Stmt::DoWhile { cond, body } => expr_uses_global_invocation_id(cond) || uses_global_invocation_id(body),
        Stmt::Loop { body } | Stmt::Block(body) => uses_global_invocation_id(body),
        Stmt::Return(value) => value.as_ref().is_some_and(expr_uses_global_invocation_id),
        Stmt::Expr(expr) => expr_uses_global_invocation_id(expr),
        Stmt::Break | Stmt::Continue | Stmt::Discard => false,
    }
}

fn expr_uses_global_invocation_id(expr: &Expr) -> bool {
    match expr {
        Expr::Ident(name) => name == GLOBAL_INVOCATION,
        Expr::Unary { expr, .. } => expr_uses_global_invocation_id(expr),
        Expr::Binary { lhs, rhs, .. } => expr_uses_global_invocation_id(lhs) || expr_uses_global_invocation_id(rhs),
        Expr::Call { args, .. } => args.iter().any(expr_uses_global_invocation_id),
        Expr::Member { base, .. } => expr_uses_global_invocation_id(base),
        Expr::Index { base, index } => expr_uses_global_invocation_id(base) || expr_uses_global_invocation_id(index),
        Expr::Paren(inner) => expr_uses_global_invocation_id(inner),
        Expr::IntegerLiteral(_) | Expr::FloatLiteral(_) | Expr::BoolLiteral(_) => false,
    }
}

fn convert_statement(statement: &g::GlslStatement) -> Result<Stmt, FrontendError> {
    match statement {
        g::GlslStatement::Var { name, ty, init, is_const, .. } => Ok(Stmt::Var {
            name: name.clone(),
            ty: ty.as_ref().map(convert_type),
            init: init.as_ref().map(convert_expr).transpose()?,
            is_const: *is_const,
        }),
        g::GlslStatement::Assign { target, op, value } => {
            Ok(Stmt::Assign { target: convert_expr(target)?, op: convert_assign_op(*op), value: convert_expr(value)? })
        }
        g::GlslStatement::If { cond, then_body, else_body } => Ok(Stmt::If {
            cond: convert_expr(cond)?,
            then_body: then_body.iter().map(convert_statement).collect::<Result<_, _>>()?,
            else_body: else_body.iter().map(convert_statement).collect::<Result<_, _>>()?,
        }),
        g::GlslStatement::For { init, cond, update, body } => Ok(Stmt::For {
            init: init.as_ref().map(|statement| convert_statement(statement).map(Box::new)).transpose()?,
            cond: cond.as_ref().map(convert_expr).transpose()?,
            update: update.as_ref().map(|statement| convert_statement(statement).map(Box::new)).transpose()?,
            body: body.iter().map(convert_statement).collect::<Result<_, _>>()?,
        }),
        g::GlslStatement::While { cond, body } => {
            Ok(Stmt::While { cond: convert_expr(cond)?, body: body.iter().map(convert_statement).collect::<Result<_, _>>()? })
        }
        g::GlslStatement::DoWhile { body, cond } => {
            Ok(Stmt::DoWhile { body: body.iter().map(convert_statement).collect::<Result<_, _>>()?, cond: convert_expr(cond)? })
        }
        g::GlslStatement::Break => Ok(Stmt::Break),
        g::GlslStatement::Continue => Ok(Stmt::Continue),
        g::GlslStatement::Discard => Ok(Stmt::Discard),
        g::GlslStatement::Return(value) => Ok(Stmt::Return(value.as_ref().map(convert_expr).transpose()?)),
        g::GlslStatement::Expr(expression) => convert_expression_statement(expression),
        g::GlslStatement::Block(statements) => Ok(Stmt::Block(statements.iter().map(convert_statement).collect::<Result<_, _>>()?)),
    }
}

fn convert_expression_statement(expression: &g::GlslExpression) -> Result<Stmt, FrontendError> {
    if let g::GlslExpression::Unary { op, expr } = expression {
        let step = match op {
            g::GlslUnaryOp::PreIncrement | g::GlslUnaryOp::PostIncrement => Some(AssignOp::AddAssign),
            g::GlslUnaryOp::PreDecrement | g::GlslUnaryOp::PostDecrement => Some(AssignOp::SubAssign),
            _ => None,
        };
        if let Some(step) = step {
            return Ok(Stmt::Assign { target: convert_expr(expr)?, op: step, value: Expr::IntegerLiteral(1) });
        }
    }
    Ok(Stmt::Expr(convert_expr(expression)?))
}

fn convert_assign_op(op: g::GlslAssignOp) -> AssignOp {
    match op {
        g::GlslAssignOp::Assign => AssignOp::Assign,
        g::GlslAssignOp::AddAssign => AssignOp::AddAssign,
        g::GlslAssignOp::SubAssign => AssignOp::SubAssign,
        g::GlslAssignOp::MulAssign => AssignOp::MulAssign,
        g::GlslAssignOp::DivAssign => AssignOp::DivAssign,
        g::GlslAssignOp::ModAssign => AssignOp::ModAssign,
        g::GlslAssignOp::AndAssign => AssignOp::AndAssign,
        g::GlslAssignOp::OrAssign => AssignOp::OrAssign,
        g::GlslAssignOp::XorAssign => AssignOp::XorAssign,
        g::GlslAssignOp::ShlAssign => AssignOp::ShlAssign,
        g::GlslAssignOp::ShrAssign => AssignOp::ShrAssign,
    }
}

fn convert_expr(expression: &g::GlslExpression) -> Result<Expr, FrontendError> {
    match expression {
        g::GlslExpression::IntegerLiteral(value) => Ok(Expr::IntegerLiteral(*value)),
        g::GlslExpression::FloatLiteral(value) => Ok(Expr::FloatLiteral(*value)),
        g::GlslExpression::BoolLiteral(value) => Ok(Expr::BoolLiteral(*value)),
        g::GlslExpression::Ident(name) => Ok(Expr::Ident(normalize_ident(name))),
        g::GlslExpression::Unary { op, expr } => Ok(Expr::Unary {
            op: match op {
                g::GlslUnaryOp::Neg => UnaryOp::Neg,
                g::GlslUnaryOp::Not => UnaryOp::Not,
                g::GlslUnaryOp::BitNot => UnaryOp::BitNot,
                other => return Err(unsupported(format!("unary operator `{other:?}` in an expression"))),
            },
            expr: Box::new(convert_expr(expr)?),
        }),
        g::GlslExpression::Binary { op, lhs, rhs } => {
            Ok(Expr::Binary { op: convert_binary_op(*op), lhs: Box::new(convert_expr(lhs)?), rhs: Box::new(convert_expr(rhs)?) })
        }
        g::GlslExpression::Call { callee, args } => {
            Ok(Expr::Call { callee: normalize_callee(callee), args: args.iter().map(convert_expr).collect::<Result<_, _>>()? })
        }
        g::GlslExpression::Member { base, field } => Ok(Expr::Member { base: Box::new(convert_expr(base)?), field: field.clone() }),
        g::GlslExpression::Index { base, index } => {
            Ok(Expr::Index { base: Box::new(convert_expr(base)?), index: Box::new(convert_expr(index)?) })
        }
        g::GlslExpression::Paren(inner) => Ok(Expr::Paren(Box::new(convert_expr(inner)?))),
    }
}

fn convert_binary_op(op: g::GlslBinaryOp) -> BinaryOp {
    match op {
        g::GlslBinaryOp::Add => BinaryOp::Add,
        g::GlslBinaryOp::Sub => BinaryOp::Sub,
        g::GlslBinaryOp::Mul => BinaryOp::Mul,
        g::GlslBinaryOp::Div => BinaryOp::Div,
        g::GlslBinaryOp::Mod => BinaryOp::Mod,
        g::GlslBinaryOp::Eq => BinaryOp::Eq,
        g::GlslBinaryOp::Ne => BinaryOp::Ne,
        g::GlslBinaryOp::Lt => BinaryOp::Lt,
        g::GlslBinaryOp::Le => BinaryOp::Le,
        g::GlslBinaryOp::Gt => BinaryOp::Gt,
        g::GlslBinaryOp::Ge => BinaryOp::Ge,
        g::GlslBinaryOp::And => BinaryOp::And,
        g::GlslBinaryOp::Or => BinaryOp::Or,
        g::GlslBinaryOp::BitAnd => BinaryOp::BitAnd,
        g::GlslBinaryOp::BitOr => BinaryOp::BitOr,
        g::GlslBinaryOp::BitXor => BinaryOp::BitXor,
        g::GlslBinaryOp::Shl => BinaryOp::Shl,
        g::GlslBinaryOp::Shr => BinaryOp::Shr,
    }
}

fn normalize_ident(name: &str) -> String {
    if name == "gl_GlobalInvocationID" { GLOBAL_INVOCATION.to_owned() } else { name.to_owned() }
}

fn normalize_callee(callee: &str) -> String {
    match callee.split('<').next().unwrap_or(callee).trim() {
        "float" => "f32",
        "uint" => "u32",
        "int" => "i32",
        "double" => "f64",
        other => other,
    }
    .to_owned()
}
