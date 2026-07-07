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
    /// Module names imported at the top of this file (`import NAME`), in source
    /// order. Empty for a single-file program with no imports. Serde-defaulted so
    /// existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Trait declarations (`trait NAME` + method signatures). Serde-defaulted so
    /// existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traits: Vec<TraitDecl>,
    /// Trait implementations (`impl Trait for Type` + method bodies).
    /// Serde-defaulted so existing artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impls: Vec<ImplDecl>,
}

/// A trait declaration: `trait NAME` followed by indented method signatures.
/// Each signature is `fn method self [param Type ...] -> Ret` with no body; the
/// receiver is named `self` and `Self` may appear as a parameter/return type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraitDecl {
    pub name: String,
    pub methods: Vec<MethodSig>,
    pub span: Span,
    /// True when the declaration is exported with `pub`. Serde-defaulted to
    /// `false` so existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_public: bool,
}

/// A single trait method signature (no body). The first parameter is always the
/// `self` receiver and is not stored in `params`; `params` holds the remaining
/// parameters after `self`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodSig {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: TypeRef,
    pub span: Span,
}

/// A trait implementation: `impl Trait for Type` followed by indented method
/// bodies. Each method is an ordinary [`Function`] whose first parameter is
/// `self` (its type is the implementing type).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImplDecl {
    pub trait_name: String,
    pub type_name: String,
    pub methods: Vec<Function>,
    pub span: Span,
}

/// A struct declaration: `struct NAME` followed by indented `field type` lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructDecl {
    pub name: String,
    pub fields: Vec<StructField>,
    pub span: Span,
    /// True when the declaration is exported with `pub`. Serde-defaulted to
    /// `false` so existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_public: bool,
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
    /// True when the declaration is exported with `pub`. Serde-defaulted to
    /// `false` so existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_public: bool,
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
    /// True when the declaration is exported with `pub`. Serde-defaulted to
    /// `false` so existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_public: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    /// Declared type parameters `<T, U>` or bounded `<T: Trait>` that follow the
    /// function name, in source order. Empty for a non-generic function.
    /// Serde-defaulted to an empty list so existing single-file artifacts and AST
    /// snapshots stay valid. A type-parameter name is in scope as a type variable
    /// within this function's signature and body only, where it is spelled as an
    /// ordinary `TypeRef`; its bounds name traits the type variable must satisfy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_params: Vec<TypeParam>,
    pub params: Vec<Param>,
    pub return_type: TypeRef,
    pub body: Vec<Stmt>,
    pub span: Span,
    /// True when the declaration is exported with `pub`. Serde-defaulted to
    /// `false` so existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_public: bool,
}

/// serde `skip_serializing_if` predicate for the `is_public` visibility flag.
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: TypeRef,
}

/// A generic type parameter `T` or a bounded one `T: Trait + Other`. The name is
/// the type variable; `bounds` names the traits it must satisfy (empty when
/// unbounded).
///
/// To keep existing AST snapshots and single-file artifacts valid, an unbounded
/// type parameter serializes as the bare string `"T"` (the historical shape),
/// and a bounded one as an object `{ "name": "T", "bounds": ["Trait"] }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParam {
    pub name: String,
    pub bounds: Vec<String>,
}

impl TypeParam {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            bounds: Vec::new(),
        }
    }
}

impl Serialize for TypeParam {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.bounds.is_empty() {
            serializer.serialize_str(&self.name)
        } else {
            use serde::ser::SerializeStruct;
            let mut state = serializer.serialize_struct("TypeParam", 2)?;
            state.serialize_field("name", &self.name)?;
            state.serialize_field("bounds", &self.bounds)?;
            state.end()
        }
    }
}

