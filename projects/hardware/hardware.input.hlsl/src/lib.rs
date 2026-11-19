//! HLSL input: `data-text-hlsl` syntax tree to neutral shader syntax.
#![warn(missing_docs)]

use data_text_hlsl::ast as h;
use hardware_frontend::{
    FrontendError, LoweredShader,
    ast::{
        AccessMode, AddressSpace, AssignOp, Attribute, BinaryOp, ConstDef, Expr, FunctionDef, GlobalVar, ModuleItem, Param, ShaderModule, Stmt,
        StructDef, StructField, TypeAlias, TypeRef, UnaryOp,
    },
    lower_module,
};

/// Parses HLSL text and lowers it into UCF IR.
pub fn lower_hlsl(source: &str) -> Result<LoweredShader, FrontendError> {
    let root = data_text_hlsl::parse_hlsl(source).map_err(FrontendError::Parse)?;
    lower_module(&convert_module(&root)?)
}

/// Parses HLSL text into the language-neutral shader module.
pub fn parse_hlsl_module(source: &str) -> Result<ShaderModule, FrontendError> {
    let root = data_text_hlsl::parse_hlsl(source).map_err(FrontendError::Parse)?;
    convert_module(&root)
}

fn unsupported(message: impl Into<String>) -> FrontendError {
    FrontendError::Unsupported(message.into())
}

fn convert_module(root: &h::HlslRoot) -> Result<ShaderModule, FrontendError> {
    let mut items = Vec::new();
    let mut buffers = 0usize;
    for item in &root.items {
        if matches!(item, h::HlslItem::GlobalVar(global) if global.name.is_empty()) {
            continue;
        }
        if let h::HlslItem::Cbuffer(buffer) = item {
            items.extend(convert_cbuffer(buffer, buffers)?);
            buffers += 1;
            continue;
        }
        items.push(convert_item(item)?);
    }
    Ok(ShaderModule { items })
}

fn convert_item(item: &h::HlslItem) -> Result<ModuleItem, FrontendError> {
    match item {
        h::HlslItem::Function(function) => Ok(ModuleItem::Function(convert_function(function)?)),
        h::HlslItem::Struct(structure) => Ok(ModuleItem::Struct(convert_struct(structure))),
        h::HlslItem::GlobalVar(global) => convert_global(global),
        h::HlslItem::Const(constant) => Ok(ModuleItem::Const(ConstDef {
            name: constant.name.clone(),
            ty: constant.ty.as_ref().map(convert_type),
            init: convert_expr(&constant.init)?,
        })),
        h::HlslItem::TypeAlias(alias) => Ok(ModuleItem::TypeAlias(TypeAlias { name: alias.name.clone(), ty: convert_type(&alias.ty) })),
        h::HlslItem::Cbuffer(_) => Err(unsupported("`cbuffer` blocks are expanded before item conversion")),
    }
}

/// Expands one `cbuffer` / `tbuffer` block into a struct definition plus a
/// uniform global bound to the block's `register(bN)` slot.
fn convert_cbuffer(buffer: &h::HlslCbuffer, index: usize) -> Result<Vec<ModuleItem>, FrontendError> {
    let name = if buffer.name.is_empty() { format!("Cbuffer{index}") } else { buffer.name.clone() };
    let binding = buffer.register.as_ref().and_then(|register| register.index).unwrap_or(0);
    let fields = buffer
        .members
        .iter()
        .map(|member| StructField {
            attributes: member.attributes.iter().filter_map(|attribute| convert_attribute(attribute).ok()).collect(),
            name: member.name.clone(),
            ty: convert_type(&member.ty),
        })
        .collect();
    Ok(vec![
        ModuleItem::Struct(StructDef { name: name.clone(), fields }),
        ModuleItem::GlobalVar(GlobalVar {
            attributes: vec![
                Attribute { name: "group".into(), args: vec![Expr::IntegerLiteral(0)] },
                Attribute { name: "binding".into(), args: vec![Expr::IntegerLiteral(i64::from(binding))] },
            ],
            address_space: Some(AddressSpace::Uniform),
            access: Some(AccessMode::Read),
            name: name.clone(),
            ty: Some(TypeRef { name, args: Vec::new() }),
            init: None,
        }),
    ])
}

