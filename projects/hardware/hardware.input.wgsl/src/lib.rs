//! WGSL input: `oak-wgsl` syntax tree to neutral shader syntax.
#![warn(missing_docs)]

use hardware_frontend::{
    FrontendError, LoweredShader,
    ast::{
        AccessMode, AddressSpace, AssignOp, Attribute, BinaryOp, ConstDef, Expr, FunctionDef, GlobalVar, ModuleItem, Param, ShaderModule, Stmt,
        StructDef, StructField, TypeAlias, TypeRef, UnaryOp,
    },
    lower_module,
};
use oak_wgsl::ast as w;

/// Parses WGSL text and lowers it into UCF IR.
pub fn lower_wgsl(source: &str) -> Result<LoweredShader, FrontendError> {
    let root = oak_wgsl::parse_wgsl(source).map_err(FrontendError::Parse)?;
    lower_module(&convert_module(&root))
}

/// Parses WGSL text into the language-neutral shader module.
pub fn parse_wgsl_module(source: &str) -> Result<ShaderModule, FrontendError> {
    let root = oak_wgsl::parse_wgsl(source).map_err(FrontendError::Parse)?;
    Ok(convert_module(&root))
}

fn convert_module(root: &w::WgslRoot) -> ShaderModule {
    ShaderModule { items: root.items.iter().map(convert_item).collect() }
}

fn convert_item(item: &w::WgslItem) -> ModuleItem {
    match item {
        w::WgslItem::Function(function) => ModuleItem::Function(convert_function(function)),
        w::WgslItem::Struct(structure) => ModuleItem::Struct(convert_struct(structure)),
        w::WgslItem::GlobalVar(global) => ModuleItem::GlobalVar(convert_global(global)),
        w::WgslItem::Const(constant) => ModuleItem::Const(ConstDef {
            name: constant.name.clone(),
            ty: constant.ty.as_ref().map(convert_type),
            init: convert_expr(&constant.init),
        }),
        w::WgslItem::TypeAlias(alias) => ModuleItem::TypeAlias(TypeAlias { name: alias.name.clone(), ty: convert_type(&alias.ty) }),
    }
}

fn convert_function(function: &w::WgslFunction) -> FunctionDef {
    FunctionDef {
        attributes: function.attributes.iter().map(convert_attribute).collect(),
        name: function.name.clone(),
        params: function
            .params
            .iter()
            .map(|param| Param {
                attributes: param.attributes.iter().map(convert_attribute).collect(),
                name: param.name.clone(),
                ty: convert_type(&param.ty),
            })
            .collect(),
        return_type: function.return_type.as_ref().map(convert_type),
        body: function.body.iter().map(convert_statement).collect(),
    }
}

fn convert_struct(structure: &w::WgslStruct) -> StructDef {
    StructDef {
        name: structure.name.clone(),
        fields: structure
            .members
            .iter()
            .map(|member| StructField {
                attributes: member.attributes.iter().map(convert_attribute).collect(),
                name: member.name.clone(),
                ty: convert_type(&member.ty),
            })
            .collect(),
    }
}

fn convert_global(global: &w::WgslGlobalVar) -> GlobalVar {
    GlobalVar {
        attributes: global.attributes.iter().map(convert_attribute).collect(),
        address_space: global.address_space.map(|space| match space {
            w::WgslAddressSpace::Function => AddressSpace::Function,
            w::WgslAddressSpace::Private => AddressSpace::Private,
            w::WgslAddressSpace::Workgroup => AddressSpace::Workgroup,
            w::WgslAddressSpace::Uniform => AddressSpace::Uniform,
            w::WgslAddressSpace::Storage => AddressSpace::Storage,
        }),
        access: global.access.map(|access| match access {
            w::WgslAccessMode::Read => AccessMode::Read,
            w::WgslAccessMode::Write => AccessMode::Write,
            w::WgslAccessMode::ReadWrite => AccessMode::ReadWrite,
        }),
        name: global.name.clone(),
        ty: global.ty.as_ref().map(convert_type),
        init: global.init.as_ref().map(convert_expr),
    }
}

fn convert_attribute(attribute: &w::WgslAttribute) -> Attribute {
    Attribute { name: attribute.name.clone(), args: attribute.args.iter().map(convert_expr).collect() }
}

fn convert_type(ty: &w::WgslType) -> TypeRef {
    TypeRef { name: ty.name.clone(), args: ty.args.iter().map(convert_type).collect() }
}

