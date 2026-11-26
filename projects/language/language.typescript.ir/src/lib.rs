#![warn(missing_docs)]

//! TypeScript 的 AST 表示（**不是 MIR**）。
//!
//! 本 crate 是 TypeScript 的 AST 表示：只承载前端解析产出的语法树形状
//! （表达式 / 语句 / 类型 / 访问器），供 `language.typescript` 语法层构造
//! 与 HIR 降低消费。语言自己的 HIR / MIR / 残差 IR 归
//! `language.typescript` crate 所有，本 crate 不做任何中间表示。

// 子模块
pub mod expression;
pub mod program;
pub mod statement;
pub mod types;
pub mod visitor;

// 从子模块重新导出常用类型
pub use types::{AssignmentOp, BinaryOp, PrimitiveType, TypeAnnotation, TypeParameter, UnaryOp};

pub use expression::{Expression, TypedExpression};

pub use statement::{ImportSpecifier, Statement, TypedStatement};

pub use program::{
    CFGNode, Class, ControlFlowGraph, Decorator, DecoratorTarget, EnumMember, Function, InterfaceMember, Method, Module,
    Program, TypedFunction, TypedModule, TypedProgram,
};

pub use visitor::{SimpleVisitor, Visitor};
