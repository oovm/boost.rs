//! Lowering from the `oak-wgsl` typed syntax tree into UCF program IR.

use std::collections::HashMap;

use hardware_program::{BinOp, Expr, Function, Program, ScalarTy, Stmt, Ty, UnOp, validate_program};
use oak_wgsl::ast::{
    WgslAccessMode, WgslAssignOp, WgslAttribute, WgslBinaryOp, WgslConst, WgslExpression, WgslFunction, WgslItem, WgslRoot, WgslStatement,
    WgslStruct, WgslType, WgslUnaryOp,
};

use crate::error::FrontendError;

/// Guard applied to `while` and `loop`, matching the UCF program validator.
const MAX_LOOP: u32 = 1_000_000;

/// One authored resource binding and the slot the UCF program uses for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceBinding {
    /// Authored variable name.
    pub name: String,
    /// `@group` value.
    pub group: u32,
    /// `@binding` value.
    pub binding: u32,
    /// Logical UCF resource slot.
    pub resource: u32,
    /// Element type read or written through the binding.
    pub element: Ty,
    /// Declared access mode.
    pub access: WgslAccessMode,
}

/// Result of lowering one shader module.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredShader {
    /// Entry function name.
    pub entry: String,
    /// Resource bindings in declaration order.
    pub resources: Vec<ResourceBinding>,
    /// Lowered UCF program.
    pub program: Program,
}

/// Parses WGSL text through `oak-wgsl` and lowers it into a UCF program.
pub fn lower_wgsl(source: &str) -> Result<LoweredShader, FrontendError> {
    let root = oak_wgsl::parse_wgsl(source).map_err(FrontendError::Parse)?;
    Lowerer::default().run(&root)
}

fn unsupported(message: impl Into<String>) -> FrontendError {
    FrontendError::Unsupported(message.into())
}

fn f32_ty() -> Ty {
    Ty::Scalar(ScalarTy::F32)
}

fn u32_ty() -> Ty {
    Ty::Scalar(ScalarTy::U32)
}

fn bool_ty() -> Ty {
    Ty::Scalar(ScalarTy::Bool)
}

fn is_scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Scalar(_))
}

#[derive(Clone)]
struct Local {
    id: u32,
    ty: Ty,
    mutable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Builtin {
    GlobalInvocationId,
}

#[derive(Default)]
struct Lowerer {
    structs: HashMap<String, Ty>,
    aliases: HashMap<String, WgslType>,
    consts: HashMap<String, WgslExpression>,
    resources: Vec<ResourceBinding>,
    resource_slots: HashMap<String, u32>,
    locals: HashMap<String, Local>,
    builtins: HashMap<String, Builtin>,
    slots: Vec<(u32, Ty)>,
    next_slot: u32,
}

impl Lowerer {
    fn run(mut self, root: &WgslRoot) -> Result<LoweredShader, FrontendError> {
        self.collect_struct_names(root);
        self.collect_aliases(root);
        self.fill_structs(root)?;
        self.collect_consts(root);
        self.collect_resources(root)?;

        let entry = root
            .items
            .iter()
            .find_map(|item| match item {
                WgslItem::Function(function) if has_attribute(&function.attributes, "compute") => Some(function),
                _ => None,
            })
            .cloned()
            .ok_or_else(|| unsupported("module has no `@compute` entry function"))?;

        if entry.return_type.is_some() {
            return Err(unsupported(format!("compute entry `{}` declares a return type", entry.name)));
        }

        self.bind_params(&entry)?;
        let mut body = self.lower_block(&entry.body)?;
        if !matches!(body.last(), Some(Stmt::Return(_))) {
            body.push(Stmt::Return(Expr::ConstF32(0.0)));
        }

        let function = Function { name: entry.name.clone(), params: Vec::new(), locals: self.slots, body, ret: f32_ty() };
        let program = Program { version: 1, functions: vec![function], entry: 0 };
        validate_program(&program).map_err(|error| FrontendError::Invalid(error.to_string()))?;

        Ok(LoweredShader { entry: entry.name, resources: self.resources, program })
    }

    fn collect_struct_names(&mut self, root: &WgslRoot) {
        for item in &root.items {
            if let WgslItem::Struct(structure) = item {
                self.structs.insert(structure.name.clone(), Ty::Struct { name: structure.name.clone(), fields: Vec::new() });
            }
        }
    }