fn convert_function(function: &h::HlslFunction) -> Result<FunctionDef, FrontendError> {
    let mut attributes: Vec<Attribute> = function.attributes.iter().map(convert_attribute).collect::<Result<_, _>>()?;
    if let Some(threads) = function.attributes.iter().find(|attribute| attribute.name == "numthreads") {
        attributes.push(Attribute { name: "workgroup_size".into(), args: threads.args.iter().map(convert_expr).collect::<Result<_, _>>()? });
        attributes.push(Attribute { name: "compute".into(), args: Vec::new() });
    }
    Ok(FunctionDef {
        attributes,
        name: function.name.clone(),
        params: function.params.iter().map(convert_param).collect::<Result<_, _>>()?,
        return_type: function.return_type.as_ref().filter(|ty| ty.name != "void").map(convert_type),
        body: function.body.iter().map(convert_statement).collect::<Result<_, _>>()?,
    })
}

fn convert_param(param: &h::HlslParam) -> Result<Param, FrontendError> {
    let mut attributes = Vec::new();
    if let Some(semantic) = &param.semantic {
        if semantic == "SV_DispatchThreadID" {
            attributes.push(Attribute { name: "builtin".into(), args: vec![Expr::Ident("global_invocation_id".into())] });
        }
        else {
            attributes.push(Attribute { name: "semantic".into(), args: vec![Expr::Ident(semantic.clone())] });
        }
    }
    Ok(Param { attributes, name: param.name.clone(), ty: convert_type(&param.ty) })
}

fn convert_struct(structure: &h::HlslStruct) -> StructDef {
    StructDef {
        name: structure.name.clone(),
        fields: structure
            .members
            .iter()
            .map(|member| StructField {
                attributes: member.attributes.iter().filter_map(|attribute| convert_attribute(attribute).ok()).collect(),
                name: member.name.clone(),
                ty: convert_type(&member.ty),
            })
            .collect(),
    }
}

fn convert_global(global: &h::HlslGlobalVar) -> Result<ModuleItem, FrontendError> {
    let name = global.name.clone();
    let ty = global.ty.as_ref().ok_or_else(|| unsupported(format!("global `{name}` has no type")))?;
    let binding = global.register.as_ref().and_then(|register| register.index).unwrap_or(0);
    if global.qualifiers.contains(&h::HlslQualifier::Uniform) {
        return Ok(ModuleItem::GlobalVar(GlobalVar {
            attributes: vec![
                Attribute { name: "group".into(), args: vec![Expr::IntegerLiteral(0)] },
                Attribute { name: "binding".into(), args: vec![Expr::IntegerLiteral(i64::from(binding))] },
            ],
            address_space: Some(AddressSpace::Uniform),
            access: Some(AccessMode::Read),
            name,
            ty: Some(convert_type(ty)),
            init: None,
        }));
    }
    let buffer = buffer_binding(ty)?;
    let Some((element, access)) = buffer
    else {
        return Err(unsupported(format!("global `{name}` of type `{}`", ty.name)));
    };
    let address_space =
        if global.qualifiers.contains(&h::HlslQualifier::Groupshared) { AddressSpace::Workgroup } else { AddressSpace::Storage };
    Ok(ModuleItem::GlobalVar(GlobalVar {
        attributes: vec![
            Attribute { name: "group".into(), args: vec![Expr::IntegerLiteral(0)] },
            Attribute { name: "binding".into(), args: vec![Expr::IntegerLiteral(i64::from(binding))] },
        ],
        address_space: Some(address_space),
        access: Some(access),
        name,
        ty: Some(TypeRef { name: "array".into(), args: vec![element] }),
        init: None,
    }))
}

/// Maps structured buffer types to their element type and access mode.
fn buffer_binding(ty: &h::HlslType) -> Result<Option<(TypeRef, AccessMode)>, FrontendError> {
    let (access, element) = match ty.name.as_str() {
        "RWStructuredBuffer" | "RWBuffer" => (AccessMode::ReadWrite, ty.args.first()),
        "StructuredBuffer" | "Buffer" => (AccessMode::Read, ty.args.first()),
        "RWByteAddressBuffer" | "ByteAddressBuffer" => {
            return Err(unsupported(format!("`{}` byte-address buffers", ty.name)));
        }
        _ => return Ok(None),
    };
    let element = element.ok_or_else(|| unsupported(format!("`{}` without an element type", ty.name)))?;
    Ok(Some((convert_type(element), access)))
}

