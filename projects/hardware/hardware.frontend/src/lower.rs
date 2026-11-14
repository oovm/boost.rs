//! Lowering from the language-neutral shader syntax into UCF program IR.

use std::collections::HashMap;

use hardware_program::{BinOp, Expr as IrExpr, Function, Program, ScalarTy, Stmt as IrStmt, Ty, UnOp, validate_program};

use crate::{
    ast::{
        AccessMode, AddressSpace, AssignOp, Attribute, BinaryOp, ConstDef, Expr, FunctionDef, ModuleItem, ShaderModule, Stmt, StructDef,
        TypeAlias, TypeRef, UnaryOp,
    },
    error::FrontendError,
};

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
    pub access: AccessMode,
}

/// One authored uniform binding lowered into contiguous scalar uniform slots.
///
/// A uniform block contributes one entry: every scalar field takes the next
/// slot, so `fields[i]` is read through `LoadUniform(slot + i)`. A scalar
/// binding contributes an entry with no fields and reads through `slot`.
#[derive(Debug, Clone, PartialEq)]
pub struct UniformBinding {
    /// Authored block or variable name.
    pub name: String,
    /// `@group` value.
    pub group: u32,
    /// `@binding` or register value.
    pub binding: u32,
    /// First UCF uniform slot the binding occupies.
    pub slot: u32,
    /// Scalar fields in declaration order. Empty for a single scalar binding.
    pub fields: Vec<(String, Ty)>,
}

/// Result of lowering one shader module.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredShader {
    /// Entry function name.
    pub entry: String,
    /// Resource bindings in declaration order.
    pub resources: Vec<ResourceBinding>,
    /// Uniform bindings in declaration order.
    pub uniforms: Vec<UniformBinding>,
    /// Lowered UCF program.
    pub program: Program,
}

/// Lowers a language-neutral shader module into a UCF program.
pub fn lower_module(module: &ShaderModule) -> Result<LoweredShader, FrontendError> {
    Lowerer::default().run(module)
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

fn is_uniform_scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Scalar(ScalarTy::F32 | ScalarTy::U32 | ScalarTy::I32 | ScalarTy::Bool))
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
    aliases: HashMap<String, TypeRef>,
    consts: HashMap<String, Expr>,
    resources: Vec<ResourceBinding>,
    resource_slots: HashMap<String, u32>,
    uniforms: Vec<UniformBinding>,
    uniform_slots: HashMap<String, usize>,
    uniform_scalars: HashMap<String, (u32, Ty)>,
    next_uniform_slot: u32,
    locals: HashMap<String, Local>,
    builtins: HashMap<String, Builtin>,
    slots: Vec<(u32, Ty)>,
    next_slot: u32,
}

impl Lowerer {
    fn run(mut self, module: &ShaderModule) -> Result<LoweredShader, FrontendError> {
        self.collect_struct_names(module);
        self.collect_aliases(module);
        self.fill_structs(module)?;
        self.collect_consts(module);
        self.collect_resources(module)?;

        let entry = module
            .items
            .iter()
            .find_map(|item| match item {
                ModuleItem::Function(function) if has_attribute(&function.attributes, "compute") => Some(function),
                _ => None,
            })
            .cloned()
            .ok_or_else(|| unsupported("module has no `compute` entry function"))?;

        if entry.return_type.is_some() {
            return Err(unsupported(format!("compute entry `{}` declares a return type", entry.name)));
        }

        self.bind_params(&entry)?;
        let mut body = self.lower_block(&entry.body)?;
        if !matches!(body.last(), Some(IrStmt::Return(_))) {
            body.push(IrStmt::Return(IrExpr::ConstF32(0.0)));
        }

        let function = Function { name: entry.name.clone(), params: Vec::new(), locals: self.slots, body, ret: f32_ty() };
        let program = Program { version: 1, functions: vec![function], entry: 0 };
        validate_program(&program).map_err(|error| FrontendError::Invalid(error.to_string()))?;

        Ok(LoweredShader { entry: entry.name, resources: self.resources, uniforms: self.uniforms, program })
    }