    fn collect_aliases(&mut self, root: &WgslRoot) {
        for item in &root.items {
            if let WgslItem::TypeAlias(alias) = item {
                self.aliases.insert(alias.name.clone(), alias.ty.clone());
            }
        }
    }

    fn fill_structs(&mut self, root: &WgslRoot) -> Result<(), FrontendError> {
        for item in &root.items {
            let WgslItem::Struct(WgslStruct { name, members }) = item
            else {
                continue;
            };
            let mut fields = Vec::with_capacity(members.len());
            for member in members {
                fields.push((member.name.clone(), self.map_type(&member.ty)?));
            }
            self.structs.insert(name.clone(), Ty::Struct { name: name.clone(), fields });
        }
        Ok(())
    }

    fn collect_consts(&mut self, root: &WgslRoot) {
        for item in &root.items {
            if let WgslItem::Const(WgslConst { name, init, .. }) = item {
                self.consts.insert(name.clone(), init.clone());
            }
        }
    }

    fn collect_resources(&mut self, root: &WgslRoot) -> Result<(), FrontendError> {
        for item in &root.items {
            let WgslItem::GlobalVar(global) = item
            else {
                continue;
            };
            let Some(address_space) = global.address_space
            else {
                continue;
            };
            if !matches!(address_space, oak_wgsl::ast::WgslAddressSpace::Storage) {
                return Err(unsupported(format!("address space of binding `{}`", global.name)));
            }
            let ty = match &global.ty {
                Some(ty) => self.map_type(ty)?,
                None => return Err(unsupported(format!("binding `{}` has no type", global.name))),
            };
            let element = match ty {
                Ty::Array { elem, .. } => *elem,
                other => other,
            };
            let group = attribute_integer(&global.attributes, "group").unwrap_or(0);
            let binding = attribute_integer(&global.attributes, "binding").unwrap_or(0);
            let resource = self.resources.len() as u32;
            self.resource_slots.insert(global.name.clone(), resource);
            self.resources.push(ResourceBinding {
                name: global.name.clone(),
                group,
                binding,
                resource,
                element,
                access: global.access.unwrap_or(WgslAccessMode::Read),
            });
        }
        Ok(())
    }

    fn bind_params(&mut self, entry: &WgslFunction) -> Result<(), FrontendError> {
        for param in &entry.params {
            match attribute_argument(&param.attributes, "builtin").as_deref() {
                Some("global_invocation_id") => {
                    self.builtins.insert(param.name.clone(), Builtin::GlobalInvocationId);
                }
                Some(other) => {
                    return Err(unsupported(format!("builtin parameter `{other}`")));
                }
                None => {
                    return Err(unsupported(format!("entry parameter `{}` has no `@builtin` attribute", param.name)));
                }
            }
        }
        Ok(())
    }

    fn map_type(&self, ty: &WgslType) -> Result<Ty, FrontendError> {
        match ty.name.as_str() {
            "f32" => Ok(f32_ty()),
            "f64" => Ok(Ty::Scalar(ScalarTy::F64)),
            "i32" => Ok(Ty::Scalar(ScalarTy::I32)),
            "u32" => Ok(u32_ty()),
            "bool" => Ok(bool_ty()),
            "vec2" => Ok(Ty::Vec2),
            "vec3" => Ok(Ty::Vec3),
            "vec4" => Ok(Ty::Vec4),
            "array" => {
                let Some(elem) = ty.args.first()
                else {
                    return Err(unsupported("`array` without an element type"));
                };
                let elem = self.map_type(elem)?;
                let len = ty.args.get(1).map(|arg| arg.name.parse::<u32>().unwrap_or(0)).unwrap_or(0);
                Ok(Ty::Array { elem: Box::new(elem), len })
            }
            name if self.aliases.contains_key(name) => {
                let alias = self.aliases[name].clone();
                if alias.name == name {
                    return Err(unsupported(format!("recursive type alias `{name}`")));
                }
                self.map_type(&alias)
            }
            name if self.structs.contains_key(name) => Ok(self.structs[name].clone()),
            other => Err(unsupported(format!("type `{other}`"))),
        }
    }

    fn lower_block(&mut self, statements: &[WgslStatement]) -> Result<Vec<Stmt>, FrontendError> {
        let saved = self.locals.clone();
        let result = self.lower_statements(statements);
        self.locals = saved;
        result
    }