impl<'de> Deserialize<'de> for TypeParam {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bare(String),
            Bounded { name: String, bounds: Vec<String> },
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Bare(name) => TypeParam {
                name,
                bounds: Vec::new(),
            },
            Repr::Bounded { name, bounds } => TypeParam { name, bounds },
        })
    }
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
    /// `rc<i64>` yields `i64`. For a multi-argument spelling this returns the
    /// full comma-separated argument text as one `TypeRef`; use `generic_args`
    /// to split it into nesting-aware top-level arguments.
    pub fn generic_arg(&self, ctor: &str) -> Option<TypeRef> {
        self.name
            .strip_prefix(&format!("{ctor}<"))
            .and_then(|name| name.strip_suffix('>'))
            .map(TypeRef::new)
    }

    /// All top-level, nesting-aware comma-separated arguments of a `ctor<...>`
    /// spelling. `result<array<i64>, string>.generic_args("result")` yields
    /// `[array<i64>, string]`; a single-argument `option<i64>` yields `[i64]`.
    /// Returns `None` when the spelling is not a `ctor<...>` form.
    pub fn generic_args(&self, ctor: &str) -> Option<Vec<TypeRef>> {
        let inner = self
            .name
            .strip_prefix(&format!("{ctor}<"))
            .and_then(|name| name.strip_suffix('>'))?;
        Some(split_generic_args(inner))
    }

    /// The payload type `T` of an `option<T>` spelling, if any.
    pub fn option_element(&self) -> Option<TypeRef> {
        self.generic_args("option")
            .filter(|args| args.len() == 1)
            .map(|mut args| args.remove(0))
    }

    /// The `(T, E)` type pair of a `result<T, E>` spelling, if any.
    pub fn result_args(&self) -> Option<(TypeRef, TypeRef)> {
        self.generic_args("result")
            .filter(|args| args.len() == 2)
            .map(|mut args| {
                let error = args.remove(1);
                let ok = args.remove(0);
                (ok, error)
            })
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

    /// The `(param types, return type)` of a function type spelled
    /// `fn(T1, T2) -> R`, if this is a function type. `fn() -> void` yields an
    /// empty parameter list. Parsing is nesting-aware so `fn(array<i64>) -> i64`
    /// splits correctly.
    pub fn function_signature(&self) -> Option<(Vec<TypeRef>, TypeRef)> {
        let rest = self.name.strip_prefix("fn(")?;
        // Find the `)` that closes the parameter list, honoring nested `<...>`
        // and `(...)` so a nested function type does not end the scan early.
        let mut depth = 0usize;
        let mut close = None;
        for (index, ch) in rest.char_indices() {
            match ch {
                '<' | '(' => depth += 1,
                '>' | ')' if depth > 0 => depth -= 1,
                ')' => {
                    close = Some(index);
                    break;
                }
                _ => {}
            }
        }
        let close = close?;
        let params_text = &rest[..close];
        let after = rest[close + 1..].strip_prefix(" -> ")?;
        let params = if params_text.is_empty() {
            Vec::new()
        } else {
            split_generic_args(params_text)
        };
        Some((params, TypeRef::new(after)))
    }

    /// True when this type is a function type `fn(...) -> R`.
    pub fn is_function(&self) -> bool {
        self.function_signature().is_some()
    }
}