    fn collect_struct_names(&mut self, module: &ShaderModule) {
        for item in &module.items {
            if let ModuleItem::Struct(structure) = item {
                self.structs.insert(structure.name.clone(), Ty::Struct { name: structure.name.clone(), fields: Vec::new() });
            }
        }
    }

    fn collect_aliases(&mut self, module: &ShaderModule) {
        for item in &module.items {
            if let ModuleItem::TypeAlias(TypeAlias { name, ty }) = item {
                self.aliases.insert(name.clone(), ty.clone());
            }
        }
    }

    fn fill_structs(&mut self, module: &ShaderModule) -> Result<(), FrontendError> {
        for item in &module.items {
            let ModuleItem::Struct(StructDef { name, fields }) = item
            else {
                continue;
            };
            let mut resolved = Vec::with_capacity(fields.len());
            for field in fields {
                resolved.push((field.name.clone(), self.map_type(&field.ty)?));
            }
            self.structs.insert(name.clone(), Ty::Struct { name: name.clone(), fields: resolved });
        }
        Ok(())
    }

    fn collect_consts(&mut self, module: &ShaderModule) {
        for item in &module.items {
            if let ModuleItem::Const(ConstDef { name, init, .. }) = item {
                self.consts.insert(name.clone(), init.clone());
            }
        }
    }

    fn collect_resources(&mut self, module: &ShaderModule) -> Result<(), FrontendError> {
        for item in &module.items {
            let ModuleItem::GlobalVar(global) = item
            else {
                continue;
            };
            let Some(address_space) = global.address_space
            else {
                continue;
            };
            if !matches!(address_space, AddressSpace::Storage | AddressSpace::Uniform) {
                return Err(unsupported(format!("address space of binding `{}`", global.name)));
            }
            if self.resource_slots.contains_key(&global.name) || self.uniform_slots.contains_key(&global.name) {
                return Err(unsupported(format!("binding `{}` is declared more than once", global.name)));
            }
            let ty = match &global.ty {
                Some(ty) => self.map_type(ty)?,
                None => return Err(unsupported(format!("binding `{}` has no type", global.name))),
            };
            let group = attribute_integer(&global.attributes, "group").unwrap_or(0);
            let binding = attribute_integer(&global.attributes, "binding").unwrap_or(0);
            match address_space {
                AddressSpace::Storage => {
                    let element = match ty {
                        Ty::Array { elem, .. } => *elem,
                        other => other,
                    };
                    let resource = self.resources.len() as u32;
                    self.resource_slots.insert(global.name.clone(), resource);
                    self.resources.push(ResourceBinding {
                        name: global.name.clone(),
                        group,
                        binding,
                        resource,
                        element,
                        access: global.access.unwrap_or(AccessMode::Read),
                    });
                }
                AddressSpace::Uniform => self.collect_uniform(&global.name, ty, group, binding)?,
                _ => unreachable!("other address spaces are rejected above"),
            }
        }
        Ok(())
    }

    /// Flattens one uniform binding into consecutive scalar slots.
    fn collect_uniform(&mut self, name: &str, ty: Ty, group: u32, binding: u32) -> Result<(), FrontendError> {
        let fields = match &ty {
            Ty::Struct { fields, .. } if !fields.is_empty() => fields.clone(),
            Ty::Scalar(_) if is_uniform_scalar(&ty) => Vec::new(),
            other => return Err(unsupported(format!("uniform `{name}` of type `{other:?}`"))),
        };
        for (field, field_ty) in &fields {
            if !is_uniform_scalar(field_ty) {
                return Err(unsupported(format!("uniform field `{name}.{field}` of type `{field_ty:?}`")));
            }
        }
        let slot = self.next_uniform_slot;
        let width = u32::try_from(fields.len()).unwrap_or(u32::MAX).max(1);
        self.next_uniform_slot = self.next_uniform_slot.saturating_add(width);
        self.uniform_slots.insert(name.to_owned(), self.uniforms.len());
        if fields.is_empty() {
            self.uniform_scalars.insert(name.to_owned(), (slot, ty.clone()));
        }
        self.uniforms.push(UniformBinding { name: name.to_owned(), group, binding, slot, fields });
        Ok(())
    }

