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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOp {
    Not,
    /// Bitwise NOT (`~`, one's complement) on an `i64`.
    BitNot,
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

    fn parse_function(
        &mut self,
        is_public: bool,
        is_async: bool,
        is_export: bool,
    ) -> Option<Function> {
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
            TokenKind::String(value) => Ok(Expr {
                kind: ExprKind::String(value),
                span: token.span,
            }),
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
                let body = self.parse_binary(0)?;
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

    /// Extract the single expression that is the body of a one-line
    /// `fn main -> i64` (used to inspect parsed operator structure).
    fn body_expr(source: &str) -> Expr {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        match &program.functions[0].body[0] {
            Stmt::Expr(expr) => expr.clone(),
            other => panic!("expected an expression statement, got {other:?}"),
        }
    }

    fn as_binary(expr: &Expr) -> (BinaryOp, &Expr, &Expr) {
        match &expr.kind {
            ExprKind::Binary { left, op, right } => (*op, left, right),
            other => panic!("expected a binary expression, got {other:?}"),
        }
    }

    #[test]
    fn parses_bitwise_operators() {
        let expr = body_expr("fn main -> i64\n    5 & 3\n");
        let (op, _, _) = as_binary(&expr);
        assert_eq!(op, BinaryOp::BitAnd);

        let expr = body_expr("fn main -> i64\n    ~5\n");
        assert!(matches!(
            expr.kind,
            ExprKind::Unary {
                op: UnaryOp::BitNot,
                ..
            }
        ));
    }

    #[test]
    fn bitwise_precedence_matches_c_like_ordering() {
        // Shifts bind tighter than `&`, which binds tighter than `^`, which
        // binds tighter than `|`. `a | b ^ c & d << e` == `a | (b ^ (c & (d << e)))`.
        let expr = body_expr("fn main -> i64\n    1 | 2 ^ 3 & 4 << 5\n");
        let (top, _, or_right) = as_binary(&expr);
        assert_eq!(top, BinaryOp::BitOr, "top of tree is `|`");
        let (xor_op, _, xor_right) = as_binary(or_right);
        assert_eq!(xor_op, BinaryOp::BitXor, "right of `|` is `^`");
        let (and_op, _, and_right) = as_binary(xor_right);
        assert_eq!(and_op, BinaryOp::BitAnd, "right of `^` is `&`");
        let (shl_op, _, _) = as_binary(and_right);
        assert_eq!(shl_op, BinaryOp::Shl, "right of `&` is `<<`");
    }

    #[test]
    fn shift_binds_below_additive_and_bitwise_below_comparison() {
        // `a + b << c` == `(a + b) << c` (additive tighter than shift).
        let expr = body_expr("fn main -> i64\n    1 + 2 << 3\n");
        let (top, left, _) = as_binary(&expr);
        assert_eq!(top, BinaryOp::Shl);
        assert_eq!(as_binary(left).0, BinaryOp::Add);

        // `a & b == c` == `(a & b) == c` (bitwise tighter than comparison).
        let expr = body_expr("fn main -> i64\n    1 & 2 == 3\n");
        let (top, left, _) = as_binary(&expr);
        assert_eq!(top, BinaryOp::Equal);
        assert_eq!(as_binary(left).0, BinaryOp::BitAnd);

        // Unary `~` binds tighter than `&`: `~a & b` == `(~a) & b`.
        let expr = body_expr("fn main -> i64\n    ~1 & 2\n");
        let (top, left, _) = as_binary(&expr);
        assert_eq!(top, BinaryOp::BitAnd);
        assert!(matches!(
            left.kind,
            ExprKind::Unary {
                op: UnaryOp::BitNot,
                ..
            }
        ));
    }

    #[test]
    fn bitwise_operators_format_idempotently() {
        // The formatter must render the new operators and re-parse to the same
        // canonical text (idempotency), parenthesizing only where precedence
        // requires it.
        let source = "fn main -> i64\n    1 | 2 ^ 3 & 4 << 5 >> 6\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let once = format_program(&program);
        let reparsed = parse(&lex(&once).expect("lex")).expect("parse");
        let twice = format_program(&reparsed);
        assert_eq!(once, twice, "formatter is idempotent");
        assert!(
            once.contains("1 | 2 ^ 3 & 4 << 5 >> 6"),
            "no spurious parens for right-descending precedence chain: {once}"
        );

        // `~` renders with no space and parenthesizes a binary operand.
        let source = "fn main -> i64\n    ~5 & 3\n";
        let program = parse(&lex(source).expect("lex")).expect("parse");
        let out = format_program(&program);
        assert!(out.contains("~5 & 3"), "renders unary bitwise not: {out}");
    }

    #[test]
    fn parses_void_function() {
        let tokens = lex("fn main -> void\n    return\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        assert_eq!(program.functions[0].return_type.name, "void");
    }

    #[test]
    fn parses_extern_function_without_body() {
        let source = "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(-7)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let extern_fn = &program.functions[0];
        assert_eq!(extern_fn.name, "llabs");
        assert!(extern_fn.is_extern, "extern flag set");
        assert!(extern_fn.body.is_empty(), "extern declaration has no body");
        assert_eq!(extern_fn.params.len(), 1);
        assert_eq!(extern_fn.return_type.name, "i64");
        // The extern signature round-trips through the canonical formatter.
        let formatted = format_program(&program);
        assert!(
            formatted.contains("extern fn llabs x i64 -> i64"),
            "formatter renders extern signature: {formatted}"
        );
    }

    #[test]
    fn parses_export_function_with_body() {
        let source = "export fn add_seven x i64 -> i64\n    x + 7\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let export_fn = &program.functions[0];
        assert_eq!(export_fn.name, "add_seven");
        assert!(export_fn.is_export, "export flag set");
        assert!(!export_fn.is_extern, "export is not extern");
        assert!(!export_fn.body.is_empty(), "export declaration has a body");
        // The export marker round-trips through the canonical formatter.
        let formatted = format_program(&program);
        assert!(
            formatted.contains("export fn add_seven x i64 -> i64"),
            "formatter renders export signature: {formatted}"
        );
    }

    #[test]
    fn rejects_export_combined_with_extern() {
        let source = "export extern fn f x i64 -> i64\n";
        let tokens = lex(source).expect("lex");
        let diagnostics = parse(&tokens).expect_err("combining export and extern is rejected");
        assert!(
            diagnostics.iter().any(|d| d.code == "L0201"),
            "expected L0201: {diagnostics:?}"
        );
    }

    /// The single expression statement of `main`'s body (a one-statement fn).
    fn only_expr(source: &str) -> Expr {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        match &program.functions[0].body[0] {
            Stmt::Expr(expr) => expr.clone(),
            other => panic!("expected an expression statement, got {other:?}"),
        }
    }

    #[test]
    fn parses_postfix_try_on_call() {
        // `f()?` applies `?` to the call result.
        let expr = only_expr("fn main -> result<i64, string>\n    f()?\n");
        match expr.kind {
            ExprKind::Try(inner) => {
                assert!(
                    matches!(inner.kind, ExprKind::Call { .. }),
                    "operand is a call"
                );
            }
            other => panic!("expected Try, got {other:?}"),
        }
    }

    #[test]
    fn parses_chained_try() {
        // `x??` is a `Try` of a `Try` (left-to-right postfix application).
        let expr = only_expr("fn main -> option<i64>\n    x??\n");
        match expr.kind {
            ExprKind::Try(outer) => match outer.kind {
                ExprKind::Try(inner) => {
                    assert!(
                        matches!(inner.kind, ExprKind::Variable(_)),
                        "innermost is a variable"
                    );
                }
                other => panic!("expected nested Try, got {other:?}"),
            },
            other => panic!("expected Try, got {other:?}"),
        }
    }

    #[test]
    fn try_binds_tighter_than_binary() {
        // `a + b?` parses as `a + (b?)`, so the `?` applies only to `b`.
        let expr = only_expr("fn main -> result<i64, string>\n    a + b?\n");
        match expr.kind {
            ExprKind::Binary { right, op, .. } => {
                assert_eq!(op, BinaryOp::Add);
                assert!(
                    matches!(right.kind, ExprKind::Try(_)),
                    "right operand is `b?`"
                );
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn parses_try_inside_call_argument() {
        // `f(g()?)` places a `Try` in the argument position; nested `?` works.
        let expr = only_expr("fn main -> result<i64, string>\n    f(g()?)\n");
        match expr.kind {
            ExprKind::Call { name, args } => {
                assert_eq!(name, "f");
                assert_eq!(args.len(), 1);
                assert!(matches!(args[0].kind, ExprKind::Try(_)), "arg is `g()?`");
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn formats_try_operator_round_trip() {
        // The formatter renders `expr?` and is idempotent: a compound operand is
        // parenthesized, a call/variable operand is not, and `x??` stays `x??`.
        let source = concat!(
            "fn main -> result<i64, string>\n",
            "    let a i64 = f()?\n",
            "    let b i64 = g()??\n",
            "    ok(a + b)\n",
        );
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let once = format_program(&program);
        assert!(once.contains("f()?"), "renders call?: {once}");
        assert!(once.contains("g()??"), "renders chained ??: {once}");
        // Idempotent: re-parsing and re-formatting yields the same text.
        let tokens2 = lex(&once).expect("re-lex");
        let program2 = parse(&tokens2).expect("re-parse");
        assert_eq!(once, format_program(&program2), "formatter is idempotent");
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
    fn nested_generic_type_closes_across_shift_token() {
        // `option<array<i64>>` and deeper nesting lex the trailing `>>`/`>>>` as
        // shift tokens; the type parser must split them to close each generic.
        let source = concat!(
            "fn f a option<array<i64>> b option<option<option<i64>>> -> void\n",
            "    return\n",
        );
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let params = &program.functions[0].params;
        assert_eq!(params[0].ty.name, "option<array<i64>>");
        assert_eq!(params[1].ty.name, "option<option<option<i64>>>");
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
    fn typed_integer_suffix_desugars_to_conversion_call() {
        // `100i32` becomes `to_i32(100)`; the plain `i64`/`f64` suffixes stay
        // literals.
        let tokens = lex("fn main -> i32\n    100i32\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        let ExprKind::Call { name, args } = &expr.kind else {
            panic!("expected a conversion call, got {:?}", expr.kind);
        };
        assert_eq!(name, "to_i32");
        assert!(matches!(args[0].kind, ExprKind::Integer(100)));

        // A hex body with an unsigned suffix: `0xFFu16` -> `to_u16(255)`.
        let tokens = lex("fn main -> u16\n    0xFFu16\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        let ExprKind::Call { name, args } = &expr.kind else {
            panic!("expected a conversion call");
        };
        assert_eq!(name, "to_u16");
        assert!(matches!(args[0].kind, ExprKind::Integer(255)));

        // `42i64` stays a plain integer (i64 is the default width).
        let tokens = lex("fn main -> i64\n    42i64\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        assert!(matches!(expr.kind, ExprKind::Integer(42)));
    }

    #[test]
    fn typed_float_suffix_desugars_and_defaults_stay_literals() {
        // `2.5f32` -> `to_f32(2.5)`.
        let tokens = lex("fn main -> f32\n    2.5f32\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        let ExprKind::Call { name, args } = &expr.kind else {
            panic!("expected a conversion call");
        };
        assert_eq!(name, "to_f32");
        assert!(matches!(args[0].kind, ExprKind::Float(value) if (value - 2.5).abs() < 1e-9));

        // A hex body is never read as an `f32` suffix: `0xABF32` is a hex number.
        let tokens = lex("fn main -> i64\n    0xABF32\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        assert!(matches!(expr.kind, ExprKind::Integer(0xABF32)));
    }

    #[test]
    fn out_of_range_typed_literal_is_rejected() {
        // 256 does not fit i8; the parser rejects the literal.
        let tokens = lex("fn main -> i8\n    256i8\n").expect("lex");
        assert!(parse(&tokens).is_err(), "256i8 must be rejected");
        // A decimal point is invalid with an integer suffix.
        let tokens = lex("fn main -> i32\n    1.5i32\n").expect("lex");
        assert!(parse(&tokens).is_err(), "1.5i32 must be rejected");
        // A `u64` literal is writable up to i64::MAX; larger values must use
        // `to_u64` (their i64 cell would be negative, with no literal form).
        let tokens = lex("fn main -> u64\n    9223372036854775807u64\n").expect("lex");
        assert!(parse(&tokens).is_ok(), "i64::MAX as u64 must be accepted");
        let tokens = lex("fn main -> u64\n    9223372036854775808u64\n").expect("lex");
        assert!(
            parse(&tokens).is_err(),
            "a u64 literal above i64::MAX must be rejected"
        );
    }

    #[test]
    fn parses_digit_separators_in_integer_and_float_literals() {
        let tokens = lex("fn main -> i64\n    1_000_000\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        assert!(matches!(expr.kind, ExprKind::Integer(1_000_000)));

        let tokens = lex("fn main -> f64\n    1_234.567_8\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[0].body[0] else {
            panic!("expected expression statement");
        };
        assert!(matches!(expr.kind, ExprKind::Float(value) if (value - 1_234.567_8).abs() < 1e-9));
    }

    #[test]
    fn rejects_misplaced_digit_separators() {
        for bad in ["1__000", "1_000_", "3._14", "3_.14"] {
            let source = format!("fn main -> i64\n    {bad}\n");
            let tokens = lex(&source).expect("lex");
            assert!(
                parse(&tokens).is_err(),
                "expected `{bad}` to be rejected as a malformed literal"
            );
        }
    }

    #[test]
    fn parses_base_prefixed_integer_literals() {
        for (source, expected) in [
            ("0xFF", 255i64),
            ("0b1010", 10),
            ("0o17", 15),
            ("0xFF_FF", 65535),
            ("0b1010_0101", 165),
            ("0XdeadBEEF", 0xdead_beef),
        ] {
            let program = parse(&lex(&format!("fn main -> i64\n    {source}\n")).expect("lex"))
                .expect("parse");
            let Stmt::Expr(expr) = &program.functions[0].body[0] else {
                panic!("expected expression statement for `{source}`");
            };
            assert!(
                matches!(expr.kind, ExprKind::Integer(value) if value == expected),
                "expected `{source}` to parse as {expected}"
            );
        }
    }

    #[test]
    fn rejects_malformed_base_prefixed_literals() {
        for bad in ["0x", "0xG", "0b2", "0o8", "0x1.5", "0xF__F", "0x_F", "0xF_"] {
            let source = format!("fn main -> i64\n    {bad}\n");
            let tokens = lex(&source).expect("lex");
            assert!(
                parse(&tokens).is_err(),
                "expected `{bad}` to be rejected as a malformed literal"
            );
        }
    }

    #[test]
    fn parses_radix_literal_helper() {
        assert_eq!(parse_radix_literal("0xFF"), Some(255));
        assert_eq!(parse_radix_literal("0b1010"), Some(10));
        assert_eq!(parse_radix_literal("0o17"), Some(15));
        assert_eq!(parse_radix_literal("0xFF_FF"), Some(65535));
        assert_eq!(parse_radix_literal("0x"), None);
        assert_eq!(parse_radix_literal("0xG"), None);
        assert_eq!(parse_radix_literal("0b2"), None);
        assert_eq!(parse_radix_literal("0o8"), None);
        assert_eq!(parse_radix_literal("0xF__F"), None);
        // A plain decimal literal is not a base-prefixed literal.
        assert_eq!(parse_radix_literal("42"), None);
    }

    #[test]
    fn normalizes_number_literals() {
        assert_eq!(normalize_number_literal("42").as_deref(), Some("42"));
        assert_eq!(normalize_number_literal("1_000").as_deref(), Some("1000"));
        assert_eq!(
            normalize_number_literal("3.141_592").as_deref(),
            Some("3.141592")
        );
        assert_eq!(normalize_number_literal("_1").as_deref(), None);
        assert_eq!(normalize_number_literal("1_").as_deref(), None);
        assert_eq!(normalize_number_literal("1__0").as_deref(), None);
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
    fn parses_asm_statement() {
        let source = "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Unsafe { body, .. } = &program.functions[0].body[0] else {
            panic!("expected unsafe block");
        };
        let Stmt::Asm { bytes, .. } = &body[0] else {
            panic!("expected asm statement");
        };
        assert_eq!(bytes, &vec![72, 199, 192, 42, 0, 0, 0]);
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

    #[test]
    fn parses_closure_literal_in_let_value() {
        // `fn x i64 -> x + n` parses to a `Closure` node with one typed param and
        // an expression body; the top-level `fn main` declaration is unaffected.
        let source = "fn main -> i64\n    let n i64 = 10\n    let f fn(i64) -> i64 = fn x i64 -> x + n\n    f(5)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Let { value, .. } = &program.functions[0].body[1] else {
            panic!("expected the `let f` binding");
        };
        let ExprKind::Closure { id, params, body } = &value.kind else {
            panic!("expected a closure literal, got {:?}", value.kind);
        };
        assert_eq!(*id, 0, "the first closure literal gets id 0");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "x");
        assert_eq!(params[0].ty.name, "i64");
        assert!(matches!(body.kind, ExprKind::Binary { .. }));
    }

    #[test]
    fn parses_closure_literal_as_call_argument() {
        // A closure body stops at the argument-separating `,`, so
        // `apply(fn x i64 -> x + 1, 5)` parses as a two-argument call whose first
        // argument is a closure.
        let source = "fn apply f fn(i64) -> i64 v i64 -> i64\n    f(v)\n\nfn main -> i64\n    apply(fn x i64 -> x + 1, 5)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let Stmt::Expr(expr) = &program.functions[1].body[0] else {
            panic!("expected the `apply(...)` expression");
        };
        let ExprKind::Call { name, args } = &expr.kind else {
            panic!("expected a call, got {:?}", expr.kind);
        };
        assert_eq!(name, "apply");
        assert_eq!(args.len(), 2);
        assert!(matches!(args[0].kind, ExprKind::Closure { .. }));
        assert!(matches!(args[1].kind, ExprKind::Integer(5)));
    }

    #[test]
    fn closure_literals_get_distinct_monotonic_ids() {
        // Two closure literals in the same program get distinct, monotonic ids,
        // which key each backend's closure-body table.
        let source = "fn main -> i64\n    let f fn(i64) -> i64 = fn x i64 -> x + 1\n    let g fn(i64) -> i64 = fn y i64 -> y + 2\n    f(0) + g(0)\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let mut ids = Vec::new();
        for stmt in &program.functions[0].body {
            if let Stmt::Let { value, .. } = stmt
                && let ExprKind::Closure { id, .. } = &value.kind
            {
                ids.push(*id);
            }
        }
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn malformed_closure_missing_arrow_is_rejected() {
        // A closure literal without `->` is a parser diagnostic, not a panic.
        let source = "fn main -> i64\n    let f fn(i64) -> i64 = fn x i64 x + 1\n    f(0)\n";
        let tokens = lex(source).expect("lex");
        assert!(
            parse(&tokens).is_err(),
            "a closure missing `->` must be rejected"
        );
    }
}
