//! 递归下降语法分析器：TypeScript 子集 → [`language_typescript_ir::Program`]。
//!
//! 子集：`interface` / `type` 别名（解析并保留成员）、`export function` /
//! `export const`、`const` / `let` / `var`（可选 `: 类型`）、函数声明（形参与
//! 返回注解）、`if` / `else`、`while`、三段 `for`、`return`、`break` /
//! `continue`、块、表达式语句、赋值（标识符与成员访问）、对象字面量、成员
//! 访问、调用、二元 `+ - * / % === !== == != < <= > >= && ||`、一元 `! -`、
//! 三元 `?:` 与字面量。语法错误结构化（行列 + 期望）。

use language_typescript_ir::{
    statement::Statement,
    types::{AssignmentOp, BinaryOp, PrimitiveType, TypeAnnotation, UnaryOp},
    Expression, Program,
};
use language_typescript_types::TsValue;

use super::error::SyntaxError;
use super::lexer::{lex, Punct, Token, TokenKind};

/// 递归下降解析器状态。
pub struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    /// 从源码构造解析器（先完成词法分析）。
    pub fn new(source: &str) -> Result<Self, SyntaxError> {
        Ok(Self { tokens: lex(source)?, position: 0 })
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.position.min(self.tokens.len() - 1)]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if self.position + 1 < self.tokens.len() {
            self.position += 1;
        }
        token
    }

    fn at_punct(&self, punct: Punct) -> bool {
        matches!(self.peek_kind(), TokenKind::Punct(p) if *p == punct)
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek_kind(), TokenKind::Identifier if self.peek().text == keyword)
    }

    fn eat_punct(&mut self, punct: Punct) -> bool {
        if self.at_punct(punct) {
            self.advance();
            true
        }
        else {
            false
        }
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        if self.at_keyword(keyword) {
            self.advance();
            true
        }
        else {
            false
        }
    }

    fn expect_punct(&mut self, punct: Punct) -> Result<(), SyntaxError> {
        if self.eat_punct(punct) {
            Ok(())
        }
        else {
            let token = self.peek();
            Err(SyntaxError::new(format!("缺少 `{}`", punct.as_str()), punct.as_str(), token.line, token.column))
        }
    }

    fn expect_identifier(&mut self, what: &str) -> Result<String, SyntaxError> {
        match self.peek_kind() {
            TokenKind::Identifier => Ok(self.advance().text),
            _ => {
                let token = self.peek();
                Err(SyntaxError::new(format!("缺少{what}"), "标识符", token.line, token.column))
            }
        }
    }

    /// 语句结束符：`;`，或在 `}` / EOF 前省略。
    fn finish_statement(&mut self) -> Result<(), SyntaxError> {
        if self.eat_punct(Punct::Semicolon) {
            return Ok(());
        }
        if matches!(self.peek_kind(), TokenKind::Eof) || self.at_punct(Punct::BraceClose) {
            return Ok(());
        }
        let token = self.peek();
        Err(SyntaxError::new("语句未结束", "`;` 或语句块结束", token.line, token.column))
    }

    /// 解析整个程序。
    pub fn parse_program(&mut self) -> Result<Program, SyntaxError> {
        let mut statements = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::Eof) {
            statements.push(self.parse_statement()?);
        }
        Ok(Program { statements })
    }

    fn parse_statement(&mut self) -> Result<Statement, SyntaxError> {
        if self.at_keyword("export") {
            self.advance();
            let declaration = self.parse_declaration()?;
            return Ok(Statement::ExportDeclaration { declaration: Box::new(declaration), is_default: false });
        }
        if self.at_keyword("interface") || self.at_keyword("type") || self.at_keyword("function") || self.at_keyword("const") || self.at_keyword("let")
            || self.at_keyword("var")
        {
            return self.parse_declaration();
        }
        if self.at_punct(Punct::BraceOpen) {
            self.advance();
            let mut statements = Vec::new();
            while !self.at_punct(Punct::BraceClose) {
                if matches!(self.peek_kind(), TokenKind::Eof) {
                    let token = self.peek();
                    return Err(SyntaxError::new("块未闭合", "`}`", token.line, token.column));
                }
                statements.push(self.parse_statement()?);
            }
            self.expect_punct(Punct::BraceClose)?;
            return Ok(Statement::Block(statements));
        }
        if self.eat_keyword("if") {
            self.expect_punct(Punct::ParenOpen)?;
            let test = self.parse_expression()?;
            self.expect_punct(Punct::ParenClose)?;
            let consequent = self.parse_statement()?;
            let alternate = if self.eat_keyword("else") { Some(Box::new(self.parse_statement()?)) } else { None };
            return Ok(Statement::If { test: Box::new(test), consequent: Box::new(consequent), alternate });
        }
        if self.eat_keyword("while") {
            self.expect_punct(Punct::ParenOpen)?;
            let test = self.parse_expression()?;
            self.expect_punct(Punct::ParenClose)?;
            let body = self.parse_statement()?;
            return Ok(Statement::While { test: Box::new(test), body: Box::new(body) });
        }
        if self.eat_keyword("for") {
            self.expect_punct(Punct::ParenOpen)?;
            let init = if self.at_punct(Punct::Semicolon) {
                self.advance();
                None
            }
            else {
                let statement = if self.at_keyword("let") || self.at_keyword("const") || self.at_keyword("var") {
                    self.parse_declaration()? // 声明自带 `;` 消费。
                }
                else {
                    let expression = Statement::Expression(Box::new(self.parse_expression()?));
                    self.expect_punct(Punct::Semicolon)?;
                    expression
                };
                Some(Box::new(statement))
            };
            let test = if self.at_punct(Punct::Semicolon) { None } else { Some(self.parse_expression()?) };
            self.expect_punct(Punct::Semicolon)?;
            let update = if self.at_punct(Punct::ParenClose) { None } else { Some(self.parse_expression()?) };
            self.expect_punct(Punct::ParenClose)?;
            let body = self.parse_statement()?;
            return Ok(Statement::For {
                init,
                test: test.map(Box::new),
                update: update.map(Box::new),
                body: Box::new(body),
            });
        }
        if self.eat_keyword("return") {
            let value = if self.at_punct(Punct::Semicolon) || self.at_punct(Punct::BraceClose) || matches!(self.peek_kind(), TokenKind::Eof) {
                None
            }
            else {
                Some(self.parse_expression()?)
            };
            self.finish_statement()?;
            return Ok(Statement::Return(value.map(Box::new)));
        }
        if self.eat_keyword("break") {
            self.finish_statement()?;
            return Ok(Statement::Break);
        }
        if self.eat_keyword("continue") {
            self.finish_statement()?;
            return Ok(Statement::Continue);
        }
        let expression = self.parse_expression()?;
        self.finish_statement()?;
        Ok(Statement::Expression(Box::new(expression)))
    }

    fn parse_declaration(&mut self) -> Result<Statement, SyntaxError> {
        if self.eat_keyword("interface") {
            let name = self.expect_identifier("接口名")?;
            self.expect_punct(Punct::BraceOpen)?;
            let mut members = Vec::new();
            while !self.at_punct(Punct::BraceClose) {
                if matches!(self.peek_kind(), TokenKind::Eof) {
                    let token = self.peek();
                    return Err(SyntaxError::new("接口体未闭合", "`}`", token.line, token.column));
                }
                let member_name = self.expect_identifier("接口成员名")?;
                let optional = self.eat_punct(Punct::Question);
                if self.at_punct(Punct::ParenOpen) {
                    self.advance();
                    let mut params = Vec::new();
                    while !self.at_punct(Punct::ParenClose) {
                        let param = self.expect_identifier("参数名")?;
                        let ty = if self.eat_punct(Punct::Colon) { self.parse_type()? } else { TypeAnnotation::Any };
                        params.push((param, ty));
                        if !self.eat_punct(Punct::Comma) {
                            break;
                        }
                    }
                    self.expect_punct(Punct::ParenClose)?;
                    let return_type = if self.eat_punct(Punct::Colon) { self.parse_type()? } else { TypeAnnotation::Void };
                    members.push(language_typescript_ir::program::InterfaceMember::Method { name: member_name, params, return_type, optional });
                }
                else {
                    self.expect_punct(Punct::Colon)?;
                    let ty = self.parse_type()?;
                    members.push(language_typescript_ir::program::InterfaceMember::Property { name: member_name, ty, optional });
                }
                let _ = self.eat_punct(Punct::Semicolon) || self.eat_punct(Punct::Comma);
            }
            self.expect_punct(Punct::BraceClose)?;
            return Ok(Statement::InterfaceDeclaration { name, type_params: Vec::new(), extends: Vec::new(), members });
        }
        if self.eat_keyword("type") {
            let name = self.expect_identifier("类型别名名")?;
            self.expect_punct(Punct::Eq)?;
            let ty = self.parse_type()?;
            self.finish_statement()?;
            return Ok(Statement::TypeAlias { name, type_params: Vec::new(), ty });
        }
        if self.eat_keyword("function") {
            let name = self.expect_identifier("函数名")?;
            self.expect_punct(Punct::ParenOpen)?;
            let mut params = Vec::new();
            let mut param_types = Vec::new();
            while !self.at_punct(Punct::ParenClose) {
                let param = self.expect_identifier("参数名")?;
                let ty = if self.eat_punct(Punct::Colon) { Some(self.parse_type()?) } else { None };
                params.push(param);
                param_types.push(ty);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::ParenClose)?;
            let return_type = if self.eat_punct(Punct::Colon) { Some(self.parse_type()?) } else { None };
            self.expect_punct(Punct::BraceOpen)?;
            let mut body = Vec::new();
            while !self.at_punct(Punct::BraceClose) {
                if matches!(self.peek_kind(), TokenKind::Eof) {
                    let token = self.peek();
                    return Err(SyntaxError::new("函数体未闭合", "`}`", token.line, token.column));
                }
                body.push(self.parse_statement()?);
            }
            self.expect_punct(Punct::BraceClose)?;
            return Ok(Statement::FunctionDeclaration {
                name,
                type_params: Vec::new(),
                params,
                param_types,
                return_type,
                body,
                decorators: Vec::new(),
            });
        }
        let is_const = self.at_keyword("const");
        if !self.eat_keyword("const") && !self.eat_keyword("let") && !self.eat_keyword("var") {
            let token = self.peek();
            return Err(SyntaxError::new("期望声明", "`const` / `let` / `var` / `function` / `interface` / `type`", token.line, token.column));
        }
        let name = self.expect_identifier("变量名")?;
        let ty = if self.eat_punct(Punct::Colon) { Some(self.parse_type()?) } else { None };
        let initializer = if self.eat_punct(Punct::Eq) { Some(Box::new(self.parse_expression()?)) } else { None };
        self.finish_statement()?;
        Ok(Statement::VariableDeclaration { name, ty, initializer, is_const })
    }

    // ---- 类型注解 ----

    fn parse_type(&mut self) -> Result<TypeAnnotation, SyntaxError> {
        let first = self.parse_type_primary()?;
        if self.at_punct(Punct::Bar) {
            let mut parts = vec![first];
            while self.eat_punct(Punct::Bar) {
                parts.push(self.parse_type_primary()?);
            }
            return Ok(TypeAnnotation::Union(parts));
        }
        Ok(first)
    }

    fn parse_type_primary(&mut self) -> Result<TypeAnnotation, SyntaxError> {
        let mut ty = self.parse_type_atom()?;
        while self.at_punct(Punct::BracketOpen) {
            self.advance();
            self.expect_punct(Punct::BracketClose)?;
            ty = TypeAnnotation::Array(Box::new(ty));
        }
        Ok(ty)
    }

    fn parse_type_atom(&mut self) -> Result<TypeAnnotation, SyntaxError> {
        if self.eat_punct(Punct::BraceOpen) {
            let mut fields = Vec::new();
            while !self.at_punct(Punct::BraceClose) {
                if matches!(self.peek_kind(), TokenKind::Eof) {
                    let token = self.peek();
                    return Err(SyntaxError::new("对象类型未闭合", "`}`", token.line, token.column));
                }
                let name = match self.peek_kind() {
                    TokenKind::Identifier | TokenKind::Str => self.advance().text,
                    _ => {
                        let token = self.peek();
                        return Err(SyntaxError::new("对象类型字段名非法", "字段名", token.line, token.column));
                    }
                };
                let _ = self.eat_punct(Punct::Question);
                self.expect_punct(Punct::Colon)?;
                let ty = self.parse_type()?;
                fields.push((name, ty));
                if !self.eat_punct(Punct::Comma) && !self.eat_punct(Punct::Semicolon) {
                    break;
                }
            }
            self.expect_punct(Punct::BraceClose)?;
            return Ok(TypeAnnotation::Object(fields));
        }
        if self.eat_punct(Punct::ParenOpen) {
            let ty = self.parse_type()?;
            self.expect_punct(Punct::ParenClose)?;
            return Ok(ty);
        }
        let token = self.advance();
        if token.kind != TokenKind::Identifier {
            return Err(SyntaxError::new("类型注解位置非法", "类型名 / 对象类型 / 数组类型", token.line, token.column));
        }
        Ok(match token.text.as_str() {
            "number" => TypeAnnotation::Primitive(PrimitiveType::Number),
            "string" => TypeAnnotation::Primitive(PrimitiveType::String),
            "boolean" => TypeAnnotation::Primitive(PrimitiveType::Boolean),
            "symbol" => TypeAnnotation::Primitive(PrimitiveType::Symbol),
            "bigint" => TypeAnnotation::Primitive(PrimitiveType::BigInt),
            "null" => TypeAnnotation::Primitive(PrimitiveType::Null),
            "undefined" => TypeAnnotation::Primitive(PrimitiveType::Undefined),
            "void" => TypeAnnotation::Void,
            "any" => TypeAnnotation::Any,
            "unknown" => TypeAnnotation::Unknown,
            "never" => TypeAnnotation::Never,
            _ => TypeAnnotation::TypeReference(token.text),
        })
    }

    // ---- 表达式 ----

    fn parse_expression(&mut self) -> Result<Expression, SyntaxError> {
        let left = self.parse_conditional()?;
        if self.at_punct(Punct::Eq) {
            match left {
                Expression::Identifier(_) | Expression::Member { .. } | Expression::Index { .. } => {
                    self.advance();
                    let right = self.parse_expression()?;
                    return Ok(Expression::Assignment { left: Box::new(left), op: AssignmentOp::Assign, right: Box::new(right) });
                }
                _ => {
                    let token = self.peek();
                    return Err(SyntaxError::new("赋值左侧不可写", "标识符或成员访问", token.line, token.column));
                }
            }
        }
        Ok(left)
    }

    fn parse_conditional(&mut self) -> Result<Expression, SyntaxError> {
        let test = self.parse_logical_or()?;
        if self.eat_punct(Punct::Question) {
            let consequent = self.parse_expression()?;
            self.expect_punct(Punct::Colon)?;
            let alternate = self.parse_conditional()?;
            return Ok(Expression::Conditional { test: Box::new(test), consequent: Box::new(consequent), alternate: Box::new(alternate) });
        }
        Ok(test)
    }

    fn parse_logical_or(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_logical_and()?;
        while self.eat_punct(Punct::PipePipe) {
            let right = self.parse_logical_and()?;
            left = Expression::Binary { left: Box::new(left), op: BinaryOp::Or, right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_logical_and(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_equality()?;
        while self.eat_punct(Punct::AmpAmp) {
            let right = self.parse_equality()?;
            left = Expression::Binary { left: Box::new(left), op: BinaryOp::And, right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_relational()?;
        loop {
            let op = if self.eat_punct(Punct::StrictEq) {
                BinaryOp::StrictEq
            }
            else if self.eat_punct(Punct::StrictNeq) {
                BinaryOp::StrictNeq
            }
            else if self.eat_punct(Punct::EqEq) {
                BinaryOp::Eq
            }
            else if self.eat_punct(Punct::NotEq) {
                BinaryOp::Neq
            }
            else {
                break;
            };
            let right = self.parse_relational()?;
            left = Expression::Binary { left: Box::new(left), op, right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_relational(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_additive()?;
        loop {
            let op = if self.eat_punct(Punct::Lt) {
                BinaryOp::Lt
            }
            else if self.eat_punct(Punct::LtEq) {
                BinaryOp::Lte
            }
            else if self.eat_punct(Punct::Gt) {
                BinaryOp::Gt
            }
            else if self.eat_punct(Punct::GtEq) {
                BinaryOp::Gte
            }
            else {
                break;
            };
            let right = self.parse_additive()?;
            left = Expression::Binary { left: Box::new(left), op, right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = if self.eat_punct(Punct::Plus) {
                BinaryOp::Add
            }
            else if self.eat_punct(Punct::Minus) {
                BinaryOp::Sub
            }
            else {
                break;
            };
            let right = self.parse_multiplicative()?;
            left = Expression::Binary { left: Box::new(left), op, right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expression, SyntaxError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = if self.eat_punct(Punct::Star) {
                BinaryOp::Mul
            }
            else if self.eat_punct(Punct::Slash) {
                BinaryOp::Div
            }
            else if self.eat_punct(Punct::Percent) {
                BinaryOp::Mod
            }
            else {
                break;
            };
            let right = self.parse_unary()?;
            left = Expression::Binary { left: Box::new(left), op, right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expression, SyntaxError> {
        if self.eat_punct(Punct::Bang) {
            let expr = self.parse_unary()?;
            return Ok(Expression::Unary { op: UnaryOp::Not, expr: Box::new(expr) });
        }
        if self.eat_punct(Punct::Minus) {
            let expr = self.parse_unary()?;
            return Ok(Expression::Unary { op: UnaryOp::Neg, expr: Box::new(expr) });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expression, SyntaxError> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat_punct(Punct::Dot) {
                let property = self.expect_identifier("属性名")?;
                expr = Expression::Member { object: Box::new(expr), property: Box::new(Expression::Identifier(property)) };
            }
            else if self.at_punct(Punct::BracketOpen) {
                self.advance();
                let index = self.parse_expression()?;
                self.expect_punct(Punct::BracketClose)?;
                expr = Expression::Index { object: Box::new(expr), index: Box::new(index) };
            }
            else if self.at_punct(Punct::ParenOpen) {
                self.advance();
                let mut args = Vec::new();
                while !self.at_punct(Punct::ParenClose) {
                    args.push(self.parse_expression()?);
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
                self.expect_punct(Punct::ParenClose)?;
                expr = Expression::Call { callee: Box::new(expr), args };
            }
            else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expression, SyntaxError> {
        let token = self.peek().clone();
        match &token.kind {
            TokenKind::Number(value) => {
                self.advance();
                Ok(Expression::Literal(TsValue::Number(*value)))
            }
            TokenKind::Str => {
                self.advance();
                Ok(Expression::Literal(TsValue::String(token.text)))
            }
            TokenKind::Identifier => {
                self.advance();
                Ok(match token.text.as_str() {
                    "true" => Expression::Literal(TsValue::Boolean(true)),
                    "false" => Expression::Literal(TsValue::Boolean(false)),
                    "null" => Expression::Literal(TsValue::Null),
                    "undefined" => Expression::Literal(TsValue::Undefined),
                    _ => Expression::Identifier(token.text),
                })
            }
            TokenKind::Punct(Punct::ParenOpen) => {
                self.advance();
                let expr = self.parse_expression()?;
                self.expect_punct(Punct::ParenClose)?;
                Ok(expr)
            }
            TokenKind::Punct(Punct::BraceOpen) => {
                self.advance();
                let mut fields = Vec::new();
                while !self.at_punct(Punct::BraceClose) {
                    if matches!(self.peek_kind(), TokenKind::Eof) {
                        return Err(SyntaxError::new("对象字面量未闭合", "`}`", token.line, token.column));
                    }
                    let key = match self.peek_kind() {
                        TokenKind::Identifier | TokenKind::Str => self.advance().text,
                        _ => {
                            let key_token = self.peek();
                            return Err(SyntaxError::new("对象字面量键非法", "标识符或字符串键", key_token.line, key_token.column));
                        }
                    };
                    self.expect_punct(Punct::Colon)?;
                    let value = self.parse_expression()?;
                    fields.push((key, value));
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
                self.expect_punct(Punct::BraceClose)?;
                Ok(Expression::Object(fields))
            }
            TokenKind::Punct(Punct::BracketOpen) => {
                self.advance();
                let mut elements = Vec::new();
                while !self.at_punct(Punct::BracketClose) {
                    elements.push(self.parse_expression()?);
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
                self.expect_punct(Punct::BracketClose)?;
                Ok(Expression::Array(elements))
            }
            _ => Err(SyntaxError::new("表达式位置非法", "字面量 / 标识符 / 调用 / 对象或数组字面量", token.line, token.column)),
        }
    }
}

/// 解析一段 TypeScript 子集源码为 IR 程序。
pub fn parse_program(source: &str) -> Result<Program, SyntaxError> {
    Parser::new(source)?.parse_program()
}
