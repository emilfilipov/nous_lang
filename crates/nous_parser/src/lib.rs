use nous_lexer::{Diagnostic, Keyword, Span, Token, TokenKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub functions: Vec<Function>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: TypeRef,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    pub name: String,
}

impl TypeRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub fn is_void(&self) -> bool {
        self.name == "void"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Let {
        name: String,
        ty: TypeRef,
        value: Expr,
        span: Span,
    },
    Assign {
        name: String,
        op: AssignOp,
        value: Expr,
        span: Span,
    },
    Return(Option<Expr>),
    Break(Span),
    Continue(Span),
    Expr(Expr),
    If {
        branches: Vec<IfBranch>,
        else_body: Vec<Stmt>,
        span: Span,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    Loop {
        body: Vec<Stmt>,
        span: Span,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Replace,
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfBranch {
    pub condition: Expr,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    Integer(i64),
    Bool(bool),
    String(String),
    Variable(String),
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

pub fn parse(tokens: &[Token]) -> Result<Program, Vec<Diagnostic>> {
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program();
    if parser.diagnostics.is_empty() {
        Ok(program)
    } else {
        Err(parser.diagnostics)
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            cursor: 0,
            diagnostics: Vec::new(),
        }
    }

    fn parse_program(&mut self) -> Program {
        let mut functions = Vec::new();
        self.skip_newlines();

        while !self.at(TokenKindRef::Eof) {
            if self.eat_keyword(Keyword::Fn).is_some() {
                if let Some(function) = self.parse_function() {
                    functions.push(function);
                }
            } else {
                let token = self.peek();
                self.error(
                    "N0201",
                    "expected top-level function declaration",
                    token.span,
                );
                self.advance();
            }
            self.skip_newlines();
        }

        Program { functions }
    }

    fn parse_function(&mut self) -> Option<Function> {
        let fn_span = self.previous().span;
        let name = self.expect_identifier("expected function name after `fn`")?;
        let mut params = Vec::new();

        while !self.at(TokenKindRef::Arrow)
            && !self.at(TokenKindRef::Newline)
            && !self.at(TokenKindRef::Eof)
        {
            let param_name = self.expect_identifier("expected parameter name")?;
            let ty = self.expect_type("expected parameter type")?;
            params.push(Param {
                name: param_name,
                ty,
            });
        }

        if !self.eat(TokenKindRef::Arrow) {
            self.error(
                "N0202",
                "expected `->` before function return type",
                self.peek().span,
            );
            return None;
        }

        let return_type = self.expect_type("expected function return type after `->`")?;
        self.expect_newline("expected newline after function signature");
        self.expect(TokenKindRef::Indent, "expected indented function body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected function body dedent")?;

        Some(Function {
            name,
            params,
            return_type,
            body,
            span: fn_span,
        })
    }

    fn parse_block(&mut self, ends: &[BlockEnd]) -> Vec<Stmt> {
        let mut statements = Vec::new();
        self.skip_newlines();

        while !self.at(TokenKindRef::Eof) && !self.is_block_end(ends) {
            if let Some(stmt) = self.parse_statement() {
                statements.push(stmt);
            } else {
                self.advance();
            }
            self.skip_newlines();
        }

        statements
    }

    fn parse_statement(&mut self) -> Option<Stmt> {
        if self.eat_keyword(Keyword::Let).is_some() {
            return self.parse_let();
        }

        if self.eat_keyword(Keyword::Return).is_some() {
            let span = self.previous().span;
            let expr = if self.at(TokenKindRef::Newline) {
                None
            } else {
                Some(self.parse_expr_line(span)?)
            };
            self.expect_newline("expected newline after return statement");
            return Some(Stmt::Return(expr));
        }

        if self.eat_keyword(Keyword::Break).is_some() {
            let span = self.previous().span;
            self.expect_newline("expected newline after break statement");
            return Some(Stmt::Break(span));
        }

        if self.eat_keyword(Keyword::Continue).is_some() {
            let span = self.previous().span;
            self.expect_newline("expected newline after continue statement");
            return Some(Stmt::Continue(span));
        }

        if self.eat_keyword(Keyword::If).is_some() {
            return self.parse_if();
        }

        if self.eat_keyword(Keyword::While).is_some() {
            return self.parse_while();
        }

        if self.eat_keyword(Keyword::Loop).is_some() {
            return self.parse_loop();
        }

        if self.next_is_assignment() {
            return self.parse_assignment();
        }

        let span = self.peek().span;
        let expr = self.parse_expr_line(span)?;
        self.expect_newline("expected newline after expression");
        Some(Stmt::Expr(expr))
    }

    fn parse_let(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected binding name after `let`")?;
        let ty = self.expect_type("expected binding type")?;
        if !self.eat_symbol("=") {
            self.error("N0206", "expected `=` in let binding", self.peek().span);
            return None;
        }
        let value = self.parse_expr_line(span)?;
        self.expect_newline("expected newline after let binding");
        Some(Stmt::Let {
            name,
            ty,
            value,
            span,
        })
    }

    fn parse_assignment(&mut self) -> Option<Stmt> {
        let span = self.peek().span;
        let name = self.expect_identifier("expected assignment target")?;
        let op = self.expect_assignment_op()?;
        let value = self.parse_expr_line(span)?;
        self.expect_newline("expected newline after assignment");
        Some(Stmt::Assign {
            name,
            op,
            value,
            span,
        })
    }

    fn parse_if(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        let first_condition = self.parse_expr_line(span)?;
        self.expect_newline("expected newline after if condition");
        self.expect(TokenKindRef::Indent, "expected indented if body")?;
        let first_body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected if body dedent")?;

        let mut branches = vec![IfBranch {
            condition: first_condition,
            body: first_body,
        }];
        let mut else_body = Vec::new();

        loop {
            self.skip_newlines();
            if self.eat_keyword(Keyword::Elif).is_some() {
                let branch_span = self.previous().span;
                let condition = self.parse_expr_line(branch_span)?;
                self.expect_newline("expected newline after elif condition");
                self.expect(TokenKindRef::Indent, "expected indented elif body")?;
                let body = self.parse_block(&[BlockEnd::Dedent]);
                self.expect(TokenKindRef::Dedent, "expected elif body dedent")?;
                branches.push(IfBranch { condition, body });
                continue;
            }

            if self.eat_keyword(Keyword::Else).is_some() {
                self.expect_newline("expected newline after else");
                self.expect(TokenKindRef::Indent, "expected indented else body")?;
                else_body = self.parse_block(&[BlockEnd::Dedent]);
                self.expect(TokenKindRef::Dedent, "expected else body dedent")?;
            }
            break;
        }

        Some(Stmt::If {
            branches,
            else_body,
            span,
        })
    }

    fn parse_while(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        let condition = self.parse_expr_line(span)?;
        self.expect_newline("expected newline after while condition");
        self.expect(TokenKindRef::Indent, "expected indented while body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected while body dedent")?;
        Some(Stmt::While {
            condition,
            body,
            span,
        })
    }

    fn parse_loop(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        self.expect_newline("expected newline after loop");
        self.expect(TokenKindRef::Indent, "expected indented loop body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected loop body dedent")?;
        Some(Stmt::Loop { body, span })
    }

    fn parse_expr_line(&mut self, fallback_span: Span) -> Option<Expr> {
        let start = self.cursor;
        while !self.at(TokenKindRef::Newline)
            && !self.at(TokenKindRef::Indent)
            && !self.at(TokenKindRef::Dedent)
            && !self.at(TokenKindRef::Eof)
        {
            self.advance();
        }

        let mut expr_parser = ExprParser::new(&self.tokens[start..self.cursor]);
        match expr_parser.parse() {
            Ok(expr) => Some(expr),
            Err(message) => {
                self.error("N0207", message, fallback_span);
                None
            }
        }
    }

    fn expect_type(&mut self, message: &'static str) -> Option<TypeRef> {
        match &self.peek().kind {
            TokenKind::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Some(TypeRef::new(name))
            }
            TokenKind::Keyword(Keyword::Void) => {
                self.advance();
                Some(TypeRef::new("void"))
            }
            _ => {
                self.error("N0203", message, self.peek().span);
                None
            }
        }
    }

    fn expect_identifier(&mut self, message: &'static str) -> Option<String> {
        match &self.peek().kind {
            TokenKind::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Some(name)
            }
            _ => {
                self.error("N0204", message, self.peek().span);
                None
            }
        }
    }

    fn expect_newline(&mut self, message: &'static str) -> bool {
        self.expect(TokenKindRef::Newline, message).is_some()
    }

    fn expect(&mut self, kind: TokenKindRef, message: &'static str) -> Option<Token> {
        if self.eat(kind) {
            Some(self.previous().clone())
        } else {
            self.error("N0205", message, self.peek().span);
            None
        }
    }

    fn eat(&mut self, kind: TokenKindRef) -> bool {
        if self.at(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        if matches!(&self.peek().kind, TokenKind::Symbol(actual) if actual == symbol) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn next_is_assignment(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Identifier(_))
            && matches!(
                self.tokens.get(self.cursor + 1).map(|token| &token.kind),
                Some(TokenKind::Symbol(symbol))
                    if matches!(symbol.as_str(), "=" | "+=" | "-=" | "*=" | "/=")
            )
    }

    fn expect_assignment_op(&mut self) -> Option<AssignOp> {
        let TokenKind::Symbol(symbol) = &self.peek().kind else {
            self.error("N0208", "expected assignment operator", self.peek().span);
            return None;
        };
        let op = match symbol.as_str() {
            "=" => AssignOp::Replace,
            "+=" => AssignOp::Add,
            "-=" => AssignOp::Subtract,
            "*=" => AssignOp::Multiply,
            "/=" => AssignOp::Divide,
            _ => {
                self.error("N0208", "expected assignment operator", self.peek().span);
                return None;
            }
        };
        self.advance();
        Some(op)
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> Option<Token> {
        if matches!(&self.peek().kind, TokenKind::Keyword(actual) if *actual == keyword) {
            self.advance();
            Some(self.previous().clone())
        } else {
            None
        }
    }

    fn skip_newlines(&mut self) {
        while self.at(TokenKindRef::Newline) {
            self.advance();
        }
    }

    fn at(&self, kind: TokenKindRef) -> bool {
        matches!(
            (kind, &self.peek().kind),
            (TokenKindRef::Arrow, TokenKind::Arrow)
                | (TokenKindRef::Newline, TokenKind::Newline)
                | (TokenKindRef::Indent, TokenKind::Indent)
                | (TokenKindRef::Dedent, TokenKind::Dedent)
                | (TokenKindRef::Eof, TokenKind::Eof)
        )
    }

    fn is_block_end(&self, ends: &[BlockEnd]) -> bool {
        ends.iter().any(|end| match end {
            BlockEnd::Dedent => self.at(TokenKindRef::Dedent),
        })
    }

    fn advance(&mut self) {
        if self.cursor < self.tokens.len().saturating_sub(1) {
            self.cursor += 1;
        }
    }

    fn peek(&self) -> &'a Token {
        &self.tokens[self.cursor]
    }

    fn previous(&self) -> &'a Token {
        &self.tokens[self.cursor.saturating_sub(1)]
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>, span: Span) {
        self.diagnostics.push(Diagnostic::new(code, message, span));
    }
}

struct ExprParser<'a> {
    tokens: &'a [Token],
    cursor: usize,
}

