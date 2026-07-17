use lullaby_lexer::{Diagnostic, Keyword, Span, Token, TokenKind};

mod ast;
mod expr_parser;
mod format;
mod number_literal;

pub use ast::*;
pub use format::{format_program, format_program_with_comments};

use expr_parser::ExprParser;
use number_literal::normalize_number_literal;

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
    /// A count of `>` closers still owed from a `>>` token that was split while
    /// closing a nested generic type (`option<array<i64>>` lexes the trailing
    /// `>>` as one shift token). The type parser consumes these before advancing
    /// the cursor, so nested generics close correctly even though `<<`/`>>` are
    /// single tokens for the bitwise shift operators.
    pending_generic_close: usize,
    /// Monotonic parse-order counter that assigns each closure literal a stable
    /// `id`. Threaded through every `ExprParser` so ids are unique and stable
    /// across the whole program; each backend's closure-body table is keyed on
    /// this id.
    closure_counter: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            cursor: 0,
            diagnostics: Vec::new(),
            pending_generic_close: 0,
            closure_counter: 0,
        }
    }

    /// Build an `ExprParser` over `tokens` seeded with the current closure
    /// counter, run `f`, then fold the counter back so closure ids stay
    /// monotonic across every expression in the program.
    fn run_expr_parser<T>(
        &mut self,
        tokens: &'a [Token],
        f: impl FnOnce(&mut ExprParser<'a>) -> T,
    ) -> T {
        let mut expr_parser = ExprParser::new(tokens, self.closure_counter);
        let result = f(&mut expr_parser);
        self.closure_counter = expr_parser.closure_counter;
        result
    }

    /// Consume a single `>` that closes a generic type-argument list. A `>>`
    /// token (the bitwise shift) is split so its second `>` remains available
    /// for the enclosing generic. Returns `false` if the next token is neither a
    /// pending split, a `>`, nor a `>>`.
    fn eat_generic_close(&mut self) -> bool {
        if self.pending_generic_close > 0 {
            self.pending_generic_close -= 1;
            return true;
        }
        match &self.peek().kind {
            TokenKind::Symbol(symbol) if symbol == ">" => {
                self.advance();
                true
            }
            TokenKind::Symbol(symbol) if symbol == ">>" => {
                // Split `>>` into two generic closers: consume the token now and
                // owe one more `>` to the enclosing generic.
                self.advance();
                self.pending_generic_close += 1;
                true
            }
            _ => false,
        }
    }

    fn parse_program(&mut self) -> Program {
        let mut functions = Vec::new();
        let mut aliases = Vec::new();
        let mut structs = Vec::new();
        let mut enums = Vec::new();
        let mut imports = Vec::new();
        let mut traits = Vec::new();
        let mut impls = Vec::new();
        let mut consts = Vec::new();
        let mut actors = Vec::new();
        self.skip_newlines();

        // The optional `no-runtime` module directive: when present it is the first
        // non-comment line and marks the whole compilation unit as freestanding
        // (`no-runtime`) tier. It appears at most once, before any `import` or
        // declaration; a second occurrence, or one after any declaration, falls
        // through to the ordinary top-level error path below.
        let mut is_no_runtime = false;
        if self.eat_keyword(Keyword::NoRuntime).is_some() {
            is_no_runtime = true;
            self.expect_newline("expected newline after `no-runtime` directive");
            self.skip_newlines();
        }

        // Leading `import NAME` lines: zero or more, before any declaration.
        while self.eat_keyword(Keyword::Import).is_some() {
            if let Some(name) = self.expect_identifier("expected module name after `import`") {
                imports.push(name);
            }
            self.expect_newline("expected newline after import");
            self.skip_newlines();
        }

        while !self.at(TokenKindRef::Eof) {
            // An optional `pub` modifier prefixes an exported top-level
            // declaration. It only applies to `fn`/`struct`/`enum`/`alias`.
            let is_public = self.eat_keyword(Keyword::Pub).is_some();
            // An optional `async` modifier prefixes a `fn` declaration. `async`
            // only applies to functions; `async fn` and `pub async fn` are both
            // valid, but `async` before anything other than `fn` is an error.
            let is_async = self.eat_keyword(Keyword::Async).is_some();
            // An optional `extern` modifier prefixes a body-less `fn` declaration
            // of a C-ABI function imported at link time. `extern` only applies to
            // functions and is mutually exclusive with `async`.
            let is_extern = self.eat_keyword(Keyword::Extern).is_some();
            // An optional `export` modifier prefixes a normal (bodied) `fn`
            // declaration and exposes it under its plain C name as an externally
            // visible native symbol so C can call into it. `export` only applies
            // to functions and is mutually exclusive with `extern` (which imports
            // a body-less C symbol) and `async`.
            let is_export = self.eat_keyword(Keyword::Export).is_some();
            if is_extern && is_async {
                let token = self.peek();
                self.error(
                    "L0201",
                    "`extern` and `async` cannot be combined on a `fn` declaration",
                    token.span,
                );
            }
            if is_export && is_extern {
                let token = self.peek();
                self.error(
                    "L0201",
                    "`export` and `extern` cannot be combined on a `fn` declaration",
                    token.span,
                );
            }
            if is_export && is_async {
                let token = self.peek();
                self.error(
                    "L0201",
                    "`export` and `async` cannot be combined on a `fn` declaration",
                    token.span,
                );
            }
            if self.eat_keyword(Keyword::Fn).is_some() {
                let function = if is_extern {
                    self.parse_extern_function(is_public)
                } else {
                    self.parse_function(is_public, is_async, is_export)
                };
                if let Some(function) = function {
                    functions.push(function);
                }
            } else if is_async || is_extern || is_export {
                let token = self.peek();
                let modifier = if is_extern {
                    "extern"
                } else if is_export {
                    "export"
                } else {
                    "async"
                };
                self.error(
                    "L0201",
                    format!("`{modifier}` must prefix a `fn` declaration"),
                    token.span,
                );
                self.advance();
            } else if self.eat_keyword(Keyword::Const).is_some() {
                if let Some(declaration) = self.parse_const(is_public) {
                    consts.push(declaration);
                }
            } else if self.eat_keyword(Keyword::Alias).is_some() {
                if let Some(alias) = self.parse_alias(is_public) {
                    aliases.push(alias);
                }
            } else if self.eat_keyword(Keyword::Struct).is_some() {
                if let Some(declaration) = self.parse_struct(is_public) {
                    structs.push(declaration);
                }
            } else if self.eat_keyword(Keyword::Enum).is_some() {
                if let Some(declaration) = self.parse_enum(is_public) {
                    enums.push(declaration);
                }
            } else if self.eat_keyword(Keyword::Trait).is_some() {
                if let Some(declaration) = self.parse_trait(is_public) {
                    traits.push(declaration);
                }
            } else if self.eat_keyword(Keyword::Actor).is_some() {
                if let Some(declaration) = self.parse_actor(is_public) {
                    actors.push(declaration);
                }
            } else if !is_public && self.eat_keyword(Keyword::Impl).is_some() {
                if let Some(declaration) = self.parse_impl() {
                    impls.push(declaration);
                }
            } else if is_public {
                // `pub` was consumed but is not followed by an exportable
                // declaration.
                let token = self.peek();
                self.error(
                    "L0201",
                    "`pub` must prefix a `fn`, `struct`, `enum`, `alias`, `const`, `trait`, or `actor` declaration",
                    token.span,
                );
                self.advance();
            } else if self.at_any_keyword(&[Keyword::NoRuntime]) {
                // A `no-runtime` directive is only valid as the first non-comment
                // line of the module. A later occurrence (a second one, or one
                // after a declaration) is a misplacement, not a declaration.
                let token = self.peek().clone();
                self.error(
                    "L0201",
                    "the `no-runtime` directive must be the first line of the module",
                    token.span,
                );
                self.advance();
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
            imports,
            traits,
            impls,
            consts,
            actors,
            is_no_runtime,
        }
    }

    /// Parse an actor declaration `actor NAME`: a mandatory `state` block of
    /// `field type` lines, an optional `init <params>` constructor, and one or
    /// more `on <handler> <params> [-> T]` message handlers, each with an
    /// indented body. Sections appear in that order but `state` is the only
    /// required one; `init` is optional and at most one; at least one `on`
    /// handler is expected (enforced in semantics). All three inner keywords
    /// (`state`, `init`, `on`) are contextual identifiers recognized only here,
    /// so existing code that uses them as ordinary names is unaffected.
    fn parse_actor(&mut self, is_public: bool) -> Option<ActorDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected actor name after `actor`")?;
        self.expect_newline("expected newline after actor name");
        self.expect(TokenKindRef::Indent, "expected indented actor body")?;

        let mut state = Vec::new();
        let mut init: Option<ActorInit> = None;
        let mut handlers = Vec::new();

        while !self.at(TokenKindRef::Dedent) && !self.at(TokenKindRef::Eof) {
            match &self.peek().kind {
                TokenKind::Identifier(word) if word == "state" => {
                    self.advance();
                    state = self.parse_actor_state()?;
                }
                TokenKind::Identifier(word) if word == "init" => {
                    let init_span = self.peek().span;
                    self.advance();
                    let params = self.parse_param_list()?;
                    self.expect_newline("expected newline after actor init parameters");
                    self.expect(TokenKindRef::Indent, "expected indented actor init body")?;
                    let body = self.parse_block(&[BlockEnd::Dedent]);
                    self.expect(TokenKindRef::Dedent, "expected actor init body dedent")?;
                    init = Some(ActorInit {
                        params,
                        body,
                        span: init_span,
                    });
                }
                TokenKind::Identifier(word) if word == "on" => {
                    let handler_span = self.peek().span;
                    self.advance();
                    let handler = self.parse_actor_handler(handler_span)?;
                    handlers.push(handler);
                }
                _ => {
                    self.error(
                        "L0201",
                        "expected `state`, `init`, or `on` in actor body",
                        self.peek().span,
                    );
                    return None;
                }
            }
            self.skip_newlines();
        }
        self.expect(TokenKindRef::Dedent, "expected actor body dedent")?;
        Some(ActorDecl {
            name,
            state,
            init,
            handlers,
            span,
            is_public,
        })
    }

    /// Parse the `state` block of an actor: an indented list of `field type`
    /// lines, exactly like a struct body.
    fn parse_actor_state(&mut self) -> Option<Vec<StructField>> {
        self.expect_newline("expected newline after `state`");
        self.expect(TokenKindRef::Indent, "expected indented actor state fields")?;
        let mut fields = Vec::new();
        while !self.at(TokenKindRef::Dedent) && !self.at(TokenKindRef::Eof) {
            let field_name = self.expect_identifier("expected state field name")?;
            let ty = self.expect_type("expected state field type")?;
            self.expect_newline("expected newline after state field");
            fields.push(StructField {
                name: field_name,
                ty,
            });
        }
        self.expect(TokenKindRef::Dedent, "expected actor state dedent")?;
        Some(fields)
    }

    /// Parse a single `on <name> <params> [-> T]` handler and its indented body
    /// (the `on` keyword-word has already been consumed). A `-> T` clause marks a
    /// reply (`ask`) handler; its absence marks a fire-and-forget (`tell`)
    /// handler.
    fn parse_actor_handler(&mut self, span: Span) -> Option<ActorHandler> {
        let name = self.expect_identifier("expected handler name after `on`")?;
        let params = self.parse_param_list()?;
        let reply_type = if self.eat(TokenKindRef::Arrow) {
            Some(self.expect_type("expected handler reply type after `->`")?)
        } else {
            None
        };
        self.expect_newline("expected newline after actor handler signature");
        self.expect(TokenKindRef::Indent, "expected indented actor handler body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected actor handler body dedent")?;
        Some(ActorHandler {
            name,
            params,
            reply_type,
            body,
            span,
        })
    }

    /// Parse `const NAME type = <expr>`. Unlike a `let`, the type annotation is
    /// mandatory; the initializer is any expression line (validated as a
    /// *constant expression* and evaluated during semantic analysis).
    fn parse_const(&mut self, is_public: bool) -> Option<ConstDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected constant name after `const`")?;
        let ty = self.expect_type("expected constant type after its name")?;
        if !self.eat_symbol("=") {
            self.error(
                "L0206",
                "expected `=` in const declaration",
                self.peek().span,
            );
            return None;
        }
        let value = self.parse_value_expr(span, "expected newline after const declaration")?;
        Some(ConstDecl {
            name,
            ty,
            value,
            span,
            is_public,
        })
    }

    /// Parse `trait NAME` followed by an indented list of method signatures
    /// `fn method self [param Type ...] -> Ret`. The receiver must be named
    /// `self`; the method has no body.
    fn parse_trait(&mut self, is_public: bool) -> Option<TraitDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected trait name")?;
        self.expect_newline("expected newline after trait name");
        self.expect(TokenKindRef::Indent, "expected indented trait methods")?;
        let mut methods = Vec::new();
        while !self.at(TokenKindRef::Dedent) && !self.at(TokenKindRef::Eof) {
            if self.eat_keyword(Keyword::Fn).is_none() {
                self.error(
                    "L0216",
                    "expected `fn` method signature in trait body",
                    self.peek().span,
                );
                return None;
            }
            let method = self.parse_method_sig()?;
            methods.push(method);
            self.skip_newlines();
        }
        self.expect(TokenKindRef::Dedent, "expected trait body dedent")?;
        Some(TraitDecl {
            name,
            methods,
            span,
            is_public,
        })
    }

    /// Parse a single trait method signature `method self [param Type ...] -> Ret`
    /// (the leading `fn` has been consumed). The first parameter must be `self`;
    /// remaining parameters are returned in `params`.
    fn parse_method_sig(&mut self) -> Option<MethodSig> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected trait method name")?;
        let receiver = self.expect_identifier("expected `self` receiver in trait method")?;
        if receiver != "self" {
            self.error(
                "L0216",
                "the first parameter of a trait method must be `self`",
                span,
            );
            return None;
        }
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
                "L0216",
                "expected `->` before trait method return type",
                self.peek().span,
            );
            return None;
        }
        let return_type = self.expect_type("expected trait method return type")?;
        self.expect_newline("expected newline after trait method signature");
        Some(MethodSig {
            name,
            params,
            return_type,
            span,
        })
    }

    /// Parse an impl block. Two forms are accepted:
    ///
    /// - `impl Trait for Type` — a trait implementation. `trait_name` is set and
    ///   `type_params` is empty; each method's `self` has the implementing type.
    /// - `impl Type<T>` (or `impl Type`) — an **inherent** impl on a (possibly
    ///   generic) type. `trait_name` is left empty, `type_params` carries the
    ///   `<T>` list (parsed by the shared `parse_type_params` helper), and each
    ///   method's `self` is typed with the full instantiation spelling (`Box<T>`)
    ///   so `T` is in scope over the method body.
    ///
    /// Each method is an ordinary `fn` whose first parameter is `self`.
    fn parse_impl(&mut self) -> Option<ImplDecl> {
        let span = self.previous().span;
        let head = self.expect_identifier("expected trait or type name after `impl`")?;
        // `impl Trait for Type` (trait impl) vs `impl Type<T>` (inherent impl) is
        // decided by whether `for` follows the head name. A trait impl's head is
        // the trait; an inherent impl's head is the type, optionally followed by
        // `<T>` generic parameters.
        if self.eat_keyword(Keyword::For).is_some() {
            let type_name =
                self.expect_identifier("expected implementing type name after `for`")?;
            let methods = self.parse_impl_methods(&TypeRef::new(&type_name), &[])?;
            return Some(ImplDecl {
                trait_name: head,
                type_name,
                type_params: Vec::new(),
                methods,
                span,
            });
        }
        // Inherent impl: an optional `<T>` / `<T: Trait>` type-parameter list
        // (same helper, `L0394` checks, that generic functions/structs/enums use).
        let type_params = self.parse_type_params(span)?;
        // The `self` receiver of every method is the full instantiation spelling,
        // e.g. `Box<T>` for `impl Box<T>`, or the bare name for a non-generic
        // inherent impl. This puts `T` in scope over the method signatures/bodies.
        let self_ty = if type_params.is_empty() {
            TypeRef::new(&head)
        } else {
            let args = type_params
                .iter()
                .map(|tp| tp.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            TypeRef::new(format!("{head}<{args}>"))
        };
        let methods = self.parse_impl_methods(&self_ty, &type_params)?;
        Some(ImplDecl {
            trait_name: String::new(),
            type_name: head,
            type_params,
            methods,
            span,
        })
    }

    /// Parse the indented `fn` method bodies of an impl block. `self_ty` is the
    /// type injected as each method's `self` parameter; `method_type_params` are
    /// attached to every method so an inherent generic impl's `T` is in scope.
    fn parse_impl_methods(
        &mut self,
        self_ty: &TypeRef,
        method_type_params: &[TypeParam],
    ) -> Option<Vec<Function>> {
        self.expect_newline("expected newline after impl header");
        self.expect(TokenKindRef::Indent, "expected indented impl methods")?;
        let mut methods = Vec::new();
        while !self.at(TokenKindRef::Dedent) && !self.at(TokenKindRef::Eof) {
            if self.eat_keyword(Keyword::Fn).is_none() {
                self.error(
                    "L0216",
                    "expected `fn` method body in impl block",
                    self.peek().span,
                );
                return None;
            }
            let method = self.parse_impl_method(self_ty, method_type_params)?;
            methods.push(method);
            self.skip_newlines();
        }
        self.expect(TokenKindRef::Dedent, "expected impl body dedent")?;
        Some(methods)
    }

    /// Parse an impl method `method self [param Type ...] -> Ret` + indented body
    /// (the leading `fn` has been consumed). The `self` receiver is untyped in
    /// source; its type is `self_ty`, injected as the first parameter so the rest
    /// of the pipeline sees an ordinary function. `method_type_params` (the
    /// inherent impl's `<T>`, empty for a trait impl) are attached so the body
    /// type-checks with the type variables in scope.
    fn parse_impl_method(
        &mut self,
        self_ty: &TypeRef,
        method_type_params: &[TypeParam],
    ) -> Option<Function> {
        let fn_span = self.previous().span;
        let name = self.expect_identifier("expected method name after `fn`")?;
        let receiver = self.expect_identifier("expected `self` receiver in impl method")?;
        if receiver != "self" {
            self.error(
                "L0216",
                "the first parameter of an impl method must be `self`",
                fn_span,
            );
            return None;
        }
        let mut params = vec![Param {
            name: "self".to_string(),
            ty: self_ty.clone(),
        }];
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
                "expected `->` before impl method return type",
                self.peek().span,
            );
            return None;
        }
        let return_type = self.expect_type("expected impl method return type after `->`")?;
        self.expect_newline("expected newline after method signature");
        self.expect(TokenKindRef::Indent, "expected indented method body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected method body dedent")?;
        Some(Function {
            name,
            type_params: method_type_params.to_vec(),
            params,
            return_type,
            body,
            span: fn_span,
            is_public: false,
            is_async: false,
            is_extern: false,
            is_export: false,
        })
    }

    /// Parse `enum NAME` followed by an indented list of `Variant type...` lines.
    /// Each variant is a name plus zero or more positional, unnamed payload types.
    fn parse_enum(&mut self, is_public: bool) -> Option<EnumDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected enum name")?;
        // Optional `<T>` / `<T: Trait + Other>` generic type parameters, parsed by
        // the same helper (and with the same `L0394` duplicate/shadow checks) that
        // generic functions and generic structs use. Absent for a non-generic enum.
        let type_params = self.parse_type_params(span)?;
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
            type_params,
            variants,
            span,
            is_public,
        })
    }

    /// Parse `struct NAME` followed by an indented list of `field type` lines.
    fn parse_struct(&mut self, is_public: bool) -> Option<StructDecl> {
        let span = self.previous().span;
        let name = self.expect_identifier("expected struct name")?;
        // Optional `<T>` / `<T: Trait + Other>` generic type parameters, parsed by
        // the same helper (and with the same `L0394` duplicate/shadow checks) that
        // generic functions use. Absent for a non-generic struct.
        let type_params = self.parse_type_params(span)?;
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
        Some(StructDecl {
            name,
            type_params,
            fields,
            span,
            is_public,
        })
    }

    /// Parse `alias NAME = TYPE`.
    fn parse_alias(&mut self, is_public: bool) -> Option<AliasDecl> {
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
        Some(AliasDecl {
            name,
            target,
            span,
            is_public,
        })
    }

    /// Parse a space-separated function parameter list up to `->`/newline/EOF.
    ///
    /// Each parameter is `name Type`. Consecutive parameters that share a type may
    /// be **grouped** by comma-separating their names before the single type:
    /// `x, y, z f64` is exactly `x f64 y f64 z f64`. The type is written once but
    /// still explicit and applied to every name in the group; the ungrouped
    /// `name Type name Type` form is unchanged. Grouping is expanded here, so the
    /// rest of the compiler only ever sees individual `Param`s.
    fn parse_param_list(&mut self) -> Option<Vec<Param>> {
        let mut params = Vec::new();
        while !self.at(TokenKindRef::Arrow)
            && !self.at(TokenKindRef::Newline)
            && !self.at(TokenKindRef::Eof)
        {
            let mut names = vec![self.expect_identifier("expected parameter name")?];
            while self.eat_symbol(",") {
                names.push(self.expect_identifier("expected parameter name after `,`")?);
            }
            let ty = self.expect_type("expected parameter type")?;
            for name in names {
                params.push(Param {
                    name,
                    ty: ty.clone(),
                });
            }
        }
        Some(params)
    }

    fn parse_function(
        &mut self,
        is_public: bool,
        is_async: bool,
        is_export: bool,
    ) -> Option<Function> {
        let fn_span = self.previous().span;
        let name = self.expect_identifier("expected function name after `fn`")?;
        let type_params = self.parse_type_params(fn_span)?;
        let params = self.parse_param_list()?;

        // The `-> T` return-type clause is optional: when it is omitted the
        // return type is inferred from the function body during semantic
        // analysis (recorded here as the `INFERRED_RETURN` sentinel).
        let return_type = if self.eat(TokenKindRef::Arrow) {
            self.expect_type("expected function return type after `->`")?
        } else {
            TypeRef::new(INFERRED_RETURN)
        };
        self.expect_newline("expected newline after function signature");
        self.expect(TokenKindRef::Indent, "expected indented function body")?;
        let body = self.parse_block(&[BlockEnd::Dedent]);
        self.expect(TokenKindRef::Dedent, "expected function body dedent")?;

        Some(Function {
            name,
            type_params,
            params,
            return_type,
            body,
            span: fn_span,
            is_public,
            is_async,
            is_extern: false,
            is_export,
        })
    }

    /// Parse an `extern fn NAME params -> Ret` declaration: a body-less signature
    /// for a C-ABI function imported at link time. The `fn` keyword has already
    /// been consumed. Generic type parameters, a `self` receiver, and an indented
    /// body are not part of an extern declaration; the line ends after the return
    /// type. The signature is registered so calls type-check like ordinary calls.
    fn parse_extern_function(&mut self, is_public: bool) -> Option<Function> {
        let fn_span = self.previous().span;
        let name = self.expect_identifier("expected function name after `extern fn`")?;
        let params = self.parse_param_list()?;
        if !self.eat(TokenKindRef::Arrow) {
            self.error(
                "L0202",
                "expected `->` before extern function return type",
                self.peek().span,
            );
            return None;
        }
        let return_type = self.expect_type("expected function return type after `->`")?;
        self.expect_newline("expected newline after extern function signature");
        Some(Function {
            name,
            type_params: Vec::new(),
            params,
            return_type,
            body: Vec::new(),
            span: fn_span,
            is_public,
            is_async: false,
            is_extern: true,
            is_export: false,
        })
    }

    /// Parse an optional `<T, U>` type-parameter list that follows a function
    /// name. Returns an empty list when no `<` follows. Each name must be a fresh
    /// identifier that is not a duplicate in the list and does not shadow a known
    /// primitive or built-in type; either violation is `L0394`.
    fn parse_type_params(&mut self, fn_span: Span) -> Option<Vec<TypeParam>> {
        if !self.eat_symbol("<") {
            return Some(Vec::new());
        }
        let mut params: Vec<TypeParam> = Vec::new();
        loop {
            let param_span = self.peek().span;
            let name = self.expect_identifier("expected type parameter name")?;
            // Optional trait bounds `T: Trait` or `T: A + B`.
            let mut bounds = Vec::new();
            if self.eat_symbol(":") {
                loop {
                    bounds.push(
                        self.expect_identifier("expected trait name in type-parameter bound")?,
                    );
                    if !self.eat_symbol("+") {
                        break;
                    }
                }
            }
            if is_builtin_type_name(&name) {
                self.error(
                    "L0394",
                    format!(
                        "type parameter `{name}` shadows a built-in type; choose a distinct name"
                    ),
                    param_span,
                );
            } else if params.iter().any(|p| p.name == name) {
                self.error(
                    "L0394",
                    format!("duplicate type parameter `{name}` in the `<...>` list"),
                    param_span,
                );
            } else {
                params.push(TypeParam { name, bounds });
            }
            if self.eat_symbol(">") {
                break;
            }
            if !self.eat_symbol(",") {
                self.error(
                    "L0394",
                    "expected `,` or `>` in the type parameter list",
                    self.peek().span,
                );
                return None;
            }
        }
        let _ = fn_span;
        Some(params)
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
            if self.at(TokenKindRef::Newline) {
                self.expect_newline("expected newline after return statement");
                return Some(Stmt::Return(None));
            }
            let expr = self.parse_value_expr(span, "expected newline after return statement")?;
            return Some(Stmt::Return(Some(expr)));
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

        if self.eat_keyword(Keyword::Asm).is_some() {
            return self.parse_asm();
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
        let value = self.parse_value_expr(span, "expected newline after let binding")?;
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
        let mut target_parser =
            ExprParser::new(&self.tokens[self.cursor..op_pos], self.closure_counter);
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
        let value = self.parse_value_expr(span, "expected newline after assignment")?;
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
        // `for x in COLLECTION` iterates elements; `for x from S to E` is the
        // numeric range form.
        if self.eat_keyword(Keyword::In).is_some() {
            let iterable = self.parse_expr_line(span)?;
            self.expect_newline("expected newline after for-in header");
            self.expect(TokenKindRef::Indent, "expected indented for body")?;
            let body = self.parse_block(&[BlockEnd::Dedent]);
            self.expect(TokenKindRef::Dedent, "expected for body dedent")?;
            return Some(Stmt::ForEach {
                name,
                iterable,
                body,
                span,
            });
        }
        if self.eat_keyword(Keyword::From).is_none() {
            self.error(
                "L0209",
                "expected `from` or `in` in for loop",
                self.peek().span,
            );
            return None;
        }
        let start = self.parse_expr_until_keywords(span, &[Keyword::To])?;
        if self.eat_keyword(Keyword::To).is_none() {
            self.error("L0217", "expected `to` in for loop", self.peek().span);
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

    /// Parse an `asm` statement: the `asm` keyword followed by a comma-separated
    /// list of integer byte literals on the same line, e.g.
    /// `asm 72, 199, 192, 42, 0, 0, 0`. The bytes are emitted verbatim into the
    /// current function's `.text` by the native backend. The parser only checks
    /// the shape (at least one integer literal, comma-separated); semantics
    /// validates each byte is in `0..=255` and that the statement is inside an
    /// `unsafe` block.
    fn parse_asm(&mut self) -> Option<Stmt> {
        let span = self.previous().span;
        let mut bytes = Vec::new();
        loop {
            let byte = self.expect_number("expected integer byte literal in asm statement")?;
            bytes.push(byte);
            if self.eat_symbol(",") {
                continue;
            }
            break;
        }
        self.expect_newline("expected newline after asm statement");
        Some(Stmt::Asm { bytes, span })
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

    /// Parse the value expression to the right of `=` (a `let`/assignment) or a
    /// `return`, consuming its statement terminator. A leading `match` keyword
    /// begins a block `match` **expression**: it spans multiple indented lines
    /// and consumes its own closing dedent, so no trailing newline follows it.
    /// Every other expression is a single line terminated by a newline. This is
    /// what makes `let x = match ...`, `return match ...`, and `x = match ...`
    /// usable in value position while the indentation-only surface is preserved.
    fn parse_value_expr(&mut self, span: Span, newline_msg: &'static str) -> Option<Expr> {
        if self.at_any_keyword(&[Keyword::Match]) {
            self.advance();
            self.parse_match()
        } else {
            let expr = self.parse_expr_line(span)?;
            self.expect_newline(newline_msg);
            Some(expr)
        }
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
        } else if self.at_any_keyword(&[Keyword::Match]) {
            // A nested `match` expression as an inline arm value:
            // `Variant -> match ...`. It spans its own indented arm block and
            // consumes its closing dedent, so no trailing newline follows it.
            self.advance();
            let match_expr = self.parse_match()?;
            Some(vec![Stmt::Expr(match_expr)])
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

        // The freestanding static-buffer arena form: `region NAME in BUFFER`
        // (`documents/freestanding_tier_design.md` §5). The arena's memory is the
        // caller's fixed buffer, so there is no `size=` to state — the extent is
        // the buffer's. Checked against the buffer in `semantics_arena.rs`.
        if self.eat_keyword(Keyword::In).is_some() {
            let backing = self.expect_identifier("expected backing buffer name after `in`")?;
            self.expect_newline("expected newline after region declaration");
            return Some(Stmt::Region(RegionDecl {
                name,
                size: 0,
                align: None,
                kind: String::from("static"),
                mutable: true,
                backing: Some(backing),
                span,
            }));
        }

        if !self.eat_symbol(":") {
            self.error(
                "L0210",
                "expected `:` or `in <buffer>` after region name",
                self.peek().span,
            );
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
            backing: None,
            span,
        }))
    }

    fn expect_number(&mut self, message: &'static str) -> Option<i64> {
        match &self.peek().kind {
            TokenKind::Number(value) => {
                let parsed = normalize_number_literal(value).and_then(|v| v.parse::<i64>().ok());
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

        let tokens = &self.tokens[start..self.cursor];
        match self.run_expr_parser(tokens, ExprParser::parse) {
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
                // Single-argument generics reuse one shape; `result` accepts a
                // comma-separated argument list. All emit the shared canonical
                // spelling so string-based `TypeRef` equality holds everywhere.
                let is_single = matches!(
                    name.as_str(),
                    "array" | "ptr" | "ref" | "rc" | "option" | "list" | "Future"
                );
                let is_multi = matches!(name.as_str(), "result" | "map");
                // A user-defined generic type `Name<A, B>` (e.g. `Box<i64>`) uses
                // the same angle-bracket spelling as the built-in generics. Any
                // identifier that is not a fixed-arity built-in constructor accepts
                // a comma-separated argument list here; arity and whether the head
                // actually names a declared generic type are checked in semantics.
                let is_user_generic = !is_single && !is_multi && self.at_symbol("<");
                if (is_single || is_multi) && self.eat_symbol("<") {
                    let mut args = vec![self.expect_type("expected generic type argument")?];
                    if is_multi {
                        while self.eat_symbol(",") {
                            args.push(self.expect_type("expected generic type argument")?);
                        }
                    } else if name == "array" && self.eat_symbol(",") {
                        // A const-sized array `array<T, N>`: the extent `N` is a
                        // constant expression (a literal or a named constant), not
                        // a type. It is captured verbatim here and resolved and
                        // validated by the semantic extent pass; the checker and
                        // every backend see the erased `array<T>`.
                        let extent = self.expect_array_extent()?;
                        args.push(TypeRef::new(extent));
                    }
                    if !self.eat_generic_close() {
                        self.error(
                            "L0203",
                            "expected `>` after generic type argument",
                            self.peek().span,
                        );
                        return None;
                    }
                    Some(generic_type(&name, &args))
                } else if is_user_generic && self.eat_symbol("<") {
                    let mut args = vec![self.expect_type("expected generic type argument")?];
                    while self.eat_symbol(",") {
                        args.push(self.expect_type("expected generic type argument")?);
                    }
                    if !self.eat_generic_close() {
                        self.error(
                            "L0203",
                            "expected `>` after generic type argument",
                            self.peek().span,
                        );
                        return None;
                    }
                    Some(generic_type(&name, &args))
                } else {
                    Some(TypeRef::new(name))
                }
            }
            TokenKind::Keyword(Keyword::Void) => {
                self.advance();
                Some(TypeRef::new("void"))
            }
            // Function type `fn(T1, T2) -> R`. Zero or more parameter types, then
            // an arrow and a return type. Emitted in the canonical spelling so
            // string-based `TypeRef` equality holds everywhere.
            TokenKind::Keyword(Keyword::Fn) => {
                self.advance();
                if !self.eat_symbol("(") {
                    self.error(
                        "L0203",
                        "expected `(` after `fn` in a function type",
                        self.peek().span,
                    );
                    return None;
                }
                let mut params = Vec::new();
                if !self.at_symbol(")") {
                    params.push(self.expect_type("expected function-type parameter type")?);
                    while self.eat_symbol(",") {
                        params.push(self.expect_type("expected function-type parameter type")?);
                    }
                }
                if !self.eat_symbol(")") {
                    self.error(
                        "L0203",
                        "expected `)` after function-type parameters",
                        self.peek().span,
                    );
                    return None;
                }
                if !self.eat(TokenKindRef::Arrow) {
                    self.error(
                        "L0203",
                        "expected `->` in a function type",
                        self.peek().span,
                    );
                    return None;
                }
                let return_type = self.expect_type("expected function-type return type")?;
                Some(function_type(&params, &return_type))
            }
            _ => {
                self.error("L0203", message, self.peek().span);
                None
            }
        }
    }

    /// Parse the extent operand of a const-sized array type `array<T, N>`. `N`
    /// is either a plain integer literal or a bare identifier naming a constant;
    /// both are captured as canonical text (a literal is normalized to decimal)
    /// for the semantic extent pass to resolve and validate. An arithmetic
    /// expression, a float, or any other token is rejected here.
    fn expect_array_extent(&mut self) -> Option<String> {
        match &self.peek().kind {
            TokenKind::Number(text) => {
                let text = text.clone();
                let span = self.peek().span;
                self.advance();
                match number_literal::parse_plain_integer_literal(&text) {
                    Some(value) => Some(value.to_string()),
                    None => {
                        self.error(
                            "L0203",
                            "an array extent `N` must be a plain integer literal or a named constant",
                            span,
                        );
                        None
                    }
                }
            }
            TokenKind::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Some(name)
            }
            _ => {
                self.error(
                    "L0203",
                    "expected an array extent `N` (an integer literal or a named constant) after `,`",
                    self.peek().span,
                );
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
                if matches!(symbol.as_str(), "=" | "+=" | "-=" | "*=" | "/=" | "%=") =>
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
            "%=" => AssignOp::Remainder,
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
            format!("`{feature}` syntax is planned and is not supported by this compiler"),
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
        Keyword::Package => "package",
        Keyword::Union => "union",
        Keyword::Interface => "interface",
        Keyword::Class => "class",
        Keyword::Switch => "switch",
        Keyword::Catch => "catch",
        Keyword::Coroutine => "coroutine",
        _ => return None,
    })
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
#[path = "parser_tests.rs"]
mod tests;