    fn lower_statements(&mut self, statements: &[WgslStatement]) -> Result<Vec<Stmt>, FrontendError> {
        let mut out = Vec::new();
        for statement in statements {
            self.lower_statement(statement, &mut out)?;
        }
        Ok(out)
    }

    fn lower_statement(&mut self, statement: &WgslStatement, out: &mut Vec<Stmt>) -> Result<(), FrontendError> {
        match statement {
            WgslStatement::Var { name, ty, init, is_const } => {
                let init = init.as_ref().ok_or_else(|| unsupported(format!("declaration `{name}` without an initializer")))?;
                let (value, init_ty) = self.expr(init)?;
                let ty = match ty {
                    Some(ty) => self.map_type(ty)?,
                    None => init_ty,
                };
                let id = self.alloc_slot(ty.clone());
                self.locals.insert(name.clone(), Local { id, ty: ty.clone(), mutable: !is_const });
                out.push(Stmt::Let { id, ty, init: value });
            }
            WgslStatement::Assign { target, op, value } => self.lower_assign(target, *op, value, out)?,
            WgslStatement::If { cond, then_body, else_body } => {
                let (cond, cond_ty) = self.expr(cond)?;
                if cond_ty != bool_ty() {
                    return Err(unsupported("`if` condition that is not boolean"));
                }
                let then_body = self.lower_block(then_body)?;
                let else_body = self.lower_block(else_body)?;
                out.push(Stmt::If { cond, then_body, else_body });
            }
            WgslStatement::For { init, cond, update, body } => {
                self.lower_for(init.as_deref(), cond.as_ref(), update.as_deref(), body, out)?;
            }
            WgslStatement::While { cond, body } => {
                let (cond, cond_ty) = self.expr(cond)?;
                if cond_ty != bool_ty() {
                    return Err(unsupported("`while` condition that is not boolean"));
                }
                let body = self.lower_block(body)?;
                out.push(Stmt::While { cond, limit: MAX_LOOP, body });
            }
            WgslStatement::Loop { body } => {
                let body = self.lower_block(body)?;
                out.push(Stmt::While { cond: Expr::ConstBool(true), limit: MAX_LOOP, body });
            }
            WgslStatement::Break => out.push(Stmt::Break),
            WgslStatement::Continue => out.push(Stmt::Continue),
            WgslStatement::Discard => return Err(unsupported("`discard`")),
            WgslStatement::Return(None) => out.push(Stmt::Return(Expr::ConstF32(0.0))),
            WgslStatement::Return(Some(_)) => return Err(unsupported("compute entry returning a value")),
            WgslStatement::Expr(expression) => {
                let (expression, _) = self.expr(expression)?;
                out.push(Stmt::Expr(expression));
            }
            WgslStatement::Block(statements) => {
                out.extend(self.lower_block(statements)?);
            }
        }
        Ok(())
    }

    fn lower_assign(
        &mut self,
        target: &WgslExpression,
        op: WgslAssignOp,
        value: &WgslExpression,
        out: &mut Vec<Stmt>,
    ) -> Result<(), FrontendError> {
        match target {
            WgslExpression::Ident(name) => {
                let local = self.locals.get(name).cloned().ok_or_else(|| unsupported(format!("assignment to unknown local `{name}`")))?;
                if !local.mutable {
                    return Err(unsupported(format!("assignment to immutable local `{name}`")));
                }
                let (value, value_ty) = self.expr(value)?;
                if value_ty != local.ty {
                    return Err(unsupported(format!("assignment to `{name}` with a mismatched type")));
                }
                let value = match assign_binop(op)? {
                    Some(binop) => Expr::BinOp { op: binop, lhs: Box::new(Expr::LoadLocal(local.id)), rhs: Box::new(value) },
                    None => value,
                };
                out.push(Stmt::StoreLocal { id: local.id, value });
                Ok(())
            }
            WgslExpression::Index { base, index } => {
                let WgslExpression::Ident(name) = base.as_ref()
                else {
                    return Err(unsupported("assignment through a computed buffer expression"));
                };
                let Some(&resource) = self.resource_slots.get(name)
                else {
                    return Err(unsupported(format!("assignment to unknown resource `{name}`")));
                };
                let element = self.resources[resource as usize].element.clone();
                let (index, index_ty) = self.expr(index)?;
                if index_ty != u32_ty() {
                    return Err(unsupported("buffer index that is not `u32`"));
                }
                let (value, value_ty) = self.expr(value)?;
                if value_ty != element {
                    return Err(unsupported(format!("store to `{name}` with a mismatched element type")));
                }
                let value = match assign_binop(op)? {
                    Some(binop) => Expr::BinOp {
                        op: binop,
                        lhs: Box::new(Expr::LoadBuffer { resource, index: Box::new(index.clone()) }),
                        rhs: Box::new(value),
                    },
                    None => value,
                };
                out.push(Stmt::StoreBuffer { resource, index, value });
                Ok(())
            }
            _ => Err(unsupported("assignment target")),
        }
    }

