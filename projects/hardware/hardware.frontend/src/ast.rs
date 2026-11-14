//! Language-neutral shader syntax consumed by the shared UCF lowering.
//!
//! Each `hardware-input-*` crate converts its own parser output into these
//! types. The lowering engine never sees language-specific spellings.

/// One authored shader module.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShaderModule {
    /// Module-scope items in source order.
    pub items: Vec<ModuleItem>,
}

/// A module-scope item.
#[derive(Debug, Clone, PartialEq)]
pub enum ModuleItem {
    /// Function definition.
    Function(FunctionDef),
    /// Struct definition.
    Struct(StructDef),
    /// Mutable module-scope variable.
    GlobalVar(GlobalVar),
    /// Immutable module-scope constant.
    Const(ConstDef),
    /// Type alias.
    TypeAlias(TypeAlias),
}

/// An attribute or qualifier attached to a declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    /// Normalized lowercase name without any sigil.
    pub name: String,
    /// Attribute arguments.
    pub args: Vec<Expr>,
}

/// A function definition.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionDef {
    /// Attached attributes such as `compute`.
    pub attributes: Vec<Attribute>,
    /// Function name.
    pub name: String,
    /// Parameters.
    pub params: Vec<Param>,
    /// Declared return type.
    pub return_type: Option<TypeRef>,
    /// Function body.
    pub body: Vec<Stmt>,
}

/// A function parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    /// Attached attributes such as `builtin`.
    pub attributes: Vec<Attribute>,
    /// Parameter name.
    pub name: String,
    /// Parameter type.
    pub ty: TypeRef,
}

/// A struct definition.
#[derive(Debug, Clone, PartialEq)]
pub struct StructDef {
    /// Struct name.
    pub name: String,
    /// Fields in declaration order.
    pub fields: Vec<StructField>,
}

/// One struct field.
#[derive(Debug, Clone, PartialEq)]
pub struct StructField {
    /// Attached attributes such as `location`.
    pub attributes: Vec<Attribute>,
    /// Field name.
    pub name: String,
    /// Field type.
    pub ty: TypeRef,
}

/// A mutable module-scope variable.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalVar {
    /// Attached attributes such as `group` or `binding`.
    pub attributes: Vec<Attribute>,
    /// Declared address space.
    pub address_space: Option<AddressSpace>,
    /// Declared access mode.
    pub access: Option<AccessMode>,
    /// Variable name.
    pub name: String,
    /// Declared type.
    pub ty: Option<TypeRef>,
    /// Initializer.
    pub init: Option<Expr>,
}

/// An immutable module-scope constant.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstDef {
    /// Constant name.
    pub name: String,
    /// Declared type.
    pub ty: Option<TypeRef>,
    /// Constant value.
    pub init: Expr,
}

/// A type alias.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeAlias {
    /// Alias name.
    pub name: String,
    /// Aliased type.
    pub ty: TypeRef,
}

/// Address space of a module-scope variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSpace {
    /// Function-local storage.
    Function,
    /// Per-invocation private storage.
    Private,
    /// Workgroup-shared storage.
    Workgroup,
    /// Read-only uniform storage.
    Uniform,
    /// Read-write storage buffer.
    Storage,
}

/// Access mode of a module-scope variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Read-only.
    Read,
    /// Write-only.
    Write,
    /// Read-write.
    ReadWrite,
}

/// A type reference.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeRef {
    /// Type name, for example `f32` or `array`.
    pub name: String,
    /// Generic arguments, for example `[f32, 64]` for `array<f32, 64>`.
    pub args: Vec<TypeRef>,
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// A local declaration.
    Var {
        /// Declared name.
        name: String,
        /// Declared type.
        ty: Option<TypeRef>,
        /// Initializer.
        init: Option<Expr>,
        /// Whether the binding is immutable.
        is_const: bool,
    },
    /// An assignment, including compound assignments.
    Assign {
        /// Assignment target.
        target: Expr,
        /// Assignment operator.
        op: AssignOp,
        /// Assigned value.
        value: Expr,
    },
    /// A conditional.
    If {
        /// Condition.
        cond: Expr,
        /// Body executed when the condition holds.
        then_body: Vec<Stmt>,
        /// Body executed otherwise.
        else_body: Vec<Stmt>,
    },
    /// A counted loop.
    For {
        /// Loop initializer.
        init: Option<Box<Stmt>>,
        /// Loop condition.
        cond: Option<Expr>,
        /// Loop update.
        update: Option<Box<Stmt>>,
        /// Loop body.
        body: Vec<Stmt>,
    },
    /// A conditional loop.
    While {
        /// Loop condition.
        cond: Expr,
        /// Loop body.
        body: Vec<Stmt>,
    },
    /// An unbounded loop.
    Loop {
        /// Loop body.
        body: Vec<Stmt>,
    },
    /// `break`.
    Break,
    /// `continue`.
    Continue,
    /// `discard`.
    Discard,
    /// A return with an optional value.
    Return(Option<Expr>),
    /// An expression used as a statement.
    Expr(Expr),
    /// A nested block.
    Block(Vec<Stmt>),
}

/// An assignment operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`.
    Assign,
    /// `+=`.
    AddAssign,
    /// `-=`.
    SubAssign,
    /// `*=`.
    MulAssign,
    /// `/=`.
    DivAssign,
    /// `%=`.
    ModAssign,
    /// `&=`.
    AndAssign,
    /// `|=`.
    OrAssign,
    /// `^=`.
    XorAssign,
    /// `<<=`.
    ShlAssign,
    /// `>>=`.
    ShrAssign,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Integer literal.
    IntegerLiteral(i64),
    /// Floating-point literal.
    FloatLiteral(f64),
    /// Boolean literal.
    BoolLiteral(bool),
    /// A name reference.
    Ident(String),
    /// A prefix operation.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        expr: Box<Expr>,
    },
    /// A binary operation.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// A call or constructor.
    Call {
        /// Callee name, possibly including generic arguments.
        callee: String,
        /// Arguments.
        args: Vec<Expr>,
    },
    /// Member access.
    Member {
        /// Base expression.
        base: Box<Expr>,
        /// Field name.
        field: String,
    },
    /// Index access.
    Index {
        /// Base expression.
        base: Box<Expr>,
        /// Index expression.
        index: Box<Expr>,
    },
    /// A parenthesized expression.
    Paren(Box<Expr>),
}

/// A prefix operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-`.
    Neg,
    /// `!`.
    Not,
    /// `~`.
    BitNot,
}

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Mod,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `&&`.
    And,
    /// `||`.
    Or,
    /// `&`.
    BitAnd,
    /// `|`.
    BitOr,
    /// `^`.
    BitXor,
    /// `<<`.
    Shl,
    /// `>>`.
    Shr,
}