/// Build the canonical spelling of a function type `fn(a, b) -> r`. This is the
/// single source of truth for function-type text so string-based `TypeRef`
/// equality holds across the parser, semantics, runtime, and IR. Parameter
/// types are joined with `", "`.
pub fn function_type(params: &[TypeRef], return_type: &TypeRef) -> TypeRef {
    let joined = params
        .iter()
        .map(|param| param.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    TypeRef::new(format!("fn({joined}) -> {}", return_type.name))
}

/// True for a type name that is a built-in primitive, scalar, or generic-type
/// constructor. A user-declared type parameter may not shadow one of these.
pub fn is_builtin_type_name(name: &str) -> bool {
    matches!(
        name,
        "i64"
            | "f64"
            | "bool"
            | "string"
            | "char"
            | "byte"
            | "void"
            | "array"
            | "list"
            | "map"
            | "option"
            | "result"
            | "ptr"
            | "ref"
            | "rc"
    )
}

/// Build the canonical spelling of a generic type `ctor<a, b, ...>`. This is the
/// single source of truth for generic-type text so string-based `TypeRef`
/// equality holds across the parser, semantics, and IR lowerer. Arguments are
/// joined with `", "` (comma-space), matching the parser's `expect_type`.
pub fn generic_type(ctor: &str, args: &[TypeRef]) -> TypeRef {
    let joined = args
        .iter()
        .map(|arg| arg.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    TypeRef::new(format!("{ctor}<{joined}>"))
}

/// Split the inner text of a `ctor<...>` spelling into its top-level,
/// nesting-aware comma-separated arguments. Commas inside nested `<...>` are not
/// splits, so `array<i64>, string` yields `["array<i64>", "string"]`.
fn split_generic_args(inner: &str) -> Vec<TypeRef> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in inner.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(TypeRef::new(inner[start..index].trim()));
                start = index + 1;
            }
            _ => {}
        }
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        args.push(TypeRef::new(tail));
    }
    args
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
    /// A `'c'` char literal: exactly one Unicode scalar.
    Char(char),
    /// A `byte` literal never appears in source; `byte`/`byte_val` builtins
    /// produce and consume byte values at runtime.
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
        let mut imports = Vec::new();
        let mut traits = Vec::new();
        let mut impls = Vec::new();
        self.skip_newlines();

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
            if self.eat_keyword(Keyword::Fn).is_some() {
                if let Some(function) = self.parse_function(is_public) {
                    functions.push(function);
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
                    "`pub` must prefix a `fn`, `struct`, `enum`, `alias`, or `trait` declaration",
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
        }
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

    /// Parse `impl Trait for Type` followed by an indented list of method bodies.
    /// Each method is an ordinary `fn` whose first parameter is `self`.
    fn parse_impl(&mut self) -> Option<ImplDecl> {
        let span = self.previous().span;
        let trait_name = self.expect_identifier("expected trait name after `impl`")?;
        if self.eat_keyword(Keyword::For).is_none() {
            self.error(
                "L0216",
                "expected `for` in `impl Trait for Type`",
                self.peek().span,
            );
            return None;
        }
        let type_name = self.expect_identifier("expected implementing type name after `for`")?;
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
            let method = self.parse_impl_method(&type_name)?;
            methods.push(method);
            self.skip_newlines();
        }
        self.expect(TokenKindRef::Dedent, "expected impl body dedent")?;
        Some(ImplDecl {
            trait_name,
            type_name,
            methods,
            span,
        })
    }

    /// Parse an impl method `method self [param Type ...] -> Ret` + indented body
    /// (the leading `fn` has been consumed). The `self` receiver is untyped in
    /// source; its type is the implementing `type_name`, injected as the first
    /// parameter so the rest of the pipeline sees an ordinary function.
    fn parse_impl_method(&mut self, type_name: &str) -> Option<Function> {
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
            ty: TypeRef::new(type_name),
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
            type_params: Vec::new(),
            params,
            return_type,
            body,
            span: fn_span,
            is_public: false,
        })
    }

    /// Parse `enum NAME` followed by an indented list of `Variant type...` lines.
    /// Each variant is a name plus zero or more positional, unnamed payload types.
    fn parse_enum(&mut self, is_public: bool) -> Option<EnumDecl> {
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
            is_public,
        })
    }

    /// Parse `struct NAME` followed by an indented list of `field type` lines.
    fn parse_struct(&mut self, is_public: bool) -> Option<StructDecl> {
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
        Some(StructDecl {
            name,
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

    fn parse_function(&mut self, is_public: bool) -> Option<Function> {
        let fn_span = self.previous().span;
        let name = self.expect_identifier("expected function name after `fn`")?;
        let type_params = self.parse_type_params(fn_span)?;
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
            type_params,
            params,
            return_type,
            body,
            span: fn_span,
            is_public,
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
                // Single-argument generics reuse one shape; `result` accepts a
                // comma-separated argument list. All emit the shared canonical
                // spelling so string-based `TypeRef` equality holds everywhere.
                let is_single = matches!(
                    name.as_str(),
                    "array" | "ptr" | "ref" | "rc" | "option" | "list"
                );
                let is_multi = matches!(name.as_str(), "result" | "map");
                if (is_single || is_multi) && self.eat_symbol("<") {
                    let mut args = vec![self.expect_type("expected generic type argument")?];
                    if is_multi {
                        while self.eat_symbol(",") {
                            args.push(self.expect_type("expected generic type argument")?);
                        }
                    }
                    if !self.eat_symbol(">") {
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
        Keyword::Package => "package",
        Keyword::Union => "union",
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
            TokenKind::Char(value) => Ok(Expr {
                kind: ExprKind::Char(value),
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
    fn parses_imports_and_pub_visibility() {
        let source = concat!(
            "import geometry\n",
            "import util\n\n",
            "pub struct Point\n    x i64\n    y i64\n\n",
            "pub fn dot a Point b Point -> i64\n    a.x * b.x\n\n",
            "fn helper n i64 -> i64\n    n\n",
        );
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.imports, vec!["geometry", "util"]);
        assert!(program.structs[0].is_public);
        assert!(program.functions[0].is_public);
        assert!(!program.functions[1].is_public);
    }

    #[test]
    fn parses_option_and_result_generic_types() {
        let source = concat!(
            "fn f a option<i64> b result<i64, string> c option<array<i64>> -> void\n",
            "    return\n",
        );
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let params = &program.functions[0].params;
        assert_eq!(params[0].ty.name, "option<i64>");
        assert_eq!(params[1].ty.name, "result<i64, string>");
        assert_eq!(params[2].ty.name, "option<array<i64>>");
    }

    #[test]
    fn generic_args_splits_nesting_aware() {
        assert_eq!(
            TypeRef::new("option<i64>").option_element(),
            Some(TypeRef::new("i64"))
        );
        assert_eq!(
            TypeRef::new("result<i64, string>").result_args(),
            Some((TypeRef::new("i64"), TypeRef::new("string")))
        );
        assert_eq!(
            TypeRef::new("result<array<i64>, string>").generic_args("result"),
            Some(vec![TypeRef::new("array<i64>"), TypeRef::new("string")])
        );
        assert_eq!(
            TypeRef::new("option<array<i64>>").option_element(),
            Some(TypeRef::new("array<i64>"))
        );
        // Canonical spelling round-trips through the shared formatter.
        assert_eq!(
            generic_type("result", &[TypeRef::new("i64"), TypeRef::new("string")]).name,
            "result<i64, string>"
        );
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
    fn parses_generic_type_parameters() {
        let source = "fn choose<T, U> pick bool a T b U -> T\n    a\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(
            program.functions[0].type_params,
            vec![TypeParam::new("T"), TypeParam::new("U")]
        );
        // A type-parameter name is spelled as an ordinary `TypeRef`.
        assert_eq!(program.functions[0].params[1].ty.name, "T");
        assert_eq!(program.functions[0].return_type.name, "T");
    }

    #[test]
    fn parses_trait_impl_and_bounded_type_param() {
        let source = concat!(
            "trait Show\n",
            "    fn show self -> string\n\n",
            "struct Point\n",
            "    x i64\n\n",
            "impl Show for Point\n",
            "    fn show self -> string\n",
            "        to_string(self.x)\n\n",
            "fn describe<T: Show> v T -> string\n",
            "    v.show()\n",
        );
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.traits.len(), 1);
        assert_eq!(program.traits[0].name, "Show");
        assert_eq!(program.traits[0].methods[0].name, "show");
        assert_eq!(program.impls.len(), 1);
        assert_eq!(program.impls[0].trait_name, "Show");
        assert_eq!(program.impls[0].type_name, "Point");
        // The impl method injects an untyped `self` with the implementing type.
        assert_eq!(program.impls[0].methods[0].params[0].name, "self");
        assert_eq!(program.impls[0].methods[0].params[0].ty.name, "Point");
        // The bounded type parameter records its trait bound.
        let describe = program
            .functions
            .iter()
            .find(|f| f.name == "describe")
            .expect("describe");
        assert_eq!(describe.type_params[0].name, "T");
        assert_eq!(describe.type_params[0].bounds, vec!["Show".to_string()]);
    }

    #[test]
    fn non_generic_function_has_empty_type_params() {
        let source = "fn add a i64 b i64 -> i64\n    a + b\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        assert!(program.functions[0].type_params.is_empty());
    }

    #[test]
    fn rejects_duplicate_type_parameter() {
        let tokens = lex("fn dup<T, T> a T -> T\n    a\n").expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse should fail");
        assert!(diagnostics.iter().any(|d| d.code == "L0394"));
    }

    #[test]
    fn rejects_type_parameter_that_shadows_builtin() {
        let tokens = lex("fn bad<i64> a i64 -> i64\n    a\n").expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse should fail");
        assert!(diagnostics.iter().any(|d| d.code == "L0394"));
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
        // `module` remains a planned keyword rejected with L0211; `import` and
        // `pub` are now accepted (see `parses_imports_and_pub_visibility`).
        let tokens = lex("module demo\nfn main -> i64\n    1\n").expect("lex");
        let diagnostics = parse(&tokens).expect_err("parse should fail");
        assert_eq!(diagnostics[0].code, "L0211");
        assert!(diagnostics[0].message.contains("module"));
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