    fn lower_for(
        &mut self,
        init: Option<&WgslStatement>,
        cond: Option<&WgslExpression>,
        update: Option<&WgslStatement>,
        body: &[WgslStatement],
        out: &mut Vec<Stmt>,
    ) -> Result<(), FrontendError> {
        let Some(WgslStatement::Var { name, init: Some(WgslExpression::IntegerLiteral(0)), .. }) = init
        else {
            return Err(unsupported("`for` initializer must declare the loop variable at `0`"));
        };
        let limit = match cond {
            Some(WgslExpression::Binary { op: WgslBinaryOp::Lt, lhs, rhs }) => {
                match lhs.as_ref() {
                    WgslExpression::Ident(inner) if inner == name => {}
                    _ => return Err(unsupported("`for` condition must compare the loop variable")),
                }
                match rhs.as_ref() {
                    WgslExpression::IntegerLiteral(value) => u32::try_from(*value).map_err(|_| unsupported("negative `for` bound"))?,
                    _ => return Err(unsupported("`for` bound must be an integer literal")),
                }
            }
            _ => return Err(unsupported("`for` requires a `< bound` condition")),
        };
        if let Some(update) = update {
            validate_for_update(update, name)?;
        }

        let id = self.alloc_slot(u32_ty());
        self.locals.insert(name.clone(), Local { id, ty: u32_ty(), mutable: true });
        let body = self.lower_block(body)?;
        self.locals.remove(name);
        out.push(Stmt::For { iter: id, limit, body });
        Ok(())
    }

    fn expr(&mut self, expression: &WgslExpression) -> Result<(Expr, Ty), FrontendError> {
        match expression {
            WgslExpression::IntegerLiteral(value) => {
                let value = u32::try_from(*value).map_err(|_| unsupported(format!("integer literal `{value}`")))?;
                Ok((Expr::ConstU32(value), u32_ty()))
            }
            WgslExpression::FloatLiteral(value) => Ok((Expr::ConstF32(*value as f32), f32_ty())),
            WgslExpression::BoolLiteral(value) => Ok((Expr::ConstBool(*value), bool_ty())),
            WgslExpression::Ident(name) => self.binding_expr(name),
            WgslExpression::Unary { op, expr } => {
                let (arg, ty) = self.expr(expr)?;
                match op {
                    WgslUnaryOp::Neg if is_scalar(&ty) => Ok((Expr::UnOp { op: UnOp::Neg, arg: Box::new(arg) }, ty)),
                    WgslUnaryOp::Neg => Err(unsupported("negation of a non-scalar value")),
                    WgslUnaryOp::Not => Err(unsupported("logical negation")),
                    WgslUnaryOp::BitNot => Err(unsupported("bitwise negation")),
                }
            }
            WgslExpression::Binary { op, lhs, rhs } => {
                let binop = binary_op(*op)?;
                let (lhs, lhs_ty) = self.expr(lhs)?;
                let (rhs, rhs_ty) = self.expr(rhs)?;
                let comparison = matches!(binop, BinOp::Lt | BinOp::Le | BinOp::Eq | BinOp::Ne | BinOp::Ge | BinOp::Gt);
                let ty = if comparison {
                    bool_ty()
                }
                else if lhs_ty == rhs_ty {
                    lhs_ty
                }
                else {
                    return Err(unsupported("binary operands with mismatched types"));
                };
                Ok((Expr::BinOp { op: binop, lhs: Box::new(lhs), rhs: Box::new(rhs) }, ty))
            }
            WgslExpression::Call { callee, args } => self.call(callee, args),
            WgslExpression::Member { base, field } => self.member(base, field),
            WgslExpression::Index { base, index } => self.index(base, index),
            WgslExpression::Paren(inner) => self.expr(inner),
        }
    }