fn convert_statement(statement: &w::WgslStatement) -> Stmt {
    match statement {
        w::WgslStatement::Var { name, ty, init, is_const } => {
            Stmt::Var { name: name.clone(), ty: ty.as_ref().map(convert_type), init: init.as_ref().map(convert_expr), is_const: *is_const }
        }
        w::WgslStatement::Assign { target, op, value } => {
            Stmt::Assign { target: convert_expr(target), op: convert_assign_op(*op), value: convert_expr(value) }
        }
        w::WgslStatement::If { cond, then_body, else_body } => Stmt::If {
            cond: convert_expr(cond),
            then_body: then_body.iter().map(convert_statement).collect(),
            else_body: else_body.iter().map(convert_statement).collect(),
        },
        w::WgslStatement::For { init, cond, update, body } => Stmt::For {
            init: init.as_ref().map(|statement| Box::new(convert_statement(statement))),
            cond: cond.as_ref().map(convert_expr),
            update: update.as_ref().map(|statement| Box::new(convert_statement(statement))),
            body: body.iter().map(convert_statement).collect(),
        },
        w::WgslStatement::While { cond, body } => Stmt::While { cond: convert_expr(cond), body: body.iter().map(convert_statement).collect() },
        w::WgslStatement::Loop { body } => Stmt::Loop { body: body.iter().map(convert_statement).collect() },
        w::WgslStatement::Break => Stmt::Break,
        w::WgslStatement::Continue => Stmt::Continue,
        w::WgslStatement::Discard => Stmt::Discard,
        w::WgslStatement::Return(value) => Stmt::Return(value.as_ref().map(convert_expr)),
        w::WgslStatement::Expr(expression) => Stmt::Expr(convert_expr(expression)),
        w::WgslStatement::Block(statements) => Stmt::Block(statements.iter().map(convert_statement).collect()),
    }
}

fn convert_assign_op(op: w::WgslAssignOp) -> AssignOp {
    match op {
        w::WgslAssignOp::Assign => AssignOp::Assign,
        w::WgslAssignOp::AddAssign => AssignOp::AddAssign,
        w::WgslAssignOp::SubAssign => AssignOp::SubAssign,
        w::WgslAssignOp::MulAssign => AssignOp::MulAssign,
        w::WgslAssignOp::DivAssign => AssignOp::DivAssign,
        w::WgslAssignOp::ModAssign => AssignOp::ModAssign,
        w::WgslAssignOp::AndAssign => AssignOp::AndAssign,
        w::WgslAssignOp::OrAssign => AssignOp::OrAssign,
        w::WgslAssignOp::XorAssign => AssignOp::XorAssign,
        w::WgslAssignOp::ShlAssign => AssignOp::ShlAssign,
        w::WgslAssignOp::ShrAssign => AssignOp::ShrAssign,
    }
}

fn convert_expr(expression: &w::WgslExpression) -> Expr {
    match expression {
        w::WgslExpression::IntegerLiteral(value) => Expr::IntegerLiteral(*value),
        w::WgslExpression::FloatLiteral(value) => Expr::FloatLiteral(*value),
        w::WgslExpression::BoolLiteral(value) => Expr::BoolLiteral(*value),
        w::WgslExpression::Ident(name) => Expr::Ident(name.clone()),
        w::WgslExpression::Unary { op, expr } => Expr::Unary {
            op: match op {
                w::WgslUnaryOp::Neg => UnaryOp::Neg,
                w::WgslUnaryOp::Not => UnaryOp::Not,
                w::WgslUnaryOp::BitNot => UnaryOp::BitNot,
            },
            expr: Box::new(convert_expr(expr)),
        },
        w::WgslExpression::Binary { op, lhs, rhs } => {
            Expr::Binary { op: convert_binary_op(*op), lhs: Box::new(convert_expr(lhs)), rhs: Box::new(convert_expr(rhs)) }
        }
        w::WgslExpression::Call { callee, args } => {
            Expr::Call { callee: normalize_callee(callee), args: args.iter().map(convert_expr).collect() }
        }
        w::WgslExpression::Member { base, field } => Expr::Member { base: Box::new(convert_expr(base)), field: field.clone() },
        w::WgslExpression::Index { base, index } => Expr::Index { base: Box::new(convert_expr(base)), index: Box::new(convert_expr(index)) },
        w::WgslExpression::Paren(inner) => Expr::Paren(Box::new(convert_expr(inner))),
    }
}

fn convert_binary_op(op: w::WgslBinaryOp) -> BinaryOp {
    match op {
        w::WgslBinaryOp::Add => BinaryOp::Add,
        w::WgslBinaryOp::Sub => BinaryOp::Sub,
        w::WgslBinaryOp::Mul => BinaryOp::Mul,
        w::WgslBinaryOp::Div => BinaryOp::Div,
        w::WgslBinaryOp::Mod => BinaryOp::Mod,
        w::WgslBinaryOp::Eq => BinaryOp::Eq,
        w::WgslBinaryOp::Ne => BinaryOp::Ne,
        w::WgslBinaryOp::Lt => BinaryOp::Lt,
        w::WgslBinaryOp::Le => BinaryOp::Le,
        w::WgslBinaryOp::Gt => BinaryOp::Gt,
        w::WgslBinaryOp::Ge => BinaryOp::Ge,
        w::WgslBinaryOp::And => BinaryOp::And,
        w::WgslBinaryOp::Or => BinaryOp::Or,
        w::WgslBinaryOp::BitAnd => BinaryOp::BitAnd,
        w::WgslBinaryOp::BitOr => BinaryOp::BitOr,
        w::WgslBinaryOp::BitXor => BinaryOp::BitXor,
        w::WgslBinaryOp::Shl => BinaryOp::Shl,
        w::WgslBinaryOp::Shr => BinaryOp::Shr,
    }
}

/// Strips generic arguments so `vec3<f32>` becomes the neutral `vec3`.
fn normalize_callee(callee: &str) -> String {
    callee.split('<').next().unwrap_or(callee).trim().to_owned()
}