fn convert_attribute(attribute: &h::HlslAttribute) -> Result<Attribute, FrontendError> {
    Ok(Attribute { name: attribute.name.clone(), args: attribute.args.iter().map(convert_expr).collect::<Result<_, _>>()? })
}

fn convert_type(ty: &h::HlslType) -> TypeRef {
    TypeRef { name: convert_type_name(&ty.name), args: ty.args.iter().map(convert_type).collect() }
}

fn convert_type_name(name: &str) -> String {
    match name {
        "float" => "f32",
        "double" => "f64",
        "half" => "f32",
        "float2" => "vec2",
        "float3" => "vec3",
        "float4" => "vec4",
        "uint" => "u32",
        "int" => "i32",
        "bool" => "bool",
        other => other,
    }
    .to_owned()
}

fn convert_statement(statement: &h::HlslStatement) -> Result<Stmt, FrontendError> {
    match statement {
        h::HlslStatement::Var { name, ty, init, is_const, .. } => Ok(Stmt::Var {
            name: name.clone(),
            ty: ty.as_ref().map(convert_type),
            init: init.as_ref().map(convert_expr).transpose()?,
            is_const: *is_const,
        }),
        h::HlslStatement::Assign { target, op, value } => {
            Ok(Stmt::Assign { target: convert_expr(target)?, op: convert_assign_op(*op), value: convert_expr(value)? })
        }
        h::HlslStatement::If { cond, then_body, else_body } => Ok(Stmt::If {
            cond: convert_expr(cond)?,
            then_body: then_body.iter().map(convert_statement).collect::<Result<_, _>>()?,
            else_body: else_body.iter().map(convert_statement).collect::<Result<_, _>>()?,
        }),
        h::HlslStatement::For { init, cond, update, body } => Ok(Stmt::For {
            init: init.as_ref().map(|statement| convert_statement(statement).map(Box::new)).transpose()?,
            cond: cond.as_ref().map(convert_expr).transpose()?,
            update: update.as_ref().map(|statement| convert_statement(statement).map(Box::new)).transpose()?,
            body: body.iter().map(convert_statement).collect::<Result<_, _>>()?,
        }),
        h::HlslStatement::While { cond, body } => {
            Ok(Stmt::While { cond: convert_expr(cond)?, body: body.iter().map(convert_statement).collect::<Result<_, _>>()? })
        }
        h::HlslStatement::DoWhile { body, cond } => {
            Ok(Stmt::DoWhile { body: body.iter().map(convert_statement).collect::<Result<_, _>>()?, cond: convert_expr(cond)? })
        }
        h::HlslStatement::Break => Ok(Stmt::Break),
        h::HlslStatement::Continue => Ok(Stmt::Continue),
        h::HlslStatement::Discard => Ok(Stmt::Discard),
        h::HlslStatement::Return(value) => Ok(Stmt::Return(value.as_ref().map(convert_expr).transpose()?)),
        h::HlslStatement::Expr(expression) => convert_expression_statement(expression),
        h::HlslStatement::Block(statements) => Ok(Stmt::Block(statements.iter().map(convert_statement).collect::<Result<_, _>>()?)),
    }
}

/// Rewrites `i++` / `++i` statements into the neutral compound assignment form.
fn convert_expression_statement(expression: &h::HlslExpression) -> Result<Stmt, FrontendError> {
    if let h::HlslExpression::Unary { op, expr } = expression {
        let step = match op {
            h::HlslUnaryOp::PreIncrement | h::HlslUnaryOp::PostIncrement => Some(AssignOp::AddAssign),
            h::HlslUnaryOp::PreDecrement | h::HlslUnaryOp::PostDecrement => Some(AssignOp::SubAssign),
            _ => None,
        };
        if let Some(step) = step {
            return Ok(Stmt::Assign { target: convert_expr(expr)?, op: step, value: Expr::IntegerLiteral(1) });
        }
    }
    Ok(Stmt::Expr(convert_expr(expression)?))
}