    fn binding_expr(&mut self, name: &str) -> Result<(Expr, Ty), FrontendError> {
        if let Some(local) = self.locals.get(name) {
            return Ok((Expr::LoadLocal(local.id), local.ty.clone()));
        }
        if let Some(init) = self.consts.get(name).cloned() {
            return self.expr(&init);
        }
        if self.builtins.contains_key(name) {
            return Err(unsupported(format!("builtin `{name}` must be indexed by a component")));
        }
        if self.resource_slots.contains_key(name) {
            return Err(unsupported(format!("resource `{name}` must be indexed")));
        }
        Err(unsupported(format!("unknown binding `{name}`")))
    }

    fn call(&mut self, callee: &str, args: &[WgslExpression]) -> Result<(Expr, Ty), FrontendError> {
        if callee.starts_with("vec3") {
            if args.len() != 3 {
                return Err(unsupported(format!("`{callee}` with {} arguments", args.len())));
            }
            let mut components = Vec::with_capacity(3);
            for arg in args {
                let (value, ty) = self.expr(arg)?;
                if ty != f32_ty() {
                    return Err(unsupported("`vec3` components that are not `f32`"));
                }
                components.push(value);
            }
            let [x, y, z] = components.as_slice()
            else {
                unreachable!("checked length")
            };
            return Ok((Expr::Vec3Compose { x: Box::new(x.clone()), y: Box::new(y.clone()), z: Box::new(z.clone()) }, Ty::Vec3));
        }
        if callee == "f32" {
            let (value, _) = self.single_arg(callee, args)?;
            return Ok((value, f32_ty()));
        }
        if callee == "u32" {
            let (value, _) = self.single_arg(callee, args)?;
            return Ok((value, u32_ty()));
        }
        let unary = match callee {
            "abs" => Some(UnOp::Abs),
            "sqrt" => Some(UnOp::Sqrt),
            "sin" => Some(UnOp::Sin),
            "cos" => Some(UnOp::Cos),
            _ => None,
        };
        if let Some(op) = unary {
            let (value, ty) = self.single_arg(callee, args)?;
            if ty != f32_ty() {
                return Err(unsupported(format!("`{callee}` on a non-`f32` value")));
            }
            return Ok((Expr::UnOp { op, arg: Box::new(value) }, f32_ty()));
        }
        if callee == "length" {
            let (value, ty) = self.single_arg(callee, args)?;
            if ty != Ty::Vec3 {
                return Err(unsupported("`length` on a non-`vec3` value"));
            }
            return Ok((Expr::UnOp { op: UnOp::Length, arg: Box::new(value) }, Ty::Scalar(ScalarTy::F64)));
        }
        if callee == "normalize" {
            let (value, ty) = self.single_arg(callee, args)?;
            if ty != Ty::Vec3 {
                return Err(unsupported("`normalize` on a non-`vec3` value"));
            }
            return Ok((Expr::UnOp { op: UnOp::Normalize, arg: Box::new(value) }, Ty::Vec3));
        }
        let binary = match callee {
            "min" => Some(BinOp::Min),
            "max" => Some(BinOp::Max),
            "pow" => Some(BinOp::Pow),
            "dot" => Some(BinOp::Dot),
            _ => None,
        };
        if let Some(op) = binary {
            if args.len() != 2 {
                return Err(unsupported(format!("`{callee}` with {} arguments", args.len())));
            }
            let (lhs, lhs_ty) = self.expr(&args[0])?;
            let (rhs, rhs_ty) = self.expr(&args[1])?;
            if lhs_ty != rhs_ty {
                return Err(unsupported(format!("`{callee}` with mismatched argument types")));
            }
            let ty = if op == BinOp::Dot {
                if lhs_ty != Ty::Vec3 {
                    return Err(unsupported("`dot` on a non-`vec3` value"));
                }
                Ty::Scalar(ScalarTy::F64)
            }
            else {
                lhs_ty
            };
            return Ok((Expr::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, ty));
        }
        Err(unsupported(format!("call to `{callee}`")))
    }

    fn single_arg(&mut self, callee: &str, args: &[WgslExpression]) -> Result<(Expr, Ty), FrontendError> {
        match args {
            [arg] => self.expr(arg),
            _ => Err(unsupported(format!("`{callee}` with {} arguments", args.len()))),
        }
    }

    fn member(&mut self, base: &WgslExpression, field: &str) -> Result<(Expr, Ty), FrontendError> {
        if let WgslExpression::Ident(name) = base {
            if self.builtins.get(name) == Some(&Builtin::GlobalInvocationId) && field == "x" {
                return Ok((Expr::GlobalId, u32_ty()));
            }
        }
        Err(unsupported(format!("member access `.{field}`")))
    }

