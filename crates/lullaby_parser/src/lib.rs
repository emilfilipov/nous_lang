use lullaby_lexer::{Diagnostic, Keyword, Span, Token, TokenKind, lex};
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
    /// True when the function is declared `async fn`. Calling an async function
    /// runs its body on a spawned OS thread and yields a `Future<T>` handle that
    /// `await` resolves to the `T`. Serde-defaulted to `false` so existing
    /// single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_async: bool,
    /// True when the function is declared `extern fn` — a body-less declaration of
    /// a C-ABI function imported at link time. An extern function has no `body`;
    /// calls to it are type-checked like ordinary calls, lower to a `call` of the
    /// external symbol on the native backend, and cannot run on the interpreters.
    /// Serde-defaulted to `false` so existing artifacts and AST snapshots stay
    /// valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_extern: bool,
    /// True when the function is declared `export fn` — a normal Lullaby function
    /// (with a body) that is additionally exposed under its plain C name as an
    /// externally visible, defined native symbol so C (or another object) can call
    /// into it. `export` is meaningful only to native codegen: on the
    /// interpreters an `export fn` runs exactly like an ordinary `fn`.
    /// Serde-defaulted to `false` so existing artifacts and AST snapshots stay
    /// valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_export: bool,
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

    /// The element type `T` of a growable `list<T>` spelling, if any.
    pub fn list_element(&self) -> Option<TypeRef> {
        self.generic_arg("list")
    }

    /// The `(K, V)` key/value type pair of a `map<K, V>` spelling, if any.
    pub fn map_args(&self) -> Option<(TypeRef, TypeRef)> {
        self.generic_args("map")
            .filter(|args| args.len() == 2)
            .map(|mut args| {
                let value = args.remove(1);
                let key = args.remove(0);
                (key, value)
            })
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
/// Sentinel `TypeRef` name for a function whose return type was omitted
/// (`fn f params` with no `-> T`) and is to be inferred from its body during
/// semantic analysis. It uses characters no real type name can contain, so it
/// never collides with a user type; the formatter renders such a function
/// without a `->` clause, and semantics replaces it with the inferred type.
pub const INFERRED_RETURN: &str = "<infer>";

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
    /// `for x in collection` — iterate `x` over each element of an `array<T>`,
    /// `list<T>`, or the characters of a `string`. Lowered by desugaring to an
    /// index-based `for` in the IR; the AST interpreter iterates directly.
    ForEach {
        name: String,
        iterable: Expr,
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
    /// Inline assembly: raw x86-64 machine-code bytes emitted verbatim into the
    /// current function's `.text` at this point. Native-only and inherently
    /// `unsafe`. Each byte is an `i64` value in `0..=255`; semantics validates
    /// the range and requires an enclosing `unsafe` block, and only the native
    /// backend can emit it (the interpreters reject it with `L0425`).
    Asm {
        bytes: Vec<i64>,
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
    Remainder,
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
    /// `await EXPR`: block until the awaited `Future<T>` completes and yield its
    /// `T`. The operand is any expression that evaluates to a `Future<T>`
    /// (typically a call to an `async fn`).
    Await {
        expr: Box<Expr>,
    },
    /// Postfix error-propagation `EXPR?`. On a `result<T, E>` operand it yields
    /// `T` for `ok(t)` and immediately returns `err(e)` from the enclosing
    /// function otherwise; on an `option<T>` operand it yields `T` for `some(t)`
    /// and returns `none` otherwise. This node exists only in the parser AST: the
    /// AST interpreter realizes it directly via a function-level early-return
    /// signal, and the IR lowerer desugars it into `let`/`match`/`return` so no
    /// IR node (and thus no native/WASM backend change) is needed.
    Try(Box<Expr>),
    /// An inline closure literal `fn PARAMS -> EXPR`: an anonymous function value
    /// that captures the enclosing scope's locals by value at evaluation time.
    /// `params` are `name type` pairs (explicit types, no parens, like a
    /// top-level `fn`), and `body` is a single expression. `id` is a parse-order
    /// identifier assigned by the parser; each backend keys its own
    /// `id -> (param_names, body)` closure-body table on it, so the runtime value
    /// carries only the id plus its captured snapshot and stores no body node.
    Closure {
        id: usize,
        params: Vec<Param>,
        body: Box<Expr>,
    },
    /// Inline conditional (ternary) expression `THEN if COND else ELSE`. `cond`
    /// must be `bool`; `then_branch` and `else_branch` must share a type, which
    /// becomes the expression's type. It is the lowest-precedence expression
    /// form and right-associative, so `a + b if c else d` parses as
    /// `(a + b) if c else d` and `x if a else y if b else z` as
    /// `x if a else (y if b else z)`. The AST interpreter evaluates it directly;
    /// the IR lowerer desugars it into a hoisted temporary plus an `if`
    /// statement, so the IR interpreter, bytecode VM, native, and WASM backends
    /// need no dedicated conditional-expression node.
    Conditional {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// Membership test `VALUE in COLLECTION`, yielding `bool`. The collection is
    /// a `string` (with a `char` or `string` value — character/substring
    /// containment) or a `list<T>` (with a `T` value). It sits at comparison
    /// precedence, so `x in xs and y in ys` is `(x in xs) and (y in ys)`. The
    /// AST interpreter evaluates it directly; the IR lowerer desugars it to a
    /// `contains`/`list_contains` builtin call, so no backend needs an `in` node.
    In {
        value: Box<Expr>,
        collection: Box<Expr>,
    },
    /// String slice `target[start:end]`, yielding a substring over the half-open
    /// char range `[start, end)`. Either bound may be omitted: `s[:end]` starts
    /// at `0`, `s[start:]` runs to `len(s)`, and `s[:]` copies the whole string.
    /// The target must be a `string`. The AST interpreter evaluates it directly;
    /// the IR lowerer desugars it to a `substring(target, start, end)` builtin
    /// call, so no backend needs a slice node.
    Slice {
        target: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOp {
    Not,
    /// Bitwise NOT (`~`, one's complement) on an `i64`.
    BitNot,
    /// Arithmetic negation (unary `-`) on any numeric type (`i64`, `f64`, `f32`,
    /// or a fixed-width integer), preserving the operand's type. Integer
    /// negation wraps (`-i64::MIN == i64::MIN`); float negation flips the sign
    /// bit (so `-0.0` and negated NaN behave like IEEE-754).
    Negate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    /// Integer remainder (`%`): truncated remainder (sign of the dividend, like
    /// C and Rust) of two operands of the same integer type. Not defined on
    /// floats.
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
    /// Bitwise AND (`&`) on two `i64`s.
    BitAnd,
    /// Bitwise OR (`|`) on two `i64`s.
    BitOr,
    /// Bitwise XOR (`^`) on two `i64`s.
    BitXor,
    /// Left shift (`<<`) of an `i64` by an `i64` amount (masked to 6 bits).
    Shl,
    /// Arithmetic (sign-preserving) right shift (`>>`) of an `i64` by an `i64`
    /// amount (masked to 6 bits).
    Shr,
}

/// Validate and remove `_` digit separators from a numeric literal. A separator
/// is only valid between two ASCII digits (`1_000`, `3.141_592`), so a leading,
/// trailing, doubled, or `.`-adjacent underscore is rejected. Returns the
/// separator-free text, or `None` when a separator is misplaced.
fn normalize_number_literal(value: &str) -> Option<String> {
    if !value.contains('_') {
        return Some(value.to_string());
    }
    let bytes = value.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'_' {
            let prev_digit = index
                .checked_sub(1)
                .is_some_and(|prev| bytes[prev].is_ascii_digit());
            let next_digit = bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
            if !(prev_digit && next_digit) {
                return None;
            }
        }
    }
    Some(value.chars().filter(|ch| *ch != '_').collect())
}

/// Parse a base-prefixed integer literal (`0x`/`0X`, `0b`/`0B`, `0o`/`0O`) into
/// an `i64`. The prefix is matched case-insensitively; the remaining text must be
/// non-empty radix digits with optional `_` separators strictly between two valid
/// radix digits (a leading, trailing, doubled, or prefix-adjacent underscore is
/// rejected). An out-of-radix digit, empty digits, a `.`, or an `i64` overflow all
/// return `None`. A decimal literal (no recognized base prefix) also returns
/// `None` so the caller falls through to the existing decimal/float path.
fn parse_radix_literal(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'0' {
        return None;
    }
    let radix = match bytes[1] {
        b'x' | b'X' => 16,
        b'b' | b'B' => 2,
        b'o' | b'O' => 8,
        _ => return None,
    };
    let digits = &value[2..];
    if digits.is_empty() {
        return None;
    }
    let digit_bytes = digits.as_bytes();
    let mut cleaned = String::with_capacity(digits.len());
    for (index, &byte) in digit_bytes.iter().enumerate() {
        if byte == b'_' {
            let prev_ok = index
                .checked_sub(1)
                .is_some_and(|prev| (digit_bytes[prev] as char).is_digit(radix));
            let next_ok = digit_bytes
                .get(index + 1)
                .is_some_and(|next| (*next as char).is_digit(radix));
            if !(prev_ok && next_ok) {
                return None;
            }
            continue;
        }
        if !(byte as char).is_digit(radix) {
            return None;
        }
        cleaned.push(byte as char);
    }
    i64::from_str_radix(&cleaned, radix).ok()
}

/// Recognized typed numeric-literal suffixes, longest first so `usize`/`isize`
/// are matched before any shorter candidate. `i64`/`f64` are the defaults; the
/// rest desugar to the corresponding `to_<T>` conversion builtin.
const NUMBER_SUFFIXES: &[&str] = &[
    "usize", "isize", "i16", "i32", "i64", "u16", "u32", "u64", "f32", "f64", "i8", "u8",
];

/// True when `s` carries a `0x`/`0b`/`0o` base prefix.
fn is_radix_prefixed(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2
        && bytes[0] == b'0'
        && matches!(bytes[1], b'x' | b'X' | b'b' | b'B' | b'o' | b'O')
}

/// The inclusive `[min, max]` range of an integer suffix as `i128`. Returns
/// `None` for the float suffixes. (The `u64`/`usize` range is the full type
/// range; a separate writable-literal cap at `i64::MAX` applies when packing the
/// desugared cell.)
fn int_suffix_range(suffix: &str) -> Option<(i128, i128)> {
    Some(match suffix {
        "i8" => (i128::from(i8::MIN), i128::from(i8::MAX)),
        "u8" => (0, i128::from(u8::MAX)),
        "i16" => (i128::from(i16::MIN), i128::from(i16::MAX)),
        "i32" => (i128::from(i32::MIN), i128::from(i32::MAX)),
        "i64" | "isize" => (i128::from(i64::MIN), i128::from(i64::MAX)),
        "u16" => (0, i128::from(u16::MAX)),
        "u32" => (0, i128::from(u32::MAX)),
        "u64" | "usize" => (0, i128::from(u64::MAX)),
        _ => return None,
    })
}

/// Parse a (possibly base-prefixed) integer literal body into `i128`. Radix
/// bodies reuse [`parse_radix_literal`] (values up to `i64::MAX`); decimal bodies
/// parse the full `i128` range so large `u64`/`usize` literals validate exactly.
fn literal_base_to_i128(base: &str) -> Option<i128> {
    if is_radix_prefixed(base) {
        parse_radix_literal(base).map(i128::from)
    } else {
        normalize_number_literal(base)?.parse::<i128>().ok()
    }
}

/// Build a `to_<name>(literal)` call expression, the desugaring of a typed
/// numeric-literal suffix; the synthetic literal argument carries the same span.
fn conversion_call(name: &str, argument: ExprKind, span: Span) -> ExprKind {
    ExprKind::Call {
        name: name.to_string(),
        args: vec![Expr {
            kind: argument,
            span,
        }],
    }
}

/// Turn a `Number` token's text into an expression. A recognized type suffix
/// (`0u8`… → err, `1i32`, `2.0f32`, `0xFFu16`, …) is range-checked and desugared
/// to the matching `to_<T>` conversion; `i64`/`f64` suffixes and unsuffixed
/// literals produce a plain `Integer`/`Float`. Unsigned 64-bit literals above
/// `i64::MAX` are supported (their `i64` bit pattern is passed to `to_u64`).
fn parse_number_literal(value: &str, span: Span) -> Result<ExprKind, String> {
    for &suffix in NUMBER_SUFFIXES {
        let Some(base) = value.strip_suffix(suffix) else {
            continue;
        };
        if base.is_empty() {
            continue;
        }
        let is_float_suffix = suffix.starts_with('f');
        // A float suffix never applies to a base-prefixed literal — `0xABF32` is
        // the hex number 0xABF32, not `0xAB` with an `f32` suffix.
        if is_float_suffix && is_radix_prefixed(base) {
            continue;
        }
        if is_float_suffix {
            let normalized = normalize_number_literal(base)
                .ok_or_else(|| format!("invalid float literal `{value}`"))?;
            let parsed = normalized
                .parse::<f64>()
                .map_err(|_| format!("invalid float literal `{value}`"))?;
            if suffix == "f64" {
                return Ok(ExprKind::Float(parsed));
            }
            return Ok(conversion_call("to_f32", ExprKind::Float(parsed), span));
        }
        // Integer suffix.
        if base.contains('.') {
            return Err(format!("integer literal `{value}` must not contain `.`"));
        }
        let (min, max) = int_suffix_range(suffix).expect("integer suffix has a range");
        let magnitude = literal_base_to_i128(base)
            .ok_or_else(|| format!("invalid integer literal `{value}`"))?;
        if magnitude < min || magnitude > max {
            return Err(format!(
                "integer literal `{value}` is out of range for `{suffix}`"
            ));
        }
        if suffix == "i64" {
            return Ok(ExprKind::Integer(magnitude as i64));
        }
        // The literal is desugared to `to_<T>(<i64>)`, so its magnitude must be
        // expressible as a non-negative `i64` literal. Every fixed-width value is
        // — except a `u64`/`usize` above `i64::MAX`, whose cell would be a
        // negative `i64` that has no round-trippable literal form. Reject those
        // with a precise pointer to the conversion builtin (the value is valid
        // for the type, just not writable as a literal).
        if magnitude > i128::from(i64::MAX) {
            return Err(format!(
                "`{suffix}` literal `{value}` exceeds the writable maximum {}; \
                 build larger `{suffix}` values with `to_{suffix}`",
                i64::MAX
            ));
        }
        return Ok(conversion_call(
            &format!("to_{suffix}"),
            ExprKind::Integer(magnitude as i64),
            span,
        ));
    }
    // No recognized suffix: base-prefixed integer, else decimal integer or float.
    if is_radix_prefixed(value) {
        let parsed = parse_radix_literal(value)
            .ok_or_else(|| format!("invalid integer literal `{value}`"))?;
        return Ok(ExprKind::Integer(parsed));
    }
    let normalized = normalize_number_literal(value)
        .ok_or_else(|| format!("invalid numeric literal `{value}`"))?;
    if normalized.contains('.') {
        let parsed = normalized
            .parse::<f64>()
            .map_err(|_| format!("invalid float literal `{value}`"))?;
        Ok(ExprKind::Float(parsed))
    } else {
        let parsed = normalized
            .parse::<i64>()
            .map_err(|_| format!("invalid integer literal `{value}`"))?;
        Ok(ExprKind::Integer(parsed))
    }
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
                if (is_single || is_multi) && self.eat_symbol("<") {
                    let mut args = vec![self.expect_type("expected generic type argument")?];
                    if is_multi {
                        while self.eat_symbol(",") {
                            args.push(self.expect_type("expected generic type argument")?);
                        }
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

struct ExprParser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    /// Next closure `id` to assign, seeded from the owning [`Parser`] so ids are
    /// unique and monotonic across every expression in the program.
    closure_counter: usize,
    /// `>` closers still owed from a split `>>` token, mirroring the field on the
    /// declaration [`Parser`] so nested generics in closure parameter types
    /// (`fn xs list<array<i64>> -> ...`) close correctly.
    pending_generic_close: usize,
}

impl<'a> ExprParser<'a> {
    fn new(tokens: &'a [Token], closure_counter: usize) -> Self {
        Self {
            tokens,
            cursor: 0,
            closure_counter,
            pending_generic_close: 0,
        }
    }

    fn parse(&mut self) -> Result<Expr, String> {
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