fn convert_assign_op(op: h::HlslAssignOp) -> AssignOp {
    match op {
        h::HlslAssignOp::Assign => AssignOp::Assign,
        h::HlslAssignOp::AddAssign => AssignOp::AddAssign,
        h::HlslAssignOp::SubAssign => AssignOp::SubAssign,
        h::HlslAssignOp::MulAssign => AssignOp::MulAssign,
        h::HlslAssignOp::DivAssign => AssignOp::DivAssign,
        h::HlslAssignOp::ModAssign => AssignOp::ModAssign,
        h::HlslAssignOp::AndAssign => AssignOp::AndAssign,
        h::HlslAssignOp::OrAssign => AssignOp::OrAssign,
        h::HlslAssignOp::XorAssign => AssignOp::XorAssign,
        h::HlslAssignOp::ShlAssign => AssignOp::ShlAssign,
        h::HlslAssignOp::ShrAssign => AssignOp::ShrAssign,
    }
}

fn convert_expr(expression: &h::HlslExpression) -> Result<Expr, FrontendError> {
    match expression {
        h::HlslExpression::IntegerLiteral(value) => Ok(Expr::IntegerLiteral(*value)),
        h::HlslExpression::FloatLiteral(value) => Ok(Expr::FloatLiteral(*value)),
        h::HlslExpression::BoolLiteral(value) => Ok(Expr::BoolLiteral(*value)),
        h::HlslExpression::Ident(name) => Ok(Expr::Ident(name.clone())),
        h::HlslExpression::Unary { op, expr } => Ok(Expr::Unary {
            op: match op {
                h::HlslUnaryOp::Neg => UnaryOp::Neg,
                h::HlslUnaryOp::Not => UnaryOp::Not,
                h::HlslUnaryOp::BitNot => UnaryOp::BitNot,
                other => return Err(unsupported(format!("unary operator `{other:?}` in an expression"))),
            },
            expr: Box::new(convert_expr(expr)?),
        }),
        h::HlslExpression::Binary { op, lhs, rhs } => {
            Ok(Expr::Binary { op: convert_binary_op(*op), lhs: Box::new(convert_expr(lhs)?), rhs: Box::new(convert_expr(rhs)?) })
        }
        h::HlslExpression::Call { callee, args } => {
            Ok(Expr::Call { callee: normalize_callee(callee), args: args.iter().map(convert_expr).collect::<Result<_, _>>()? })
        }
        h::HlslExpression::Member { base, field } => Ok(Expr::Member { base: Box::new(convert_expr(base)?), field: field.clone() }),
        h::HlslExpression::Index { base, index } => {
            Ok(Expr::Index { base: Box::new(convert_expr(base)?), index: Box::new(convert_expr(index)?) })
        }
        h::HlslExpression::Paren(inner) => Ok(Expr::Paren(Box::new(convert_expr(inner)?))),
    }
}

fn convert_binary_op(op: h::HlslBinaryOp) -> BinaryOp {
    match op {
        h::HlslBinaryOp::Add => BinaryOp::Add,
        h::HlslBinaryOp::Sub => BinaryOp::Sub,
        h::HlslBinaryOp::Mul => BinaryOp::Mul,
        h::HlslBinaryOp::Div => BinaryOp::Div,
        h::HlslBinaryOp::Mod => BinaryOp::Mod,
        h::HlslBinaryOp::Eq => BinaryOp::Eq,
        h::HlslBinaryOp::Ne => BinaryOp::Ne,
        h::HlslBinaryOp::Lt => BinaryOp::Lt,
        h::HlslBinaryOp::Le => BinaryOp::Le,
        h::HlslBinaryOp::Gt => BinaryOp::Gt,
        h::HlslBinaryOp::Ge => BinaryOp::Ge,
        h::HlslBinaryOp::And => BinaryOp::And,
        h::HlslBinaryOp::Or => BinaryOp::Or,
        h::HlslBinaryOp::BitAnd => BinaryOp::BitAnd,
        h::HlslBinaryOp::BitOr => BinaryOp::BitOr,
        h::HlslBinaryOp::BitXor => BinaryOp::BitXor,
        h::HlslBinaryOp::Shl => BinaryOp::Shl,
        h::HlslBinaryOp::Shr => BinaryOp::Shr,
    }
}

/// Normalizes HLSL spellings such as `float3` or `f32` into neutral constructors.
fn normalize_callee(callee: &str) -> String {
    let base = callee.split('<').next().unwrap_or(callee).trim();
    match base {
        "float" => "f32",
        "uint" => "u32",
        "int" => "i32",
        "float2" => "vec2",
        "float3" => "vec3",
        "float4" => "vec4",
        other => other,
    }
    .to_owned()
}
