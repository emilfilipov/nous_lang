use lullaby_lexer::{Keyword, Span, Token, TokenKind, lex};

use crate::number_literal::parse_number_literal;
use crate::{
    BinaryOp, Expr, ExprKind, Param, SupervisionPolicy, TypeRef, UnaryOp, function_type,
    generic_type,
};

pub(crate) struct ExprParser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    /// Next closure `id` to assign, seeded from the owning [`crate::Parser`] so ids are
    /// unique and monotonic across every expression in the program.
    pub(crate) closure_counter: usize,
    /// `>` closers still owed from a split `>>` token, mirroring the field on the
    /// declaration [`crate::Parser`] so nested generics in closure parameter types
    /// (`fn xs list<array<i64>> -> ...`) close correctly.
    pending_generic_close: usize,
}

impl<'a> ExprParser<'a> {
    pub(crate) fn new(tokens: &'a [Token], closure_counter: usize) -> Self {
        Self {
            tokens,
            cursor: 0,
            closure_counter,
            pending_generic_close: 0,
        }
    }

    pub(crate) fn parse(&mut self) -> Result<Expr, String> {
        let expr = self.parse_conditional()?;
        if !self.is_at_end() {
            return Err("unexpected token in expression".to_string());
        }
        Ok(expr)
    }

    /// The lowest-precedence expression layer: a binary/unary expression,
    /// optionally followed by an inline conditional `if COND else ELSE`. Every
    /// "fresh expression" position (call/method arguments, array elements,
    /// named-field values, parentheses, index brackets, closure bodies, and the
    /// top-level entry) parses through here, so a ternary is accepted wherever a
    /// full expression is. Right-associative: the `else` branch recurses.
    fn parse_conditional(&mut self) -> Result<Expr, String> {
        let then_branch = self.parse_binary(0)?;
        if !self.peek_keyword(Keyword::If) {
            return Ok(then_branch);
        }
        self.cursor += 1;
        let cond = self.parse_binary(0)?;
        if !self.eat_keyword_tok(Keyword::Else) {
            return Err(
                "expected `else` in inline conditional `THEN if COND else ELSE`".to_string(),
            );
        }
        let else_branch = self.parse_conditional()?;
        let span = then_branch.span;
        Ok(Expr {
            kind: ExprKind::Conditional {
                cond: Box::new(cond),
                then_branch: Box::new(then_branch),
                else_branch: Box::new(else_branch),
            },
            span,
        })
    }

    fn peek_keyword(&self, keyword: Keyword) -> bool {
        matches!(self.peek(), Some(token) if matches!(&token.kind, TokenKind::Keyword(k) if *k == keyword))
    }