    fn bind_params(&mut self, entry: &FunctionDef) -> Result<(), FrontendError> {
        for param in &entry.params {
            match attribute_argument(&param.attributes, "builtin").as_deref() {
                Some("global_invocation_id") => {
                    self.builtins.insert(param.name.clone(), Builtin::GlobalInvocationId);
                }
                Some(other) => {
                    return Err(unsupported(format!("builtin parameter `{other}`")));
                }
                None => {
                    return Err(unsupported(format!("entry parameter `{}` has no `builtin` attribute", param.name)));
                }
            }
        }
        Ok(())
    }

    fn map_type(&self, ty: &TypeRef) -> Result<Ty, FrontendError> {
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

    fn lower_block(&mut self, statements: &[Stmt]) -> Result<Vec<IrStmt>, FrontendError> {
        let saved = self.locals.clone();
        let result = self.lower_statements(statements);
        self.locals = saved;
        result
    }

    fn lower_statements(&mut self, statements: &[Stmt]) -> Result<Vec<IrStmt>, FrontendError> {
        let mut out = Vec::new();
        for statement in statements {
            self.lower_statement(statement, &mut out)?;
        }
        Ok(out)
    }

    fn lower_statement(&mut self, statement: &Stmt, out: &mut Vec<IrStmt>) -> Result<(), FrontendError> {
        match statement {
            Stmt::Var { name, ty, init, is_const } => {
                let init = init.as_ref().ok_or_else(|| unsupported(format!("declaration `{name}` without an initializer")))?;
                let (value, init_ty) = self.expr(init)?;
                let ty = match ty {
                    Some(ty) => self.map_type(ty)?,
                    None => init_ty,
                };
                let id = self.alloc_slot(ty.clone());
                self.locals.insert(name.clone(), Local { id, ty: ty.clone(), mutable: !is_const });
                out.push(IrStmt::Let { id, ty, init: value });
            }
            Stmt::Assign { target, op, value } => self.lower_assign(target, *op, value, out)?,
            Stmt::If { cond, then_body, else_body } => {
                let (cond, cond_ty) = self.expr(cond)?;
                if cond_ty != bool_ty() {
                    return Err(unsupported("`if` condition that is not boolean"));
                }
                let then_body = self.lower_block(then_body)?;
                let else_body = self.lower_block(else_body)?;
                out.push(IrStmt::If { cond, then_body, else_body });
            }
            Stmt::For { init, cond, update, body } => {
                self.lower_for(init.as_deref(), cond.as_ref(), update.as_deref(), body, out)?;
            }
            Stmt::While { cond, body } => {
                let (cond, cond_ty) = self.expr(cond)?;
                if cond_ty != bool_ty() {
                    return Err(unsupported("`while` condition that is not boolean"));
                }
                let body = self.lower_block(body)?;
                out.push(IrStmt::While { cond, limit: MAX_LOOP, body });
            }
            Stmt::DoWhile { body, cond } => {
                let (cond, cond_ty) = self.expr(cond)?;
                if cond_ty != bool_ty() {
                    return Err(unsupported("`do` condition that is not boolean"));
                }
                let mut loop_body = self.lower_block(body)?;
                loop_body.push(IrStmt::If { cond, then_body: Vec::new(), else_body: vec![IrStmt::Break] });
                out.push(IrStmt::While { cond: IrExpr::ConstBool(true), limit: MAX_LOOP, body: loop_body });
            }
            Stmt::Loop { body } => {
                let body = self.lower_block(body)?;
                out.push(IrStmt::While { cond: IrExpr::ConstBool(true), limit: MAX_LOOP, body });
            }
            Stmt::Break => out.push(IrStmt::Break),
            Stmt::Continue => out.push(IrStmt::Continue),
            Stmt::Discard => return Err(unsupported("`discard`")),
            Stmt::Return(None) => out.push(IrStmt::Return(IrExpr::ConstF32(0.0))),
            Stmt::Return(Some(_)) => return Err(unsupported("compute entry returning a value")),
            Stmt::Expr(expression) => {
                let (expression, _) = self.expr(expression)?;
                out.push(IrStmt::Expr(expression));
            }
            Stmt::Block(statements) => {
                out.extend(self.lower_block(statements)?);
            }
        }
        Ok(())
    }

    fn lower_assign(&mut self, target: &Expr, op: AssignOp, value: &Expr, out: &mut Vec<IrStmt>) -> Result<(), FrontendError> {
        match target {
            Expr::Ident(name) => {
                let local = self.locals.get(name).cloned().ok_or_else(|| unsupported(format!("assignment to unknown local `{name}`")))?;
                if !local.mutable {
                    return Err(unsupported(format!("assignment to immutable local `{name}`")));
                }
                let (value, value_ty) = self.expr(value)?;
                if value_ty != local.ty {
                    return Err(unsupported(format!("assignment to `{name}` with a mismatched type")));
                }
                let value = match assign_binop(op)? {
                    Some(binop) => IrExpr::BinOp { op: binop, lhs: Box::new(IrExpr::LoadLocal(local.id)), rhs: Box::new(value) },
                    None => value,
                };
                out.push(IrStmt::StoreLocal { id: local.id, value });
                Ok(())
            }
            Expr::Index { base, index } => {
                let Expr::Ident(name) = base.as_ref()
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
                    Some(binop) => IrExpr::BinOp {
                        op: binop,
                        lhs: Box::new(IrExpr::LoadBuffer { resource, index: Box::new(index.clone()) }),
                        rhs: Box::new(value),
                    },
                    None => value,
                };
                out.push(IrStmt::StoreBuffer { resource, index, value });
                Ok(())
            }
            _ => Err(unsupported("assignment target")),
        }
    }