    fn index(&mut self, base: &WgslExpression, index: &WgslExpression) -> Result<(Expr, Ty), FrontendError> {
        let WgslExpression::Ident(name) = base
        else {
            return Err(unsupported("indexing a computed expression"));
        };
        let Some(&resource) = self.resource_slots.get(name)
        else {
            return Err(unsupported(format!("indexing unknown resource `{name}`")));
        };
        let (index, index_ty) = self.expr(index)?;
        if index_ty != u32_ty() {
            return Err(unsupported("buffer index that is not `u32`"));
        }
        let element = self.resources[resource as usize].element.clone();
        Ok((Expr::LoadBuffer { resource, index: Box::new(index) }, element))
    }

    fn alloc_slot(&mut self, ty: Ty) -> u32 {
        let id = self.next_slot;
        self.next_slot += 1;
        self.slots.push((id, ty));
        id
    }
}

fn has_attribute(attributes: &[WgslAttribute], name: &str) -> bool {
    attributes.iter().any(|attribute| attribute.name == name)
}

fn attribute_integer(attributes: &[WgslAttribute], name: &str) -> Option<u32> {
    let attribute = attributes.iter().find(|attribute| attribute.name == name)?;
    match attribute.args.as_slice() {
        [WgslExpression::IntegerLiteral(value)] => u32::try_from(*value).ok(),
        _ => None,
    }
}

fn attribute_argument(attributes: &[WgslAttribute], name: &str) -> Option<String> {
    let attribute = attributes.iter().find(|attribute| attribute.name == name)?;
    match attribute.args.as_slice() {
        [WgslExpression::Ident(value)] => Some(value.clone()),
        _ => None,
    }
}

fn assign_binop(op: WgslAssignOp) -> Result<Option<BinOp>, FrontendError> {
    match op {
        WgslAssignOp::Assign => Ok(None),
        WgslAssignOp::AddAssign => Ok(Some(BinOp::Add)),
        WgslAssignOp::SubAssign => Ok(Some(BinOp::Sub)),
        WgslAssignOp::MulAssign => Ok(Some(BinOp::Mul)),
        WgslAssignOp::DivAssign => Ok(Some(BinOp::Div)),
        WgslAssignOp::ModAssign => Ok(Some(BinOp::Mod)),
        other => Err(unsupported(format!("assignment operator `{other:?}`"))),
    }
}

fn binary_op(op: WgslBinaryOp) -> Result<BinOp, FrontendError> {
    match op {
        WgslBinaryOp::Add => Ok(BinOp::Add),
        WgslBinaryOp::Sub => Ok(BinOp::Sub),
        WgslBinaryOp::Mul => Ok(BinOp::Mul),
        WgslBinaryOp::Div => Ok(BinOp::Div),
        WgslBinaryOp::Mod => Ok(BinOp::Mod),
        WgslBinaryOp::Lt => Ok(BinOp::Lt),
        WgslBinaryOp::Le => Ok(BinOp::Le),
        WgslBinaryOp::Eq => Ok(BinOp::Eq),
        WgslBinaryOp::Ne => Ok(BinOp::Ne),
        WgslBinaryOp::Ge => Ok(BinOp::Ge),
        WgslBinaryOp::Gt => Ok(BinOp::Gt),
        other => Err(unsupported(format!("binary operator `{other:?}`"))),
    }
}

fn validate_for_update(update: &WgslStatement, name: &str) -> Result<(), FrontendError> {
    match update {
        WgslStatement::Assign {
            target: WgslExpression::Ident(target),
            op: WgslAssignOp::Assign,
            value: WgslExpression::Binary { op: WgslBinaryOp::Add, lhs, rhs },
        } if target == name => match (lhs.as_ref(), rhs.as_ref()) {
            (WgslExpression::Ident(base), WgslExpression::IntegerLiteral(1)) if base == name => Ok(()),
            _ => Err(unsupported("`for` update must be `i = i + 1`")),
        },
        WgslStatement::Assign {
            target: WgslExpression::Ident(target),
            op: WgslAssignOp::AddAssign,
            value: WgslExpression::IntegerLiteral(1),
        } if target == name => Ok(()),
        _ => Err(unsupported("`for` update must be `i = i + 1`")),
    }
}