impl<'a> ExprParser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, cursor: 0 }
    }

    fn parse(&mut self) -> Result<Expr, String> {
        let expr = self.parse_binary(0)?;
        if !self.is_at_end() {
            return Err("unexpected token in expression".to_string());
        }
        Ok(expr)
    }

    fn parse_binary(&mut self, min_precedence: u8) -> Result<Expr, String> {
        let mut left = self.parse_primary()?;

        while let Some((op, precedence)) = self.peek_binary_op() {
            if precedence < min_precedence {
                break;
            }
            self.cursor += 1;
            let right = self.parse_binary(precedence + 1)?;
            let span = left.span;
            left = Expr {
                kind: ExprKind::Binary {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                },
                span,
            };
        }

        Ok(left)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        let token = self.peek().ok_or("expected expression")?.clone();
        self.cursor += 1;
        match token.kind {
            TokenKind::Number(value) => {
                let parsed = value
                    .parse::<i64>()
                    .map_err(|_| format!("invalid integer literal `{value}`"))?;
                Ok(Expr {
                    kind: ExprKind::Integer(parsed),
                    span: token.span,
                })
            }
            TokenKind::String(value) => Ok(Expr {
                kind: ExprKind::String(value),
                span: token.span,
            }),
            TokenKind::Keyword(Keyword::True) => Ok(Expr {
                kind: ExprKind::Bool(true),
                span: token.span,
            }),
            TokenKind::Keyword(Keyword::False) => Ok(Expr {
                kind: ExprKind::Bool(false),
                span: token.span,
            }),
            TokenKind::Identifier(name) => {
                if self.eat_symbol("(") {
                    let mut args = Vec::new();
                    if !self.eat_symbol(")") {
                        loop {
                            args.push(self.parse_binary(0)?);
                            if self.eat_symbol(")") {
                                break;
                            }
                            if !self.eat_symbol(",") {
                                return Err("expected `,` or `)` in call expression".to_string());
                            }
                        }
                    }
                    Ok(Expr {
                        kind: ExprKind::Call { name, args },
                        span: token.span,
                    })
                } else {
                    Ok(Expr {
                        kind: ExprKind::Variable(name),
                        span: token.span,
                    })
                }
            }
            TokenKind::Symbol(symbol) if symbol == "(" => {
                let expr = self.parse_binary(0)?;
                if !self.eat_symbol(")") {
                    return Err("expected `)` after grouped expression".to_string());
                }
                Ok(expr)
            }
            TokenKind::Symbol(symbol) if symbol == "-" => {
                let value = self.parse_primary()?;
                Ok(Expr {
                    kind: ExprKind::Binary {
                        left: Box::new(Expr {
                            kind: ExprKind::Integer(0),
                            span: token.span,
                        }),
                        op: BinaryOp::Subtract,
                        right: Box::new(value),
                    },
                    span: token.span,
                })
            }
            _ => Err("expected expression".to_string()),
        }
    }

    fn peek_binary_op(&self) -> Option<(BinaryOp, u8)> {
        let TokenKind::Symbol(symbol) = &self.peek()?.kind else {
            return None;
        };
        Some(match symbol.as_str() {
            "==" => (BinaryOp::Equal, 1),
            "!=" => (BinaryOp::NotEqual, 1),
            "<" => (BinaryOp::Less, 1),
            "<=" => (BinaryOp::LessEqual, 1),
            ">" => (BinaryOp::Greater, 1),
            ">=" => (BinaryOp::GreaterEqual, 1),
            "+" => (BinaryOp::Add, 2),
            "-" => (BinaryOp::Subtract, 2),
            "*" => (BinaryOp::Multiply, 3),
            "/" => (BinaryOp::Divide, 3),
            _ => return None,
        })
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        if matches!(self.peek().map(|token| &token.kind), Some(TokenKind::Symbol(actual)) if actual == symbol)
        {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn is_at_end(&self) -> bool {
        self.cursor >= self.tokens.len()
    }

    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.cursor)
    }
}

