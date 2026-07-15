use lullaby_lexer::Span;
use serde::{Deserialize, Serialize};

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
    /// Named compile-time constants (`const NAME type = <expr>`), in source
    /// order. Each carries a constant-expression initializer that semantic
    /// analysis evaluates to a literal and folds into every reference, so the
    /// backends never see a `const`. Serde-defaulted so existing single-file
    /// artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consts: Vec<ConstDecl>,
}

/// A named compile-time constant declaration: `const NAME type = <expr>`. The
/// type annotation is mandatory (unlike an inferred `let`), and the initializer
/// must be a *constant expression* — literals plus arithmetic/logical/bitwise/
/// comparison operators over literals and other already-defined constants.
/// Semantic analysis evaluates it once at compile time and folds every
/// reference to the constant into the resulting literal, so no backend needs
/// any `const` awareness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstDecl {
    pub name: String,
    pub ty: TypeRef,
    pub value: Expr,
    pub span: Span,
    /// True when the declaration is exported with `pub`. Serde-defaulted to
    /// `false` so existing single-file artifacts and AST snapshots stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_public: bool,
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
pub(crate) fn expr_to_place(expr: &Expr) -> Option<(String, Vec<Place>)> {
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
