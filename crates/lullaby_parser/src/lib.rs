use lullaby_lexer::{Diagnostic, Keyword, Span, Token, TokenKind};
use serde::{Deserialize, Serialize};

mod format;
pub use format::format_program;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub functions: Vec<Function>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<AliasDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structs: Vec<StructDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<EnumDecl>,
}

/// A struct declaration: `struct NAME` followed by indented `field type` lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructDecl {
    pub name: String,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub ty: TypeRef,
}

/// An enum (tagged-union) declaration: `enum NAME` followed by indented
/// `Variant type...` lines. Each variant is a name plus zero or more positional,
/// unnamed payload types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumDecl {
    pub name: String,
    pub variants: Vec<EnumVariant>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumVariant {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<TypeRef>,
}

/// A type alias declaration: `alias NAME = TYPE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliasDecl {
    pub name: String,
    pub target: TypeRef,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: TypeRef,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    pub fn array_element(&self) -> Option<TypeRef> {
        self.generic_arg("array")
    }

    /// The inner type of a `<ctor><T>` spelling, e.g. `generic_arg("rc")` on
    /// `rc<i64>` yields `i64`.
    pub fn generic_arg(&self, ctor: &str) -> Option<TypeRef> {
        self.name
            .strip_prefix(&format!("{ctor}<"))
            .and_then(|name| name.strip_suffix('>'))
            .map(TypeRef::new)
    }

    /// The pointee of a raw pointer, accepting the modern `ptr<T>` spelling and
    /// the legacy `ptr_T` spelling produced by `alloc`.
    pub fn pointer_target(&self) -> Option<TypeRef> {
        self.generic_arg("ptr")
            .or_else(|| self.name.strip_prefix("ptr_").map(TypeRef::new))
    }

    /// The referent of a safe borrowed reference `ref<T>`.
    pub fn reference_target(&self) -> Option<TypeRef> {
        self.generic_arg("ref")
    }

    /// The owned type of a reference-counted handle `rc<T>`.
    pub fn rc_target(&self) -> Option<TypeRef> {
        self.generic_arg("rc")
    }

    /// True for a raw pointer (`ptr<T>` or legacy `ptr_T`).
    pub fn is_raw_pointer(&self) -> bool {
        self.pointer_target().is_some()
    }

    /// True for a safe reference type (`ref<T>` or `rc<T>`).
    pub fn is_safe_reference(&self) -> bool {
        self.reference_target().is_some() || self.rc_target().is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Stmt {
    Let {
        name: String,
        ty: Option<TypeRef>,
        value: Expr,
        span: Span,
    },
    Assign {
        name: String,
        /// Access path for struct field / array element mutation, e.g. `p.x = e`
        /// has `[Field("x")]`, `a[i] = e` has `[Index(i)]`, and `p.items[0].x = e`
        /// mixes both. Empty for a plain variable assignment. The root variable
        /// is always `name`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        path: Vec<Place>,
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
    For {
        name: String,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
        span: Span,
    },
    Loop {
        body: Vec<Stmt>,
        span: Span,
    },
    Unsafe {
        body: Vec<Stmt>,
        span: Span,
    },
    Region(RegionDecl),
    Throw {
        value: Expr,
        span: Span,
    },
    Try {
        body: Vec<Stmt>,
        catch_name: String,
        catch_body: Vec<Stmt>,
        span: Span,
    },
}

/// One step in an assignment target path: a struct field or an array index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Place {
    Field(String),
    Index(Expr),
}

/// Convert an lvalue expression (a variable plus `.field` / `[index]` accessors)
/// into a root variable name and place path. Returns `None` for anything that is
/// not a valid assignment target.
fn expr_to_place(expr: &Expr) -> Option<(String, Vec<Place>)> {
    match &expr.kind {
        ExprKind::Variable(name) => Some((name.clone(), Vec::new())),
        ExprKind::Field { target, field } => {
            let (name, mut path) = expr_to_place(target)?;
            path.push(Place::Field(field.clone()));
            Some((name, path))
        }
        ExprKind::Index { target, index } => {
            let (name, mut path) = expr_to_place(target)?;
            path.push(Place::Index((**index).clone()));
            Some((name, path))
        }
        _ => None,
    }
}

/// A memory-region declaration: `region NAME: size=N[, align=N][, kind=static|dynamic][, mutable=true|false]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionDecl {
    pub name: String,
    pub size: i64,
    pub align: Option<i64>,
    pub kind: String,
    pub mutable: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssignOp {
    Replace,
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IfBranch {
    pub condition: Expr,
    pub body: Vec<Stmt>,
}

/// One arm of a `match`: a pattern plus an inline-or-block body. An inline
/// `pattern -> expr` becomes a single-expression body, exactly like `if`/`try`
/// arms; a block body's last expression is the arm's value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub body: Vec<Stmt>,
}

/// A `match` arm pattern: a variant (with optional positional payload bindings)
/// or the `_` wildcard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchPattern {
    Variant {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bindings: Vec<String>,
    },
    Wildcard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

// `Eq` is intentionally omitted: `ExprKind::Float` holds an `f64`, which is not
// `Eq`. Types that transitively contain an expression therefore also derive
// `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExprKind {
    Integer(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Array(Vec<Expr>),
    Variable(String),
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
    /// Named-field struct construction, e.g. `Point(x: 3, y: 4)`. Fields are
    /// stored in source order; semantics and lowering reorder them to the
    /// struct's declared field order.
    StructLiteral {
        name: String,
        fields: Vec<(String, Expr)>,
    },
    Field {
        target: Box<Expr>,
        field: String,
    },
    /// Pattern matching over an enum: `match SCRUTINEE` followed by an indented
    /// arm list. Like `if`/`try`, a `match` is an expression: when every arm
    /// yields the same type it produces that value; otherwise it is a void
    /// statement.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOp {
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    And,
    Or,
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
        let mut aliases = Vec::new();
        let mut structs = Vec::new();
        let mut enums = Vec::new();
        self.skip_newlines();

        while !self.at(TokenKindRef::Eof) {
            if self.eat_keyword(Keyword::Fn).is_some() {
                if let Some(function) = self.parse_function() {
                    functions.push(function);
                }
            } else if self.eat_keyword(Keyword::Alias).is_some() {
                if let Some(alias) = self.parse_alias() {
                    aliases.push(alias);
                }
            } else if self.eat_keyword(Keyword::Struct).is_some() {
                if let Some(declaration) = self.parse_struct() {
                    structs.push(declaration);
                }
            } else if self.eat_keyword(Keyword::Enum).is_some() {
                if let Some(declaration) = self.parse_enum() {
                    enums.push(declaration);
                }
            } else if self.reject_planned_syntax().is_some() {
                self.skip_planned_syntax();
            } else {
                let token = self.peek();
                self.error(
                    "L0201",
                    "expected top-level function declaration",
                    token.span,
                );
                self.advance();
            }
            self.skip_newlines();
        }

        Program {
            functions,
            aliases,
            structs,
            enums,
        }
    }

    /// Parse `enum NAME` followed by an indented list of `Variant type...` lines.
    /// Each variant is a name plus zero or more positional, unnamed payload types.
    fn parse_enum(&mut self) -> Option<EnumDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected enum name")?;
        self.expect_newline("expected newline after enum name");
        self.expect(TokenKindRef::Indent, "expected indented enum variants")?;
        let mut variants = Vec::new();
        while !self.at(TokenKindRef::Dedent) && !self.at(TokenKindRef::Eof) {
            let variant_name = self.expect_identifier("expected variant name")?;
            let mut payload = Vec::new();
            while !self.at(TokenKindRef::Newline) && !self.at(TokenKindRef::Eof) {
                payload.push(self.expect_type("expected variant payload type")?);
            }
            self.expect_newline("expected newline after enum variant");
            variants.push(EnumVariant {
                name: variant_name,
                payload,
            });
        }
        self.expect(TokenKindRef::Dedent, "expected enum body dedent")?;
        Some(EnumDecl {
            name,
            variants,
            span,
        })
    }

    /// Parse `struct NAME` followed by an indented list of `field type` lines.
    fn parse_struct(&mut self) -> Option<StructDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected struct name")?;
        self.expect_newline("expected newline after struct name");
        self.expect(TokenKindRef::Indent, "expected indented struct fields")?;
        let mut fields = Vec::new();
        while !self.at(TokenKindRef::Dedent) && !self.at(TokenKindRef::Eof) {
            let field_name = self.expect_identifier("expected field name")?;
            let ty = self.expect_type("expected field type")?;
            self.expect_newline("expected newline after struct field");
            fields.push(StructField {
                name: field_name,
                ty,
            });
        }
        self.expect(TokenKindRef::Dedent, "expected struct body dedent")?;
        Some(StructDecl { name, fields, span })
    }

    /// Parse `alias NAME = TYPE`.
    fn parse_alias(&mut self) -> Option<AliasDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected alias name")?;
        if !self.eat_symbol("=") {
            self.error(
                "L0212",
                "expected `=` in alias declaration",
                self.peek().span,
            );
            return None;
        }
        let target = self.expect_type("expected alias target type")?;
        self.expect_newline("expected newline after alias declaration");
        Some(AliasDecl { name, target, span })
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
                "L0202",
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
        if self.reject_planned_syntax().is_some() {
            self.skip_planned_syntax();
            return None;
        }

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

        if self.eat_keyword(Keyword::For).is_some() {
            return self.parse_for();
        }

        if self.eat_keyword(Keyword::Loop).is_some() {
            return self.parse_loop();
        }

        if self.eat_keyword(Keyword::Unsafe).is_some() {
            return self.parse_unsafe();
        }

        if self.eat_keyword(Keyword::Region).is_some() {
            return self.parse_region();
        }

        if self.eat_keyword(Keyword::Throw).is_some() {
            let span = self.previous().span;
            let value = self.parse_expr_line(span)?;
            self.expect_newline("expected newline after throw statement");
            return Some(Stmt::Throw { value, span });
        }

        if self.eat_keyword(Keyword::Try).is_some() {
            return self.parse_try();
        }

        if self.eat_keyword(Keyword::Match).is_some() {
            let match_expr = self.parse_match()?;
            return Some(Stmt::Expr(match_expr));
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
        let ty = if self.at_symbol("=") {
            None
        } else {
            Some(self.expect_type("expected binding type or `=` for inferred binding")?)
        };
        if !self.eat_symbol("=") {
            self.error("L0206", "expected `=` in let binding", self.peek().span);
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
        // The target is everything before the assignment operator: a variable
        // plus `.field` / `[index]` accessors. Parse it as an expression, then
        // convert that expression into a place path.
        let op_pos = self.assignment_op_position()?;
        let mut target_parser = ExprParser::new(&self.tokens[self.cursor..op_pos]);
        let target = match target_parser.parse() {
            Ok(expr) => expr,
            Err(_) => {
                self.error("L0214", "invalid assignment target", span);
                return None;
            }
        };
        let Some((name, path)) = expr_to_place(&target) else {
            self.error("L0214", "invalid assignment target", span);
            return None;
        };
        self.cursor = op_pos;
        let op = self.expect_assignment_op()?;
        let value = self.parse_expr_line(span)?;
        self.expect_newline("expected newline after assignment");
        Some(Stmt::Assign {
            name,
            path,
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

    fn parse_for(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected loop variable after `for`")?;
        if self.eat_keyword(Keyword::From).is_none() {
            self.error("L0209", "expected `from` in for loop", self.peek().span);
            return None;
        }
        let start = self.parse_expr_until_keywords(span, &[Keyword::To])?;
        if self.eat_keyword(Keyword::To).is_none() {
            self.error("L0206", "expected `to` in for loop", self.peek().span);
            return None;
        }
        let end = self.parse_expr_until_keywords(span, &[Keyword::By])?;
        let step = if self.eat_keyword(Keyword::By).is_some() {
            Some(self.parse_expr_line(span)?)
        } else {
            None
        };
        self.expect_newline("expected newline after for loop header");
        self.expect(TokenKindRef::Indent, "expected indented for body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected for body dedent")?;
        Some(Stmt::For {
            name,
            start,
            end,
            step,
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

    fn parse_unsafe(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        self.expect_newline("expected newline after unsafe");
        self.expect(TokenKindRef::Indent, "expected indented unsafe body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected unsafe body dedent")?;
        Some(Stmt::Unsafe { body, span })
    }

    /// Parse a `try` / `catch NAME` block.
    fn parse_try(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        self.expect_newline("expected newline after try");
        self.expect(TokenKindRef::Indent, "expected indented try body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected try body dedent")?;

        if self.eat_keyword(Keyword::Catch).is_none() {
            self.error(
                "L0213",
                "expected `catch` after try block",
                self.peek().span,
            );
            return None;
        }
        let catch_name = self.expect_identifier("expected catch binding name")?;
        self.expect_newline("expected newline after catch binding");
        self.expect(TokenKindRef::Indent, "expected indented catch body")?;
        let catch_body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected catch body dedent")?;

        Some(Stmt::Try {
            body,
            catch_name,
            catch_body,
            span,
        })
    }

    /// Parse `match SCRUTINEE` followed by an indented arm list. The `match`
    /// keyword has already been consumed. Each arm is `pattern -> inline_expr`
    /// or `pattern ->` followed by an indented block, mirroring the arrow
    /// inline-or-block spelling used by function bodies and `if`/`try` arms.
    fn parse_match(&mut self) -> Option<Expr> {
        let span = self.previous().span;
        let scrutinee = self.parse_expr_line(span)?;
        self.expect_newline("expected newline after match scrutinee");
        self.expect(TokenKindRef::Indent, "expected indented match arms")?;
        let mut arms = Vec::new();
        while !self.at(TokenKindRef::Dedent) && !self.at(TokenKindRef::Eof) {
            let pattern = self.parse_match_pattern()?;
            if !self.eat(TokenKindRef::Arrow) {
                self.error(
                    "L0215",
                    "expected `->` after match pattern",
                    self.peek().span,
                );
                return None;
            }
            let body = self.parse_arm_body(span)?;
            arms.push(MatchArm { pattern, body });
            self.skip_newlines();
        }
        self.expect(TokenKindRef::Dedent, "expected match body dedent")?;
        Some(Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span,
        })
    }

    /// Parse a match arm pattern: `_` (wildcard), a bare `Variant`, or
    /// `Variant(bind1, bind2, ...)` with positional payload bindings.
    fn parse_match_pattern(&mut self) -> Option<MatchPattern> {
        // `_` lexes as an identifier (its leading `_` is an identifier char), so
        // the wildcard is recognized by matching that identifier text exactly.
        if matches!(&self.peek().kind, TokenKind::Identifier(name) if name == "_") {
            self.advance();
            return Some(MatchPattern::Wildcard);
        }
        let name = self.expect_identifier("expected match pattern variant name")?;
        let mut bindings = Vec::new();
        if self.eat_symbol("(") && !self.eat_symbol(")") {
            loop {
                bindings.push(self.expect_identifier("expected payload binding name")?);
                if self.eat_symbol(")") {
                    break;
                }
                if !self.eat_symbol(",") {
                    self.error(
                        "L0215",
                        "expected `,` or `)` in match pattern bindings",
                        self.peek().span,
                    );
                    return None;
                }
            }
        }
        Some(MatchPattern::Variant { name, bindings })
    }

    /// Parse the body of an arrow arm: either an inline expression on the same
    /// line, or a newline-introduced indented block whose last expression is the
    /// arm's value.
    fn parse_arm_body(&mut self, span: Span) -> Option<Vec<Stmt>> {
        if self.at(TokenKindRef::Newline) {
            self.expect_newline("expected newline after `->`");
            self.expect(TokenKindRef::Indent, "expected indented match arm body")?;
            let body = self.parse_block(&[BlockEnd::Dedent]);
            self.expect(TokenKindRef::Dedent, "expected match arm body dedent")?;
            Some(body)
        } else {
            let expr = self.parse_expr_line(span)?;
            self.expect_newline("expected newline after match arm expression");
            Some(vec![Stmt::Expr(expr)])
        }
    }

    /// Parse `region NAME: size=N[, align=N][, kind=static|dynamic][, mutable=true|false]`.
    fn parse_region(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected region name")?;
        if !self.eat_symbol(":") {
            self.error("L0210", "expected `:` after region name", self.peek().span);
            return None;
        }

        let mut size: Option<i64> = None;
        let mut align: Option<i64> = None;
        let mut kind = String::from("static");
        let mut mutable = false;

        loop {
            let field = self.expect_identifier("expected region field name")?;
            if !self.eat_symbol("=") {
                self.error("L0210", "expected `=` in region field", self.peek().span);
                return None;
            }
            match field.as_str() {
                "size" => size = Some(self.expect_number("expected region size")?),
                "align" => align = Some(self.expect_number("expected region alignment")?),
                "kind" => kind = self.expect_identifier("expected region kind")?,
                "mutable" => mutable = self.expect_bool_word("expected true or false")?,
                other => {
                    self.error("L0210", "unknown region field", self.previous().span);
                    let _ = other;
                    return None;
                }
            }
            if !self.eat_symbol(",") {
                break;
            }
        }

        self.expect_newline("expected newline after region declaration");
        let Some(size) = size else {
            self.error("L0210", "region declaration requires `size`", span);
            return None;
        };
        Some(Stmt::Region(RegionDecl {
            name,
            size,
            align,
            kind,
            mutable,
            span,
        }))
    }

    fn expect_number(&mut self, message: &'static str) -> Option<i64> {
        match &self.peek().kind {
            TokenKind::Number(value) => {
                let parsed = value.parse::<i64>().ok();
                self.advance();
                match parsed {
                    Some(number) => Some(number),
                    None => {
                        self.error("L0210", message, self.previous().span);
                        None
                    }
                }
            }
            _ => {
                self.error("L0210", message, self.peek().span);
                None
            }
        }
    }

    fn expect_bool_word(&mut self, message: &'static str) -> Option<bool> {
        match &self.peek().kind {
            TokenKind::Keyword(Keyword::True) => {
                self.advance();
                Some(true)
            }
            TokenKind::Keyword(Keyword::False) => {
                self.advance();
                Some(false)
            }
            _ => {
                self.error("L0210", message, self.peek().span);
                None
            }
        }
    }

    fn parse_expr_line(&mut self, fallback_span: Span) -> Option<Expr> {
        self.parse_expr_until(fallback_span, &[])
    }

    fn parse_expr_until_keywords(
        &mut self,
        fallback_span: Span,
        keywords: &[Keyword],
    ) -> Option<Expr> {
        self.parse_expr_until(fallback_span, keywords)
    }

    fn parse_expr_until(&mut self, fallback_span: Span, keywords: &[Keyword]) -> Option<Expr> {
        let start = self.cursor;
        while !self.at(TokenKindRef::Newline)
            && !self.at(TokenKindRef::Indent)
            && !self.at(TokenKindRef::Dedent)
            && !self.at(TokenKindRef::Eof)
            && !self.at_any_keyword(keywords)
        {
            self.advance();
        }

        let mut expr_parser = ExprParser::new(&self.tokens[start..self.cursor]);
        match expr_parser.parse() {
            Ok(expr) => Some(expr),
            Err(message) => {
                self.error("L0207", message, fallback_span);
                None
            }
        }
    }

    fn expect_type(&mut self, message: &'static str) -> Option<TypeRef> {
        match &self.peek().kind {
            TokenKind::Identifier(name) => {
                let name = name.clone();
                self.advance();
                if matches!(name.as_str(), "array" | "ptr" | "ref" | "rc") && self.eat_symbol("<") {
                    let inner = self.expect_type("expected generic type argument")?;
                    if !self.eat_symbol(">") {
                        self.error(
                            "L0203",
                            "expected `>` after generic type argument",
                            self.peek().span,
                        );
                        return None;
                    }
                    Some(TypeRef::new(format!("{name}<{}>", inner.name)))
                } else {
                    Some(TypeRef::new(name))
                }
            }
            TokenKind::Keyword(Keyword::Void) => {
                self.advance();
                Some(TypeRef::new("void"))
            }
            _ => {
                self.error("L0203", message, self.peek().span);
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
                self.error("L0204", message, self.peek().span);
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
            self.error("L0205", message, self.peek().span);
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

    fn at_symbol(&self, symbol: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Symbol(actual) if actual == symbol)
    }

    fn next_is_assignment(&self) -> bool {
        self.assignment_op_position().is_some()
    }

    /// If the statement starting at the cursor is an assignment, return the token
    /// index of its assignment operator. The target is an identifier optionally
    /// followed by `.field` and `[index]` accessors (balanced brackets).
    fn assignment_op_position(&self) -> Option<usize> {
        if !matches!(self.peek().kind, TokenKind::Identifier(_)) {
            return None;
        }
        let mut index = self.cursor + 1;
        loop {
            match self.tokens.get(index).map(|token| &token.kind) {
                Some(TokenKind::Symbol(symbol)) if symbol == "." => {
                    if !matches!(
                        self.tokens.get(index + 1).map(|token| &token.kind),
                        Some(TokenKind::Identifier(_))
                    ) {
                        return None;
                    }
                    index += 2;
                }
                Some(TokenKind::Symbol(symbol)) if symbol == "[" => {
                    let mut depth = 1;
                    index += 1;
                    while depth > 0 {
                        match self.tokens.get(index).map(|token| &token.kind) {
                            Some(TokenKind::Symbol(symbol)) if symbol == "[" => depth += 1,
                            Some(TokenKind::Symbol(symbol)) if symbol == "]" => depth -= 1,
                            Some(TokenKind::Eof) | None => return None,
                            _ => {}
                        }
                        index += 1;
                    }
                }
                _ => break,
            }
        }
        match self.tokens.get(index).map(|token| &token.kind) {
            Some(TokenKind::Symbol(symbol))
                if matches!(symbol.as_str(), "=" | "+=" | "-=" | "*=" | "/=") =>
            {
                Some(index)
            }
            _ => None,
        }
    }

    fn expect_assignment_op(&mut self) -> Option<AssignOp> {
        let TokenKind::Symbol(symbol) = &self.peek().kind else {
            self.error("L0208", "expected assignment operator", self.peek().span);
            return None;
        };
        let op = match symbol.as_str() {
            "=" => AssignOp::Replace,
            "+=" => AssignOp::Add,
            "-=" => AssignOp::Subtract,
            "*=" => AssignOp::Multiply,
            "/=" => AssignOp::Divide,
            _ => {
                self.error("L0208", "expected assignment operator", self.peek().span);
                return None;
            }
        };
        self.advance();
        Some(op)
    }

    fn reject_planned_syntax(&mut self) -> Option<Token> {
        let feature = planned_syntax_name(&self.peek().kind)?;
        let token = self.peek().clone();
        self.error(
            "L0211",
            format!(
                "`{feature}` syntax is planned beyond Alpha 1 and is not supported by this compiler"
            ),
            token.span,
        );
        Some(token)
    }

    fn skip_planned_syntax(&mut self) {
        while !self.at(TokenKindRef::Newline)
            && !self.at(TokenKindRef::Dedent)
            && !self.at(TokenKindRef::Eof)
        {
            self.advance();
        }
        if self.at(TokenKindRef::Newline) {
            self.advance();
        }

        if self.at(TokenKindRef::Indent) {
            let mut depth = 0usize;
            while !self.at(TokenKindRef::Eof) {
                if self.at(TokenKindRef::Indent) {
                    depth += 1;
                    self.advance();
                    continue;
                }
                if self.at(TokenKindRef::Dedent) {
                    self.advance();
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                self.advance();
            }
        }
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> Option<Token> {
        if matches!(&self.peek().kind, TokenKind::Keyword(actual) if *actual == keyword) {
            self.advance();
            Some(self.previous().clone())
        } else {
            None
        }
    }

    fn at_any_keyword(&self, keywords: &[Keyword]) -> bool {
        keywords.iter().any(
            |keyword| matches!(&self.peek().kind, TokenKind::Keyword(actual) if actual == keyword),
        )
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

fn planned_syntax_name(kind: &TokenKind) -> Option<&'static str> {
    let TokenKind::Keyword(keyword) = kind else {
        return None;
    };
    Some(match keyword {
        Keyword::Module => "module",
        Keyword::Import => "import",
        Keyword::Package => "package",
        Keyword::Union => "union",
        Keyword::Trait => "trait",
        Keyword::Interface => "interface",
        Keyword::Class => "class",
        Keyword::Switch => "switch",
        Keyword::Catch => "catch",
        Keyword::Async => "async",
        Keyword::Await => "await",
        Keyword::Coroutine => "coroutine",
        _ => return None,
    })
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
        let mut left = self.parse_unary()?;

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

    fn parse_unary(&mut self) -> Result<Expr, String> {
        let token = self.peek().ok_or("expected expression")?.clone();
        match token.kind {
            TokenKind::Keyword(Keyword::Not) => {
                self.cursor += 1;
                let expr = self.parse_unary()?;
                Ok(Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(expr),
                    },
                    span: token.span,
                })
            }
            TokenKind::Symbol(symbol) if symbol == "-" => {
                self.cursor += 1;
                let value = self.parse_unary()?;
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
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat_symbol("[") {
                let index = self.parse_binary(0)?;
                if !self.eat_symbol("]") {
                    return Err("expected `]` after index expression".to_string());
                }
                let span = expr.span;
                expr = Expr {
                    kind: ExprKind::Index {
                        target: Box::new(expr),
                        index: Box::new(index),
                    },
                    span,
                };
            } else if self.eat_symbol(".") {
                let span = expr.span;
                let name = match self.peek().map(|token| token.kind.clone()) {
                    Some(TokenKind::Identifier(name)) => {
                        self.cursor += 1;
                        name
                    }
                    _ => return Err("expected field name after `.`".to_string()),
                };
                if self.eat_symbol("(") {
                    // Method-call sugar: `recv.name(args)` desugars to
                    // `name(recv, args)` (UFCS), reusing normal call checking.
                    let mut args = vec![expr];
                    if !self.eat_symbol(")") {
                        loop {
                            args.push(self.parse_binary(0)?);
                            if self.eat_symbol(")") {
                                break;
                            }
                            if !self.eat_symbol(",") {
                                return Err("expected `,` or `)` in method call".to_string());
                            }
                        }
                    }
                    expr = Expr {
                        kind: ExprKind::Call { name, args },
                        span,
                    };
                } else {
                    expr = Expr {
                        kind: ExprKind::Field {
                            target: Box::new(expr),
                            field: name,
                        },
                        span,
                    };
                }
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        let token = self.peek().ok_or("expected expression")?.clone();
        self.cursor += 1;
        match token.kind {
            TokenKind::Number(value) => {
                // A `.` in the literal marks a floating-point (`f64`) literal.
                let kind = if value.contains('.') {
                    let parsed = value
                        .parse::<f64>()
                        .map_err(|_| format!("invalid float literal `{value}`"))?;
                    ExprKind::Float(parsed)
                } else {
                    let parsed = value
                        .parse::<i64>()
                        .map_err(|_| format!("invalid integer literal `{value}`"))?;
                    ExprKind::Integer(parsed)
                };
                Ok(Expr {
                    kind,
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
                    // Named-field construction `Name(field: expr, ...)` is
                    // detected by an `identifier :` prefix on the first argument.
                    if self.peek_is_named_field() {
                        let mut fields = Vec::new();
                        loop {
                            let field = self.expect_identifier_for_field()?;
                            if !self.eat_symbol(":") {
                                return Err("expected `:` after field name in named construction"
                                    .to_string());
                            }
                            fields.push((field, self.parse_binary(0)?));
                            if self.eat_symbol(")") {
                                break;
                            }
                            if !self.eat_symbol(",") {
                                return Err("expected `,` or `)` in named construction".to_string());
                            }
                        }
                        return Ok(Expr {
                            kind: ExprKind::StructLiteral { name, fields },
                            span: token.span,
                        });
                    }
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
            TokenKind::Symbol(symbol) if symbol == "[" => {
                let mut values = Vec::new();
                if !self.eat_symbol("]") {
                    loop {
                        values.push(self.parse_binary(0)?);
                        if self.eat_symbol("]") {
                            break;
                        }
                        if !self.eat_symbol(",") {
                            return Err("expected `,` or `]` in array literal".to_string());
                        }
                    }
                }
                Ok(Expr {
                    kind: ExprKind::Array(values),
                    span: token.span,
                })
            }
            TokenKind::Symbol(symbol) if symbol == "(" => {
                let expr = self.parse_binary(0)?;
                if !self.eat_symbol(")") {
                    return Err("expected `)` after grouped expression".to_string());
                }
                Ok(expr)
            }
            _ => Err("expected expression".to_string()),
        }
    }

    fn peek_binary_op(&self) -> Option<(BinaryOp, u8)> {
        match &self.peek()?.kind {
            TokenKind::Keyword(Keyword::Or) => Some((BinaryOp::Or, 1)),
            TokenKind::Keyword(Keyword::And) => Some((BinaryOp::And, 2)),
            TokenKind::Symbol(symbol) => Some(match symbol.as_str() {
                "==" => (BinaryOp::Equal, 3),
                "!=" => (BinaryOp::NotEqual, 3),
                "<" => (BinaryOp::Less, 3),
                "<=" => (BinaryOp::LessEqual, 3),
                ">" => (BinaryOp::Greater, 3),
                ">=" => (BinaryOp::GreaterEqual, 3),
                "+" => (BinaryOp::Add, 4),
                "-" => (BinaryOp::Subtract, 4),
                "*" => (BinaryOp::Multiply, 5),
                "/" => (BinaryOp::Divide, 5),
                _ => return None,
            }),
            _ => None,
        }
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

    /// True when the cursor is at an `identifier :` pair, marking a named-field
    /// construction argument rather than a positional expression.
    fn peek_is_named_field(&self) -> bool {
        matches!(
            self.tokens.get(self.cursor).map(|token| &token.kind),
            Some(TokenKind::Identifier(_))
        ) && matches!(
            self.tokens.get(self.cursor + 1).map(|token| &token.kind),
            Some(TokenKind::Symbol(symbol)) if symbol == ":"
        )
    }

    fn expect_identifier_for_field(&mut self) -> Result<String, String> {
        match self.peek().map(|token| &token.kind) {
            Some(TokenKind::Identifier(name)) => {
                let name = name.clone();
                self.cursor += 1;
                Ok(name)
            }
            _ => Err("expected field name in named construction".to_string()),
        }
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
    use lullaby_lexer::lex;

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
    fn parses_inferred_let_binding() {
        let tokens = lex("fn main -> i64\n    let value = add(1, 2)\n    value\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Let { ty, .. } = &program.functions[0].body[0] else {
            panic!("expected let statement");
        };
        assert_eq!(ty, &None);
    }

    #[test]
    fn parses_flat_io_and_system_builtin_calls() {
        let source = "fn main -> i64\n    write_file(\"target/parser.txt\", \"alpha\")\n    let text string = read_file(\"target/parser.txt\")\n    sys_status(\"rustc\", [\"--version\"])\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.functions[0].body.len(), 3);
        assert!(matches!(program.functions[0].body[0], Stmt::Expr(_)));
    }

    #[test]
    fn parses_try_catch_and_throw() {
        let source =
            "fn main -> void\n    try\n        throw \"boom\"\n    catch e\n        warn(e)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Try {
            catch_name, body, ..
        } = &program.functions[0].body[0]
        else {
            panic!("expected try/catch");
        };
        assert_eq!(catch_name, "e");
        assert!(matches!(body[0], Stmt::Throw { .. }));
    }

    #[test]
    fn parses_match_expression_with_bindings_and_wildcard() {
        let source = concat!(
            "enum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\n",
            "fn area s Shape -> f64\n",
            "    match s\n",
            "        Circle(r) -> 3.14 * r * r\n",
            "        Rect(w, h) -> w * h\n",
            "        _ -> 0.0\n",
        );
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(Expr {
            kind: ExprKind::Match { arms, .. },
            ..
        }) = &program.functions[0].body[0]
        else {
            panic!("expected match expression statement");
        };
        assert_eq!(arms.len(), 3);
        assert_eq!(
            arms[0].pattern,
            MatchPattern::Variant {
                name: "Circle".to_string(),
                bindings: vec!["r".to_string()],
            }
        );
        assert_eq!(
            arms[1].pattern,
            MatchPattern::Variant {
                name: "Rect".to_string(),
                bindings: vec!["w".to_string(), "h".to_string()],
            }
        );
        assert_eq!(arms[2].pattern, MatchPattern::Wildcard);
    }

    #[test]
    fn parses_match_arm_with_indented_block_body() {
        let source = concat!(
            "enum Opt\n    Some i64\n    None\n\n",
            "fn unwrap o Opt -> i64\n",
            "    match o\n",
            "        Some(v) ->\n",
            "            let doubled i64 = v * 2\n",
            "            doubled\n",
            "        None -> 0\n",
        );
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(Expr {
            kind: ExprKind::Match { arms, .. },
            ..
        }) = &program.functions[0].body[0]
        else {
            panic!("expected match expression statement");
        };
        assert_eq!(arms[0].body.len(), 2);
        assert!(matches!(arms[0].body[0], Stmt::Let { .. }));
    }

    #[test]
    fn parses_float_literal() {
        let source = "fn main -> f64\n    2.5\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        assert!(matches!(expr.kind, ExprKind::Float(value) if (value - 2.5).abs() < 1e-9));
    }

    #[test]
    fn parses_type_alias_declaration() {
        let source = "alias Count = i64\nalias Numbers = array<i64>\n\nfn main -> Count\n    0\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.aliases.len(), 2);
        assert_eq!(program.aliases[0].name, "Count");
        assert_eq!(program.aliases[0].target.name, "i64");
        assert_eq!(program.aliases[1].target.name, "array<i64>");
    }

    #[test]
    fn parses_region_declaration() {
        let source = "fn main -> i64\n    region pool: size=4096, align=16, kind=static, mutable=true\n    0\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Region(decl) = &program.functions[0].body[0] else {
            panic!("expected region declaration");
        };
        assert_eq!(decl.name, "pool");
        assert_eq!(decl.size, 4096);
        assert_eq!(decl.align, Some(16));
        assert_eq!(decl.kind, "static");
        assert!(decl.mutable);
    }

    #[test]
    fn parses_unsafe_block_and_reference_types() {
        let source = "fn main -> i64\n    let h rc<i64> = rc_new(1)\n    let p ptr_i64 = alloc(2)\n    unsafe\n        let v i64 = ptr_read(p)\n    rc_get(h)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert!(matches!(program.functions[0].body[2], Stmt::Unsafe { .. }));
        // `rc<i64>` parses as a generic reference type.
        let Stmt::Let { ty: Some(ty), .. } = &program.functions[0].body[0] else {
            panic!("expected typed let");
        };
        assert_eq!(ty.rc_target().expect("rc target").name, "i64");
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
    fn parses_for_loop() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert!(matches!(program.functions[0].body[1], Stmt::For { .. }));
    }

    #[test]
    fn parses_for_loop_with_step() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 5 by 2\n        total += i\n    total\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert!(matches!(
            program.functions[0].body[1],
            Stmt::For { step: Some(_), .. }
        ));
    }

    #[test]
    fn parses_logical_expressions() {
        let source = "fn main -> bool\n    not false and true or false\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert!(matches!(program.functions[0].body[0], Stmt::Expr(_)));
    }

    #[test]
    fn parses_array_literal_and_index() {
        let source = "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.functions[0].body.len(), 2);
    }

    #[test]
    fn requires_indented_function_body() {
        let tokens = lex("fn main -> void\nreturn\n").expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse should fail");
        assert_eq!(diagnostics[0].code, "L0205");
    }

    #[test]
    fn rejects_planned_top_level_syntax() {
        let tokens = lex("import math\nfn main -> i64\n    1\n").expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse should fail");
        assert_eq!(diagnostics[0].code, "L0211");
        assert!(diagnostics[0].message.contains("import"));
    }

    #[test]
    fn rejects_struct_declaration_inside_function_body() {
        // Struct declarations are top-level only.
        let tokens = lex("fn main -> i64\n    struct Point\n        x i64\n    1\n").expect("lex");
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn parses_struct_field_assignment() {
        let source = "struct Point\n    x i64\n\nfn main -> i64\n    let p Point = Point(1)\n    p.x = 5\n    p.x\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Assign { name, path, .. } = &program.functions[0].body[1] else {
            panic!("expected field assignment");
        };
        assert_eq!(name, "p");
        assert_eq!(path, &vec![Place::Field("x".to_string())]);
    }

    #[test]
    fn parses_array_element_assignment() {
        let source = "fn main -> i64\n    let a array<i64> = [1, 2, 3]\n    a[1] = 9\n    a[1]\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Assign { name, path, .. } = &program.functions[0].body[1] else {
            panic!("expected array element assignment");
        };
        assert_eq!(name, "a");
        assert!(matches!(path.as_slice(), [Place::Index(_)]));
    }

    #[test]
    fn desugars_method_call_to_ufcs_call() {
        // `recv.name(args)` parses to `name(recv, args)`; plain `recv.name`
        // stays field access.
        let source = "fn main -> i64\n    p.scaled(2)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(Expr {
            kind: ExprKind::Call { name, args },
            ..
        }) = &program.functions[0].body[0]
        else {
            panic!("expected method call desugared to a call");
        };
        assert_eq!(name, "scaled");
        assert_eq!(args.len(), 2);
        assert!(matches!(&args[0].kind, ExprKind::Variable(v) if v == "p"));
        assert!(matches!(&args[1].kind, ExprKind::Integer(2)));
    }

    #[test]
    fn parses_enum_declaration_with_unit_and_payload_variants() {
        let source =
            "enum Shape\n    Circle f64\n    Rect f64 f64\n    Empty\n\nfn main -> i64\n    0\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.enums.len(), 1);
        assert_eq!(program.enums[0].name, "Shape");
        assert_eq!(program.enums[0].variants.len(), 3);
        assert_eq!(program.enums[0].variants[0].name, "Circle");
        assert_eq!(program.enums[0].variants[0].payload.len(), 1);
        assert_eq!(program.enums[0].variants[1].payload.len(), 2);
        assert!(program.enums[0].variants[2].payload.is_empty());
    }

    #[test]
    fn parses_struct_declaration_and_field_access() {
        let source = "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    p.x\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.structs.len(), 1);
        assert_eq!(program.structs[0].name, "Point");
        assert_eq!(program.structs[0].fields.len(), 2);
        assert_eq!(program.structs[0].fields[0].name, "x");
    }
}