    fn lower_for(
        &mut self,
        init: Option<&Stmt>,
        cond: Option<&Expr>,
        update: Option<&Stmt>,
        body: &[Stmt],
        out: &mut Vec<IrStmt>,
    ) -> Result<(), FrontendError> {
        let Some(Stmt::Var { name, init: Some(Expr::IntegerLiteral(0)), .. }) = init
        else {
            return Err(unsupported("`for` initializer must declare the loop variable at `0`"));
        };
        let limit = match cond {
            Some(Expr::Binary { op: BinaryOp::Lt, lhs, rhs }) => {
                match lhs.as_ref() {
                    Expr::Ident(inner) if inner == name => {}
                    _ => return Err(unsupported("`for` condition must compare the loop variable")),
                }
                match rhs.as_ref() {
                    Expr::IntegerLiteral(value) => u32::try_from(*value).map_err(|_| unsupported("negative `for` bound"))?,
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
        out.push(IrStmt::For { iter: id, limit, body });
        Ok(())
    }

    fn expr(&mut self, expression: &Expr) -> Result<(IrExpr, Ty), FrontendError> {
        match expression {
            Expr::IntegerLiteral(value) => {
                let value = u32::try_from(*value).map_err(|_| unsupported(format!("integer literal `{value}`")))?;
                Ok((IrExpr::ConstU32(value), u32_ty()))
            }
            Expr::FloatLiteral(value) => Ok((IrExpr::ConstF32(*value as f32), f32_ty())),
            Expr::BoolLiteral(value) => Ok((IrExpr::ConstBool(*value), bool_ty())),
            Expr::Ident(name) => self.binding_expr(name),
            Expr::Unary { op, expr } => {
                let (arg, ty) = self.expr(expr)?;
                match op {
                    UnaryOp::Neg if is_scalar(&ty) => Ok((IrExpr::UnOp { op: UnOp::Neg, arg: Box::new(arg) }, ty)),
                    UnaryOp::Neg => Err(unsupported("negation of a non-scalar value")),
                    UnaryOp::Not => Err(unsupported("logical negation")),
                    UnaryOp::BitNot => Err(unsupported("bitwise negation")),
                }
            }
            Expr::Binary { op, lhs, rhs } => {
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
                Ok((IrExpr::BinOp { op: binop, lhs: Box::new(lhs), rhs: Box::new(rhs) }, ty))
            }
            Expr::Call { callee, args } => self.call(callee, args),
            Expr::Member { base, field } => self.member(base, field),
            Expr::Index { base, index } => self.index(base, index),
            Expr::Paren(inner) => self.expr(inner),
        }
    }

    fn binding_expr(&mut self, name: &str) -> Result<(IrExpr, Ty), FrontendError> {
        if let Some(local) = self.locals.get(name) {
            return Ok((IrExpr::LoadLocal(local.id), local.ty.clone()));
        }
        if let Some(init) = self.consts.get(name).cloned() {
            return self.expr(&init);
        }
        if self.builtins.contains_key(name) {
            return Err(unsupported(format!("builtin `{name}` must be indexed by a component")));
        }
        if let Some((slot, ty)) = self.uniform_scalars.get(name) {
            return Ok((IrExpr::LoadUniform(*slot), ty.clone()));
        }
        if self.uniform_slots.contains_key(name) {
            return Err(unsupported(format!("uniform `{name}` must be accessed by field")));
        }
        if self.resource_slots.contains_key(name) {
            return Err(unsupported(format!("resource `{name}` must be indexed")));
        }
        Err(unsupported(format!("unknown binding `{name}`")))
    }

    fn call(&mut self, callee: &str, args: &[Expr]) -> Result<(IrExpr, Ty), FrontendError> {
        if callee == "vec3" {
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
            return Ok((IrExpr::Vec3Compose { x: Box::new(x.clone()), y: Box::new(y.clone()), z: Box::new(z.clone()) }, Ty::Vec3));
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
            return Ok((IrExpr::UnOp { op, arg: Box::new(value) }, f32_ty()));
        }
        if callee == "length" {
            let (value, ty) = self.single_arg(callee, args)?;
            if ty != Ty::Vec3 {
                return Err(unsupported("`length` on a non-`vec3` value"));
            }
            return Ok((IrExpr::UnOp { op: UnOp::Length, arg: Box::new(value) }, Ty::Scalar(ScalarTy::F64)));
        }
        if callee == "normalize" {
            let (value, ty) = self.single_arg(callee, args)?;
            if ty != Ty::Vec3 {
                return Err(unsupported("`normalize` on a non-`vec3` value"));
            }
            return Ok((IrExpr::UnOp { op: UnOp::Normalize, arg: Box::new(value) }, Ty::Vec3));
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
            return Ok((IrExpr::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, ty));
        }
        Err(unsupported(format!("call to `{callee}`")))
    }

    fn single_arg(&mut self, callee: &str, args: &[Expr]) -> Result<(IrExpr, Ty), FrontendError> {
        match args {
            [arg] => self.expr(arg),
            _ => Err(unsupported(format!("`{callee}` with {} arguments", args.len()))),
        }
    }

    fn member(&mut self, base: &Expr, field: &str) -> Result<(IrExpr, Ty), FrontendError> {
        if let Expr::Ident(name) = base {
            if self.builtins.get(name) == Some(&Builtin::GlobalInvocationId) && field == "x" {
                return Ok((IrExpr::GlobalId, u32_ty()));
            }
            if let Some(&index) = self.uniform_slots.get(name) {
                let uniform = &self.uniforms[index];
                let Some(position) = uniform.fields.iter().position(|(candidate, _)| candidate == field)
                else {
                    return Err(unsupported(format!("uniform `{name}` has no field `{field}`")));
                };
                let slot = uniform.slot + position as u32;
                let ty = uniform.fields[position].1.clone();
                return Ok((IrExpr::LoadUniform(slot), ty));
            }
        }
        Err(unsupported(format!("member access `.{field}`")))
    }

    fn index(&mut self, base: &Expr, index: &Expr) -> Result<(IrExpr, Ty), FrontendError> {
        let Expr::Ident(name) = base
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
        Ok((IrExpr::LoadBuffer { resource, index: Box::new(index) }, element))
    }

    fn alloc_slot(&mut self, ty: Ty) -> u32 {
        let id = self.next_slot;
        self.next_slot += 1;
        self.slots.push((id, ty));
        id
    }
}

fn has_attribute(attributes: &[Attribute], name: &str) -> bool {
    attributes.iter().any(|attribute| attribute.name == name)
}

fn attribute_integer(attributes: &[Attribute], name: &str) -> Option<u32> {
    let attribute = attributes.iter().find(|attribute| attribute.name == name)?;
    match attribute.args.as_slice() {
        [Expr::IntegerLiteral(value)] => u32::try_from(*value).ok(),
        _ => None,
    }
}

fn attribute_argument(attributes: &[Attribute], name: &str) -> Option<String> {
    let attribute = attributes.iter().find(|attribute| attribute.name == name)?;
    match attribute.args.as_slice() {
        [Expr::Ident(value)] => Some(value.clone()),
        _ => None,
    }
}

fn assign_binop(op: AssignOp) -> Result<Option<BinOp>, FrontendError> {
    match op {
        AssignOp::Assign => Ok(None),
        AssignOp::AddAssign => Ok(Some(BinOp::Add)),
        AssignOp::SubAssign => Ok(Some(BinOp::Sub)),
        AssignOp::MulAssign => Ok(Some(BinOp::Mul)),
        AssignOp::DivAssign => Ok(Some(BinOp::Div)),
        AssignOp::ModAssign => Ok(Some(BinOp::Mod)),
        other => Err(unsupported(format!("assignment operator `{other:?}`"))),
    }
}

fn binary_op(op: BinaryOp) -> Result<BinOp, FrontendError> {
    match op {
        BinaryOp::Add => Ok(BinOp::Add),
        BinaryOp::Sub => Ok(BinOp::Sub),
        BinaryOp::Mul => Ok(BinOp::Mul),
        BinaryOp::Div => Ok(BinOp::Div),
        BinaryOp::Mod => Ok(BinOp::Mod),
        BinaryOp::Lt => Ok(BinOp::Lt),
        BinaryOp::Le => Ok(BinOp::Le),
        BinaryOp::Eq => Ok(BinOp::Eq),
        BinaryOp::Ne => Ok(BinOp::Ne),
        BinaryOp::Ge => Ok(BinOp::Ge),
        BinaryOp::Gt => Ok(BinOp::Gt),
        other => Err(unsupported(format!("binary operator `{other:?}`"))),
    }
}

fn validate_for_update(update: &Stmt, name: &str) -> Result<(), FrontendError> {
    match update {
        Stmt::Assign { target: Expr::Ident(target), op: AssignOp::Assign, value: Expr::Binary { op: BinaryOp::Add, lhs, rhs } }
            if target == name =>
        {
            match (lhs.as_ref(), rhs.as_ref()) {
                (Expr::Ident(base), Expr::IntegerLiteral(1)) if base == name => Ok(()),
                _ => Err(unsupported("`for` update must be `i = i + 1`")),
            }
        }
        Stmt::Assign { target: Expr::Ident(target), op: AssignOp::AddAssign, value: Expr::IntegerLiteral(1) } if target == name => Ok(()),
        _ => Err(unsupported("`for` update must be `i = i + 1`")),
    }
}