    fn eat_keyword_tok(&mut self, keyword: Keyword) -> bool {
        if self.peek_keyword(keyword) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn parse_binary(&mut self, min_precedence: u8) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;

        loop {
            // `in` is the membership operator at comparison precedence (3). It is
            // non-associative in practice (the result is `bool`), so its
            // right-hand collection is parsed above that precedence.
            if self.peek_keyword(Keyword::In) {
                const IN_PRECEDENCE: u8 = 3;
                if IN_PRECEDENCE < min_precedence {
                    break;
                }
                self.cursor += 1;
                let collection = self.parse_binary(IN_PRECEDENCE + 1)?;
                let span = left.span;
                left = Expr {
                    kind: ExprKind::In {
                        value: Box::new(left),
                        collection: Box::new(collection),
                    },
                    span,
                };
                continue;
            }

            let Some((op, precedence)) = self.peek_binary_op() else {
                break;
            };
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
            // `await EXPR` is a prefix operator: it binds tighter than binary
            // operators (like `not`), so `await f(x) + await g(y)` awaits each
            // call, then adds. The operand is parsed as a unary expression.
            TokenKind::Keyword(Keyword::Await) => {
                self.cursor += 1;
                let expr = self.parse_unary()?;
                Ok(Expr {
                    kind: ExprKind::Await {
                        expr: Box::new(expr),
                    },
                    span: token.span,
                })
            }
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
            // Unary bitwise NOT (`~`) binds like `not`: as a prefix operator over
            // a unary expression, so `~a & b` parses as `(~a) & b`.
            TokenKind::Symbol(ref symbol) if symbol == "~" => {
                self.cursor += 1;
                let expr = self.parse_unary()?;
                Ok(Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::BitNot,
                        expr: Box::new(expr),
                    },
                    span: token.span,
                })
            }
            TokenKind::Symbol(symbol) if symbol == "-" => {
                self.cursor += 1;
                let value = self.parse_unary()?;
                Ok(Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::Negate,
                        expr: Box::new(value),
                    },
                    span: token.span,
                })
            }
            _ => self.parse_postfix(),
        }
    }

    /// Build a string expression, desugaring interpolation `"a=${expr}b"` into a
    /// `+`-concatenation of string literals and `to_string(expr)` calls (so no
    /// backend, formatter, or walker needs an interpolation node). A `${` with no
    /// closing `}` is an error; a plain string with no `${` stays a literal.
    fn build_string_expr(&mut self, value: String, span: Span) -> Result<Expr, String> {
        if !value.contains("${") {
            return Ok(Expr {
                kind: ExprKind::String(value),
                span,
            });
        }
        let str_lit = |text: &str| Expr {
            kind: ExprKind::String(text.to_string()),
            span,
        };
        let mut parts: Vec<Expr> = Vec::new();
        let mut rest = value.as_str();
        loop {
            match rest.find("${") {
                None => {
                    if !rest.is_empty() {
                        parts.push(str_lit(rest));
                    }
                    break;
                }
                Some(pos) => {
                    if pos > 0 {
                        parts.push(str_lit(&rest[..pos]));
                    }
                    let after = &rest[pos + 2..];
                    let close = after
                        .find('}')
                        .ok_or_else(|| "unterminated `${` in string interpolation".to_string())?;
                    let inner = after[..close].trim();
                    if inner.is_empty() {
                        return Err("empty `${}` in string interpolation".to_string());
                    }
                    let expr = self.parse_interpolated_expr(inner)?;
                    // Wrap in `to_string(...)`; it accepts every scalar and is the
                    // identity on a `string`, so any interpolated value renders.
                    parts.push(Expr {
                        kind: ExprKind::Call {
                            name: "to_string".to_string(),
                            args: vec![expr],
                        },
                        span,
                    });
                    rest = &after[close + 1..];
                }
            }
        }
        // `parts` is non-empty (the value contained `${`). Fold left with `+`.
        let mut iter = parts.into_iter();
        let mut acc = iter.next().expect("interpolation yields at least one part");
        for part in iter {
            acc = Expr {
                kind: ExprKind::Binary {
                    left: Box::new(acc),
                    op: BinaryOp::Add,
                    right: Box::new(part),
                },
                span,
            };
        }
        Ok(acc)
    }

    /// Lex and parse a single expression from the inside of a `${...}` segment.
    /// The inner text is one expression (Lullaby strings hold no nested strings),
    /// so structural indentation tokens are dropped before expression parsing.
    fn parse_interpolated_expr(&mut self, inner: &str) -> Result<Expr, String> {
        let tokens =
            lex(inner).map_err(|_| "invalid expression in string interpolation".to_string())?;
        let expr_tokens: Vec<Token> = tokens
            .into_iter()
            .filter(|token| {
                !matches!(
                    token.kind,
                    TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::Eof
                )
            })
            .collect();
        let mut sub = ExprParser::new(&expr_tokens, self.closure_counter);
        let expr = sub
            .parse()
            .map_err(|message| format!("in string interpolation: {message}"))?;
        self.closure_counter = sub.closure_counter;
        Ok(expr)
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat_symbol("[") {
                // `target[i]` is an index; `target[a:b]` (with either bound
                // optional) is a string slice. A leading `:` means the start is
                // omitted.
                let start = if self.at_symbol(":") {
                    None
                } else {
                    Some(self.parse_conditional()?)
                };
                let span = expr.span;
                if self.eat_symbol(":") {
                    let end = if self.at_symbol("]") {
                        None
                    } else {
                        Some(self.parse_conditional()?)
                    };
                    if !self.eat_symbol("]") {
                        return Err("expected `]` after slice bounds".to_string());
                    }
                    expr = Expr {
                        kind: ExprKind::Slice {
                            target: Box::new(expr),
                            start: start.map(Box::new),
                            end: end.map(Box::new),
                        },
                        span,
                    };
                } else {
                    let index = start.ok_or_else(|| "expected an index expression".to_string())?;
                    if !self.eat_symbol("]") {
                        return Err("expected `]` after index expression".to_string());
                    }
                    expr = Expr {
                        kind: ExprKind::Index {
                            target: Box::new(expr),
                            index: Box::new(index),
                        },
                        span,
                    };
                }
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
                            args.push(self.parse_conditional()?);
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
            } else if self.eat_symbol("?") {
                // Postfix error-propagation `expr?`. It binds tighter than every
                // binary operator (it lives in the postfix loop, above
                // `parse_binary`), so `a + b?` parses as `a + (b?)` and `f()?`
                // applies `?` to the call. Chaining `x??` reuses the same loop.
                let span = expr.span;
                expr = Expr {
                    kind: ExprKind::Try(Box::new(expr)),
                    span,
                };
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
                // A recognized type suffix (`1i32`, `2.0f32`, `0xFFu16`, …) is
                // range-checked and desugared to the matching `to_<T>` conversion;
                // `i64`/`f64` and unsuffixed literals stay plain `Integer`/`Float`.
                let kind = parse_number_literal(&value, token.span)?;
                Ok(Expr {
                    kind,
                    span: token.span,
                })
            }
            TokenKind::String(value) => self.build_string_expr(value, token.span),
            TokenKind::Char(value) => Ok(Expr {
                kind: ExprKind::Char(value),
                span: token.span,
            }),
            // Inline closure literal `fn PARAMS -> EXPR`. `PARAMS` are zero or
            // more `name type` pairs (the top-level `fn` parameter shape); the
            // body is a single expression parsed after `->`. Only valid in
            // expression position — a top-level `fn` declaration is parsed by the
            // declaration parser, never here.
            TokenKind::Keyword(Keyword::Fn) => {
                let id = self.closure_counter;
                self.closure_counter += 1;
                let mut params = Vec::new();
                while !self.at_arrow() {
                    let name = self.expect_closure_identifier()?;
                    let ty = self.parse_type()?;
                    params.push(Param { name, ty });
                }
                if !self.eat_arrow() {
                    return Err("expected `->` in closure literal".to_string());
                }
                let body = self.parse_conditional()?;
                Ok(Expr {
                    kind: ExprKind::Closure {
                        id,
                        params,
                        body: Box::new(body),
                    },
                    span: token.span,
                })
            }
            TokenKind::Keyword(Keyword::True) => Ok(Expr {
                kind: ExprKind::Bool(true),
                span: token.span,
            }),
            TokenKind::Keyword(Keyword::False) => Ok(Expr {
                kind: ExprKind::Bool(false),
                span: token.span,
            }),
            // `spawn NAME(args)` is the actor-spawn form: the `spawn` identifier
            // followed by an actor type name (an identifier, not `(`). The bare
            // `spawn(...)` builtin call — `spawn` immediately followed by `(` —
            // still parses through the ordinary identifier-call path below, so the
            // delivered thread `spawn` builtin keeps working.
            TokenKind::Identifier(ref name)
                if name == "spawn"
                    && matches!(
                        self.peek().map(|token| &token.kind),
                        Some(TokenKind::Identifier(_))
                    ) =>
            {
                let actor = match self.peek().map(|token| token.kind.clone()) {
                    Some(TokenKind::Identifier(actor)) => {
                        self.cursor += 1;
                        actor
                    }
                    // The guard already proved the next token is an identifier.
                    _ => unreachable!("spawn actor name is an identifier"),
                };
                if !self.eat_symbol("(") {
                    return Err("expected `(` after actor name in `spawn`".to_string());
                }
                let mut args = Vec::new();
                if !self.eat_symbol(")") {
                    loop {
                        args.push(self.parse_conditional()?);
                        if self.eat_symbol(")") {
                            break;
                        }
                        if !self.eat_symbol(",") {
                            return Err("expected `,` or `)` in spawn arguments".to_string());
                        }
                    }
                }
                let supervise = self.parse_supervise_clause()?;
                Ok(Expr {
                    kind: ExprKind::Spawn {
                        actor,
                        args,
                        supervise,
                    },
                    span: token.span,
                })
            }
            // `tell`/`ask TARGET.HANDLER(args)`: a message send. The operand
            // parses as an ordinary postfix expression, which turns the
            // method-call syntax `c.increment(5)` into a `Call{increment,[c,5]}`
            // (UFCS). We split that back into the target (first argument) and the
            // handler name plus its remaining arguments. `tell` is fire-and-forget
            // (`is_ask = false`); `ask` is request-reply (`is_ask = true`, yields
            // `Future<R>`). Both share the `Tell` node.
            TokenKind::Keyword(keyword @ (Keyword::Tell | Keyword::Ask)) => {
                let is_ask = keyword == Keyword::Ask;
                let verb = if is_ask { "ask" } else { "tell" };
                let operand = self.parse_postfix()?;
                match operand.kind {
                    ExprKind::Call { name, mut args } if !args.is_empty() => {
                        let target = args.remove(0);
                        Ok(Expr {
                            kind: ExprKind::Tell {
                                target: Box::new(target),
                                handler: name,
                                args,
                                is_ask,
                            },
                            span: token.span,
                        })
                    }
                    _ => Err(format!("`{verb}` expects `target.handler(args)`")),
                }
            }
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
                            fields.push((field, self.parse_conditional()?));
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
                            args.push(self.parse_conditional()?);
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
                        values.push(self.parse_conditional()?);
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
                let expr = self.parse_conditional()?;
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
                // Bitwise-logical operators bind tighter than comparison and
                // looser than the shifts. C-like ordering among themselves:
                // `|` (loosest) < `^` < `&` (tightest).
                "|" => (BinaryOp::BitOr, 4),
                "^" => (BinaryOp::BitXor, 5),
                "&" => (BinaryOp::BitAnd, 6),
                // Shifts bind just below additive.
                "<<" => (BinaryOp::Shl, 7),
                ">>" => (BinaryOp::Shr, 7),
                "+" => (BinaryOp::Add, 8),
                "-" => (BinaryOp::Subtract, 8),
                "*" => (BinaryOp::Multiply, 9),
                "/" => (BinaryOp::Divide, 9),
                "%" => (BinaryOp::Remainder, 9),
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

    /// Parse the optional `supervise POLICY` clause that may trail a
    /// `spawn NAME(args)`, yielding `None` when no clause is present.
    ///
    /// `supervise`, `restart`, `stop` and `escalate` are **contextual** — plain
    /// identifiers everywhere else, recognized only in this position — so no new
    /// keyword is reserved and existing code that uses those words as names is
    /// unaffected. The position is unambiguous: nothing else in the expression
    /// grammar may follow a completed `spawn NAME(...)` with a bare identifier.
    fn parse_supervise_clause(&mut self) -> Result<Option<SupervisionPolicy>, String> {
        let is_supervise = matches!(
            self.peek().map(|token| &token.kind),
            Some(TokenKind::Identifier(word)) if word == "supervise"
        );
        if !is_supervise {
            return Ok(None);
        }
        self.cursor += 1;
        let Some(TokenKind::Identifier(word)) = self.peek().map(|token| &token.kind) else {
            return Err(
                "expected a policy (`restart`, `stop`, or `escalate`) after `supervise`"
                    .to_string(),
            );
        };
        let Some(policy) = SupervisionPolicy::from_word(word) else {
            return Err(format!(
                "unknown supervision policy `{word}`: expected `restart`, `stop`, or `escalate`"
            ));
        };
        self.cursor += 1;
        Ok(Some(policy))
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

    /// True when the cursor is at a `->` arrow (the closure body separator).
    fn at_arrow(&self) -> bool {
        matches!(self.peek().map(|token| &token.kind), Some(TokenKind::Arrow))
    }

    /// Consume a `->` arrow, returning whether one was present.
    fn eat_arrow(&mut self) -> bool {
        if self.at_arrow() {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    /// True when the cursor is at the given `symbol` token.
    fn at_symbol(&self, symbol: &str) -> bool {
        matches!(self.peek().map(|token| &token.kind), Some(TokenKind::Symbol(actual)) if actual == symbol)
    }

    /// Consume the closure-parameter name identifier, or fail with a diagnostic
    /// message routed through the parser's `L0207` malformed-expression code.
    fn expect_closure_identifier(&mut self) -> Result<String, String> {
        match self.peek().map(|token| &token.kind) {
            Some(TokenKind::Identifier(name)) => {
                let name = name.clone();
                self.cursor += 1;
                Ok(name)
            }
            _ => Err("expected closure parameter name".to_string()),
        }
    }

    /// Consume a `>` that closes a generic type-argument list, splitting a `>>`
    /// shift token into two closers exactly like the declaration parser.
    fn eat_generic_close_expr(&mut self) -> bool {
        if self.pending_generic_close > 0 {
            self.pending_generic_close -= 1;
            return true;
        }
        match self.peek().map(|token| &token.kind) {
            Some(TokenKind::Symbol(symbol)) if symbol == ">" => {
                self.cursor += 1;
                true
            }
            Some(TokenKind::Symbol(symbol)) if symbol == ">>" => {
                self.cursor += 1;
                self.pending_generic_close += 1;
                true
            }
            _ => false,
        }
    }

    /// Parse a closure-parameter type, mirroring `Parser::expect_type`: primitive
    /// and aliased names, single/multi-argument generics, `void`, and the
    /// function type `fn(T, ...) -> R`. Emits the shared canonical spelling so a
    /// closure parameter type string-compares equal to a declared one.
    fn parse_type(&mut self) -> Result<TypeRef, String> {
        match self.peek().map(|token| token.kind.clone()) {
            Some(TokenKind::Identifier(name)) => {
                self.cursor += 1;
                let is_single = matches!(
                    name.as_str(),
                    "array" | "ptr" | "ref" | "rc" | "option" | "list" | "Future"
                );
                let is_multi = matches!(name.as_str(), "result" | "map");
                if (is_single || is_multi) && self.eat_symbol("<") {
                    let mut args = vec![self.parse_type()?];
                    if is_multi {
                        while self.eat_symbol(",") {
                            args.push(self.parse_type()?);
                        }
                    }
                    if !self.eat_generic_close_expr() {
                        return Err("expected `>` after generic type argument".to_string());
                    }
                    Ok(generic_type(&name, &args))
                } else {
                    Ok(TypeRef::new(name))
                }
            }
            Some(TokenKind::Keyword(Keyword::Void)) => {
                self.cursor += 1;
                Ok(TypeRef::new("void"))
            }
            Some(TokenKind::Keyword(Keyword::Fn)) => {
                self.cursor += 1;
                if !self.eat_symbol("(") {
                    return Err("expected `(` after `fn` in a function type".to_string());
                }
                let mut params = Vec::new();
                if !self.at_symbol(")") {
                    params.push(self.parse_type()?);
                    while self.eat_symbol(",") {
                        params.push(self.parse_type()?);
                    }
                }
                if !self.eat_symbol(")") {
                    return Err("expected `)` after function-type parameters".to_string());
                }
                if !self.eat_arrow() {
                    return Err("expected `->` in a function type".to_string());
                }
                let return_type = self.parse_type()?;
                Ok(function_type(&params, &return_type))
            }
            _ => Err("expected closure parameter type".to_string()),
        }
    }
}