#[derive(Debug, Clone, Copy)]
enum TokenKindRef {
    Arrow,
    Newline,
    Indent,
    Dedent,
    Eof,
}

#[derive(Debug, Clone, Copy)]
enum BlockEnd {
    Dedent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nous_lexer::lex;

    #[test]
    fn parses_function_with_expression_return() {
        let tokens = lex("fn add x i64 y i64 -> i64\n    x + y\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.functions.len(), 1);
        assert_eq!(program.functions[0].name, "add");
        assert_eq!(program.functions[0].params.len(), 2);
        assert_eq!(program.functions[0].return_type.name, "i64");
    }

    #[test]
    fn parses_void_function() {
        let tokens = lex("fn main -> void\n    return\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.functions[0].return_type.name, "void");
    }

    #[test]
    fn parses_let_and_call_expression() {
        let tokens =
            lex("fn main -> i64\n    let value i64 = add(1, 2)\n    value\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.functions[0].body.len(), 2);
    }

    #[test]
    fn parses_assignment_and_while_loop() {
        let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.functions[0].body.len(), 3);
        assert!(matches!(program.functions[0].body[1], Stmt::While { .. }));
    }

    #[test]
    fn parses_loop_break_and_continue() {
        let source = "fn main -> void\n    loop\n        continue\n        break\n    return\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert!(matches!(program.functions[0].body[0], Stmt::Loop { .. }));
    }

    #[test]
    fn requires_indented_function_body() {
        let tokens = lex("fn main -> void\nreturn\n").expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse should fail");
        assert_eq!(diagnostics[0].code, "N0205");
    }
}
