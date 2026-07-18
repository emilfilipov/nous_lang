//! Const-sized arrays `array<T, N>`: extent resolution, checking, and erasure.
//!
//! A const-sized array `array<T, N>` is a length-agnostic `array<T>` carrying a
//! compile-time extent `N`. `N` is a *constant expression* that folds to a
//! positive integer — either a literal (`array<u8, 512>`) or a named constant
//! (`const SIZE i64 = 512` then `array<u8, SIZE>`). This pass is the only place
//! the extent means anything; it runs right after constant folding and before
//! the type checker, and does three things:
//!
//! 1. **Resolve + validate.** Every `array<T, N>` spelling anywhere in the
//!    program (parameters, returns, `let` annotations, struct fields, enum
//!    payloads, nested generics, function types, closure parameters) has its
//!    extent resolved to a literal integer via the constant-folding results and
//!    validated: a non-constant extent is `L0463`, a zero or negative extent is
//!    `L0464`.
//! 2. **Check constructions.** A statically-counted array literal `[a, b, c]`
//!    or fill literal `[v; k]` used to initialize an `array<T, N>` slot must
//!    have exactly `N` elements (`L0465`). The assertion fires wherever the
//!    frontend can pair a construction with an extent-typed slot without type
//!    inference: `let` annotations, explicit `return`s and a function's trailing
//!    expression, struct-literal fields, and free-function call arguments.
//! 3. **Erase + expand.** Every extent is stripped (`array<T, N>` becomes
//!    `array<T>`) and every fill literal `[v; k]` is expanded to an ordinary
//!    `k`-element array literal, so the type checker and every backend
//!    (AST/IR/bytecode interpreters, native, WASM) only ever see the existing
//!    `array<T>` representation and need zero extent awareness. A fixed-extent
//!    array therefore compiles and runs exactly wherever the same-length
//!    `array<T>` already does.
//!
//! The extent is a construction-count assertion, not a distinct value type:
//! `array<T, N>` decays to `array<T>` for every typing purpose (they share the
//! element type and the erased runtime representation), so a fixed-extent array
//! interoperates freely with the length-agnostic surface (`len`, `a[i]`,
//! `for x in a`, passing to an `array<T>` helper).

use std::collections::HashMap;

use lullaby_diagnostics::Span;
use lullaby_parser::{
    Expr, ExprKind, Function, Program, Stmt, TypeRef, function_type, generic_type,
};

use super::SemanticDiagnostic;

/// The const-sized-array survival record for the native backend: per struct name,
/// the list of `(field, un-erased `array<T, N>` type)` pairs for every struct field
/// declared with an extent. This is the ONLY place a struct field's extent survives
/// erasure (a field has no initializer for the native backend to infer a length
/// from). Captured after resolution but before erasure, so each recorded type
/// carries its literal extent(s) at every nesting depth.
pub(crate) type FieldExtents = HashMap<String, Vec<(String, TypeRef)>>;

/// Resolve, validate, check, erase, and expand every const-sized array in the
/// program. `int_consts` maps each integer-valued named constant to its folded
/// value, used to resolve a named extent `array<T, SIZE>`. Mutates `program`
/// in place so that after this pass no extent and no fill literal remains, and
/// returns any extent diagnostics together with the [`FieldExtents`] survival
/// record (the un-erased struct-field array types the native backend lays out
/// inline).
pub(crate) fn resolve_check_and_erase(
    program: &mut Program,
    int_consts: &HashMap<String, i64>,
) -> (Vec<SemanticDiagnostic>, FieldExtents) {
    let mut diagnostics = Vec::new();
    // 1. Resolve named extents to literals and validate them, in place.
    walk_program_types(program, Mode::Resolve(int_consts), &mut diagnostics);
    // 2. Check construction lengths against the now-resolved literal extents,
    //    and expand fill literals to element lists.
    check_and_expand_program(program, &mut diagnostics);
    // 3. Capture the un-erased struct-field array types BEFORE erasure — the only
    //    channel through which a fixed-extent struct field reaches the native
    //    backend (see `IrStructDef::field_extents`).
    let field_extents = capture_field_extents(program);
    // 4. Erase every remaining extent so downstream stages see plain `array<T>`.
    walk_program_types(program, Mode::Erase, &mut diagnostics);
    (diagnostics, field_extents)
}

/// Record, per struct, the un-erased (extent-carrying) type of every field that
/// bears an extent at any nesting depth. Called between resolution and erasure, so
/// a recorded type still spells its literal extents (`array<u32, 1024>`,
/// `array<array<i32, 4>, 4>`). A struct with no extent-bearing field contributes no
/// entry, so the map is empty for a program that uses no fixed-extent struct fields.
fn capture_field_extents(program: &Program) -> FieldExtents {
    let mut extents = FieldExtents::new();
    for decl in &program.structs {
        let recorded: Vec<(String, TypeRef)> = decl
            .fields
            .iter()
            .filter(|field| type_has_extent(&field.ty))
            .map(|field| (field.name.clone(), field.ty.clone()))
            .collect();
        if !recorded.is_empty() {
            extents.insert(decl.name.clone(), recorded);
        }
    }
    extents
}

/// Whether a type spelling carries an `array<T, N>` extent at any nesting depth —
/// as a top-level fixed array, or nested inside another generic's type arguments or
/// a function type. Used to select which struct fields the survival record captures.
fn type_has_extent(ty: &TypeRef) -> bool {
    if let Some((params, ret)) = ty.function_signature() {
        return params.iter().any(type_has_extent) || type_has_extent(&ret);
    }
    match split_head_args(&ty.name) {
        Some((head, args)) => {
            if head == "array" && args.len() == 2 {
                return true;
            }
            args.iter().any(type_has_extent)
        }
        None => false,
    }
}

/// Whether the type walk is resolving/validating extents (with the constant
/// table) or erasing them.
enum Mode<'a> {
    Resolve(&'a HashMap<String, i64>),
    Erase,
}

// ---------------------------------------------------------------------------
// Type-string rewriting
// ---------------------------------------------------------------------------

/// Rewrite one type spelling. In `Resolve` mode, a named extent is folded to its
/// literal value and validated (emitting `L0463`/`L0464`); a valid extent is
/// kept as `array<T, N>` with the literal `N`, an invalid one is erased to
/// `array<T>` so the error does not cascade. In `Erase` mode every extent is
/// stripped. The rewrite recurses through every nested type argument, function
/// type, and array element, so an extent at any depth is handled.
fn rewrite_type(
    ty: &TypeRef,
    mode: &Mode,
    diagnostics: &mut Vec<SemanticDiagnostic>,
    span: Span,
) -> TypeRef {
    // Function type `fn(P, ...) -> R`: rewrite each parameter and the return.
    if let Some((params, ret)) = ty.function_signature() {
        let params: Vec<TypeRef> = params
            .iter()
            .map(|p| rewrite_type(p, mode, diagnostics, span))
            .collect();
        let ret = rewrite_type(&ret, mode, diagnostics, span);
        return function_type(&params, &ret);
    }

    let name = &ty.name;
    let (head, args) = match split_head_args(name) {
        Some(parts) => parts,
        // A plain scalar/name (no `<...>`) has no extent and no nested type.
        None => return ty.clone(),
    };

    if head == "array" && !args.is_empty() {
        let element = rewrite_type(&args[0], mode, diagnostics, span);
        if args.len() == 2 {
            match mode {
                Mode::Resolve(int_consts) => {
                    match resolve_extent(&args[1].name, int_consts) {
                        Ok(n) => return TypeRef::new(format!("array<{}, {}>", element.name, n)),
                        Err(error) => {
                            diagnostics.push(error.into_diagnostic(&args[1].name, span));
                            // Erase the bad extent so the checker sees `array<T>`
                            // and the error does not cascade into a type mismatch.
                            return generic_type("array", std::slice::from_ref(&element));
                        }
                    }
                }
                Mode::Erase => return generic_type("array", std::slice::from_ref(&element)),
            }
        }
        // `array<T>` (no extent) or a malformed arg count: keep the element only.
        return generic_type("array", std::slice::from_ref(&element));
    }

    // Any other generic constructor: rewrite every argument, preserving the head.
    let rewritten: Vec<TypeRef> = args
        .iter()
        .map(|arg| rewrite_type(arg, mode, diagnostics, span))
        .collect();
    generic_type(&head, &rewritten)
}

/// Split a `ctor<arg, arg, ...>` spelling into its head constructor name and its
/// top-level, nesting-aware type arguments. Returns `None` for a plain name or a
/// function type (which the caller handles separately).
fn split_head_args(name: &str) -> Option<(String, Vec<TypeRef>)> {
    if name.starts_with("fn(") {
        return None;
    }
    let open = name.find('<')?;
    if !name.ends_with('>') {
        return None;
    }
    let head = name[..open].to_string();
    let args = TypeRef::new(name).generic_args(&head)?;
    Some((head, args))
}

/// The reason an extent operand is not a usable positive integer.
enum ExtentError {
    /// The extent is neither a literal nor a known integer constant.
    NonConstant,
    /// The extent resolved to a zero or negative value.
    NonPositive(i64),
}

impl ExtentError {
    fn into_diagnostic(self, text: &str, span: Span) -> SemanticDiagnostic {
        match self {
            ExtentError::NonConstant => SemanticDiagnostic::at(
                "L0463",
                format!(
                    "array extent `{text}` is not a constant; an `array<T, N>` extent must be an \
                     integer literal or a named integer constant"
                ),
                None,
                span,
            ),
            ExtentError::NonPositive(value) => SemanticDiagnostic::at(
                "L0464",
                format!("array extent must be a positive integer, but `{text}` is {value}"),
                None,
                span,
            ),
        }
    }
}

/// Resolve an extent operand (the raw text of the second `array<...>` argument)
/// to a positive integer. A literal was normalized to decimal by the parser; a
/// bare identifier is looked up in the integer-constant table.
fn resolve_extent(text: &str, int_consts: &HashMap<String, i64>) -> Result<i64, ExtentError> {
    let text = text.trim();
    let value = if let Ok(literal) = text.parse::<i64>() {
        literal
    } else if let Some(&constant) = int_consts.get(text) {
        constant
    } else {
        return Err(ExtentError::NonConstant);
    };
    if value <= 0 {
        return Err(ExtentError::NonPositive(value));
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Program type walk (applies `rewrite_type` to every type position)
// ---------------------------------------------------------------------------

fn walk_program_types(
    program: &mut Program,
    mode: Mode,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    for function in &mut program.functions {
        walk_function_types(function, &mode, diagnostics);
    }
    for decl in &mut program.structs {
        for field in &mut decl.fields {
            field.ty = rewrite_type(&field.ty, &mode, diagnostics, decl.span);
        }
    }
    for decl in &mut program.enums {
        for variant in &mut decl.variants {
            for payload in &mut variant.payload {
                *payload = rewrite_type(payload, &mode, diagnostics, decl.span);
            }
        }
    }
    for decl in &mut program.traits {
        for method in &mut decl.methods {
            for param in &mut method.params {
                param.ty = rewrite_type(&param.ty, &mode, diagnostics, method.span);
            }
            method.return_type = rewrite_type(&method.return_type, &mode, diagnostics, method.span);
        }
    }
    for decl in &mut program.impls {
        for method in &mut decl.methods {
            walk_function_types(method, &mode, diagnostics);
        }
    }
    for decl in &mut program.consts {
        decl.ty = rewrite_type(&decl.ty, &mode, diagnostics, decl.span);
    }
    for decl in &mut program.actors {
        for field in &mut decl.state {
            field.ty = rewrite_type(&field.ty, &mode, diagnostics, decl.span);
        }
        if let Some(init) = &mut decl.init {
            for param in &mut init.params {
                param.ty = rewrite_type(&param.ty, &mode, diagnostics, init.span);
            }
            for stmt in &mut init.body {
                walk_stmt_types(stmt, &mode, diagnostics, init.span);
            }
        }
        for handler in &mut decl.handlers {
            for param in &mut handler.params {
                param.ty = rewrite_type(&param.ty, &mode, diagnostics, handler.span);
            }
            if let Some(reply) = &mut handler.reply_type {
                *reply = rewrite_type(reply, &mode, diagnostics, handler.span);
            }
            for stmt in &mut handler.body {
                walk_stmt_types(stmt, &mode, diagnostics, handler.span);
            }
        }
    }
    for decl in &mut program.aliases {
        decl.target = rewrite_type(&decl.target, &mode, diagnostics, decl.span);
    }
}

fn walk_function_types(
    function: &mut Function,
    mode: &Mode,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    for param in &mut function.params {
        param.ty = rewrite_type(&param.ty, mode, diagnostics, function.span);
    }
    function.return_type = rewrite_type(&function.return_type, mode, diagnostics, function.span);
    for stmt in &mut function.body {
        walk_stmt_types(stmt, mode, diagnostics, function.span);
    }
}

/// Rewrite every type position inside a statement: a `let` annotation, and any
/// closure parameter type reachable through the statement's expressions.
fn walk_stmt_types(
    stmt: &mut Stmt,
    mode: &Mode,
    diagnostics: &mut Vec<SemanticDiagnostic>,
    span: Span,
) {
    match stmt {
        Stmt::Let { ty, value, .. } => {
            if let Some(ty) = ty {
                *ty = rewrite_type(ty, mode, diagnostics, span);
            }
            walk_expr_types(value, mode, diagnostics, span);
        }
        Stmt::Assign { value, .. } | Stmt::Throw { value, .. } => {
            walk_expr_types(value, mode, diagnostics, span)
        }
        Stmt::Return(expr) => {
            if let Some(expr) = expr {
                walk_expr_types(expr, mode, diagnostics, span);
            }
        }
        Stmt::Expr(expr) => walk_expr_types(expr, mode, diagnostics, span),
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            for branch in branches {
                walk_expr_types(&mut branch.condition, mode, diagnostics, span);
                walk_block_types(&mut branch.body, mode, diagnostics, span);
            }
            walk_block_types(else_body, mode, diagnostics, span);
        }
        Stmt::While {
            condition, body, ..
        } => {
            walk_expr_types(condition, mode, diagnostics, span);
            walk_block_types(body, mode, diagnostics, span);
        }
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            walk_expr_types(start, mode, diagnostics, span);
            walk_expr_types(end, mode, diagnostics, span);
            if let Some(step) = step {
                walk_expr_types(step, mode, diagnostics, span);
            }
            walk_block_types(body, mode, diagnostics, span);
        }
        Stmt::ForEach { iterable, body, .. } => {
            walk_expr_types(iterable, mode, diagnostics, span);
            walk_block_types(body, mode, diagnostics, span);
        }
        Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } | Stmt::RegionBlock { body, .. } => {
            walk_block_types(body, mode, diagnostics, span)
        }
        Stmt::Try {
            body, catch_body, ..
        } => {
            walk_block_types(body, mode, diagnostics, span);
            walk_block_types(catch_body, mode, diagnostics, span);
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm { .. } | Stmt::Region(_) => {}
    }
}

fn walk_block_types(
    body: &mut [Stmt],
    mode: &Mode,
    diagnostics: &mut Vec<SemanticDiagnostic>,
    span: Span,
) {
    for stmt in body {
        walk_stmt_types(stmt, mode, diagnostics, span);
    }
}

/// Rewrite closure parameter types reachable through an expression. Closures are
/// the only expressions that carry a type annotation, so every other kind simply
/// recurses into its sub-expressions.
fn walk_expr_types(
    expr: &mut Expr,
    mode: &Mode,
    diagnostics: &mut Vec<SemanticDiagnostic>,
    span: Span,
) {
    match &mut expr.kind {
        ExprKind::Closure { params, body, .. } => {
            for param in params {
                param.ty = rewrite_type(&param.ty, mode, diagnostics, span);
            }
            walk_expr_types(body, mode, diagnostics, span);
        }
        ExprKind::Array(items) => {
            for item in items {
                walk_expr_types(item, mode, diagnostics, span);
            }
        }
        ExprKind::ArrayFill { value, count } => {
            walk_expr_types(value, mode, diagnostics, span);
            walk_expr_types(count, mode, diagnostics, span);
        }
        ExprKind::Index { target, index } => {
            walk_expr_types(target, mode, diagnostics, span);
            walk_expr_types(index, mode, diagnostics, span);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Await { expr } | ExprKind::Try(expr) => {
            walk_expr_types(expr, mode, diagnostics, span)
        }
        ExprKind::Binary { left, right, .. }
        | ExprKind::In {
            value: left,
            collection: right,
        } => {
            walk_expr_types(left, mode, diagnostics, span);
            walk_expr_types(right, mode, diagnostics, span);
        }
        ExprKind::Call { args, .. } | ExprKind::Spawn { args, .. } => {
            for arg in args {
                walk_expr_types(arg, mode, diagnostics, span);
            }
        }
        ExprKind::Tell { target, args, .. } => {
            walk_expr_types(target, mode, diagnostics, span);
            for arg in args {
                walk_expr_types(arg, mode, diagnostics, span);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                walk_expr_types(value, mode, diagnostics, span);
            }
        }
        ExprKind::Field { target, .. } => walk_expr_types(target, mode, diagnostics, span),
        ExprKind::Match { scrutinee, arms } => {
            walk_expr_types(scrutinee, mode, diagnostics, span);
            for arm in arms {
                walk_block_types(&mut arm.body, mode, diagnostics, span);
            }
        }
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr_types(cond, mode, diagnostics, span);
            walk_expr_types(then_branch, mode, diagnostics, span);
            walk_expr_types(else_branch, mode, diagnostics, span);
        }
        ExprKind::Slice { target, start, end } => {
            walk_expr_types(target, mode, diagnostics, span);
            if let Some(start) = start {
                walk_expr_types(start, mode, diagnostics, span);
            }
            if let Some(end) = end {
                walk_expr_types(end, mode, diagnostics, span);
            }
        }
        ExprKind::Combinator { operand, .. } => walk_expr_types(operand, mode, diagnostics, span),
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Char(_)
        | ExprKind::Variable(_) => {}
    }
}

// ---------------------------------------------------------------------------
// Construction-length checking and fill expansion
// ---------------------------------------------------------------------------

/// Declared signatures needed to propagate an expected extent to a construction:
/// free-function parameter types and struct field types (both read directly from
/// the AST — no type inference).
struct Sigs {
    functions: HashMap<String, Vec<TypeRef>>,
    structs: HashMap<String, Vec<(String, TypeRef)>>,
}

fn check_and_expand_program(program: &mut Program, diagnostics: &mut Vec<SemanticDiagnostic>) {
    let mut sigs = Sigs {
        functions: HashMap::new(),
        structs: HashMap::new(),
    };
    for function in &program.functions {
        sigs.functions.insert(
            function.name.clone(),
            function.params.iter().map(|p| p.ty.clone()).collect(),
        );
    }
    for decl in &program.structs {
        sigs.structs.insert(
            decl.name.clone(),
            decl.fields
                .iter()
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect(),
        );
    }

    for function in &mut program.functions {
        let owner = function.name.clone();
        let ret = function.return_type.clone();
        check_and_expand_body(&mut function.body, &ret, &owner, &sigs, diagnostics);
    }
    for decl in &mut program.impls {
        for method in &mut decl.methods {
            let owner = method.name.clone();
            let ret = method.return_type.clone();
            check_and_expand_body(&mut method.body, &ret, &owner, &sigs, diagnostics);
        }
    }
    for decl in &mut program.actors {
        if let Some(init) = &mut decl.init {
            let void = TypeRef::new("void");
            check_and_expand_body(&mut init.body, &void, &decl.name, &sigs, diagnostics);
        }
        for handler in &mut decl.handlers {
            let reply = handler
                .reply_type
                .clone()
                .unwrap_or_else(|| TypeRef::new("void"));
            check_and_expand_body(&mut handler.body, &reply, &handler.name, &sigs, diagnostics);
        }
    }
}

/// Walk a function/method body: apply the construction-length assertion at every
/// expected-type site and expand every fill literal. `return_type` is the body's
/// declared return type (used for `return` and a trailing expression).
fn check_and_expand_body(
    body: &mut [Stmt],
    return_type: &TypeRef,
    owner: &str,
    sigs: &Sigs,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let len = body.len();
    for (index, stmt) in body.iter_mut().enumerate() {
        // A function's value is its trailing expression when the last statement
        // is a bare expression, so it carries the return type as its expected.
        let trailing = index + 1 == len;
        check_and_expand_stmt(stmt, return_type, trailing, owner, sigs, diagnostics);
    }
}

fn check_and_expand_stmt(
    stmt: &mut Stmt,
    return_type: &TypeRef,
    trailing: bool,
    owner: &str,
    sigs: &Sigs,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    match stmt {
        Stmt::Let { ty, value, .. } => {
            check_and_expand_expr(value, ty.as_ref(), owner, sigs, diagnostics);
        }
        Stmt::Return(expr) => {
            if let Some(expr) = expr {
                check_and_expand_expr(expr, Some(return_type), owner, sigs, diagnostics);
            }
        }
        Stmt::Expr(expr) => {
            let expected = if trailing { Some(return_type) } else { None };
            check_and_expand_expr(expr, expected, owner, sigs, diagnostics);
        }
        Stmt::Assign { value, .. } | Stmt::Throw { value, .. } => {
            check_and_expand_expr(value, None, owner, sigs, diagnostics);
        }
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            for branch in branches {
                check_and_expand_expr(&mut branch.condition, None, owner, sigs, diagnostics);
                check_and_expand_body(&mut branch.body, return_type, owner, sigs, diagnostics);
            }
            check_and_expand_body(else_body, return_type, owner, sigs, diagnostics);
        }
        Stmt::While {
            condition, body, ..
        } => {
            check_and_expand_expr(condition, None, owner, sigs, diagnostics);
            check_and_expand_body(body, return_type, owner, sigs, diagnostics);
        }
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            check_and_expand_expr(start, None, owner, sigs, diagnostics);
            check_and_expand_expr(end, None, owner, sigs, diagnostics);
            if let Some(step) = step {
                check_and_expand_expr(step, None, owner, sigs, diagnostics);
            }
            check_and_expand_body(body, return_type, owner, sigs, diagnostics);
        }
        Stmt::ForEach { iterable, body, .. } => {
            check_and_expand_expr(iterable, None, owner, sigs, diagnostics);
            check_and_expand_body(body, return_type, owner, sigs, diagnostics);
        }
        Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } | Stmt::RegionBlock { body, .. } => {
            check_and_expand_body(body, return_type, owner, sigs, diagnostics);
        }
        Stmt::Try {
            body, catch_body, ..
        } => {
            check_and_expand_body(body, return_type, owner, sigs, diagnostics);
            check_and_expand_body(catch_body, return_type, owner, sigs, diagnostics);
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm { .. } | Stmt::Region(_) => {}
    }
}

/// Check a construction against its expected type and expand any fill literal.
/// `expected` carries a resolved literal extent when the context supplies one.
fn check_and_expand_expr(
    expr: &mut Expr,
    expected: Option<&TypeRef>,
    owner: &str,
    sigs: &Sigs,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let expected_extent = expected.and_then(TypeRef::array_extent);
    let element_expected = expected.and_then(TypeRef::array_element);

    match &mut expr.kind {
        ExprKind::Array(items) => {
            if let Some(extent) = expected_extent
                && items.len() as i64 != extent
            {
                diagnostics.push(count_mismatch(items.len() as i64, extent, expr.span, owner));
            }
            for item in items {
                check_and_expand_expr(item, element_expected.as_ref(), owner, sigs, diagnostics);
            }
        }
        ExprKind::ArrayFill { value, count } => {
            // Recurse into the fill value against the element type first, then
            // expand this node in place to an ordinary array literal.
            check_and_expand_expr(value, element_expected.as_ref(), owner, sigs, diagnostics);
            check_and_expand_expr(count, None, owner, sigs, diagnostics);
            let resolved = fill_count(count);
            if let (Some(k), Some(extent)) = (resolved, expected_extent)
                && k != extent
            {
                diagnostics.push(count_mismatch(k, extent, expr.span, owner));
            }
            *expr = expand_fill(value, count, expr.span, owner, diagnostics);
        }
        ExprKind::Call { name, args } => {
            let params = sigs.functions.get(name);
            for (index, arg) in args.iter_mut().enumerate() {
                let param_ty = params.and_then(|params| params.get(index));
                check_and_expand_expr(arg, param_ty, owner, sigs, diagnostics);
            }
        }
        ExprKind::StructLiteral { name, fields } => {
            let field_types = sigs.structs.get(name);
            for (field_name, value) in fields {
                let field_ty = field_types.and_then(|types| {
                    types
                        .iter()
                        .find(|(candidate, _)| candidate == field_name)
                        .map(|(_, ty)| ty)
                });
                check_and_expand_expr(value, field_ty, owner, sigs, diagnostics);
            }
        }
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            check_and_expand_expr(cond, None, owner, sigs, diagnostics);
            // A ternary's arms are in the same expected-type position as the
            // whole expression, so the extent (if any) propagates to both.
            check_and_expand_expr(then_branch, expected, owner, sigs, diagnostics);
            check_and_expand_expr(else_branch, expected, owner, sigs, diagnostics);
        }
        ExprKind::Index { target, index } => {
            check_and_expand_expr(target, None, owner, sigs, diagnostics);
            check_and_expand_expr(index, None, owner, sigs, diagnostics);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Await { expr } | ExprKind::Try(expr) => {
            check_and_expand_expr(expr, None, owner, sigs, diagnostics)
        }
        ExprKind::Binary { left, right, .. }
        | ExprKind::In {
            value: left,
            collection: right,
        } => {
            check_and_expand_expr(left, None, owner, sigs, diagnostics);
            check_and_expand_expr(right, None, owner, sigs, diagnostics);
        }
        ExprKind::Spawn { args, .. } => {
            for arg in args {
                check_and_expand_expr(arg, None, owner, sigs, diagnostics);
            }
        }
        ExprKind::Tell { target, args, .. } => {
            check_and_expand_expr(target, None, owner, sigs, diagnostics);
            for arg in args {
                check_and_expand_expr(arg, None, owner, sigs, diagnostics);
            }
        }
        ExprKind::Field { target, .. } => {
            check_and_expand_expr(target, None, owner, sigs, diagnostics)
        }
        ExprKind::Match { scrutinee, arms } => {
            check_and_expand_expr(scrutinee, None, owner, sigs, diagnostics);
            for arm in arms {
                // Match arms are handled as statements; their trailing value (if
                // the match is in value position) decays to `array<T>` — the
                // extent assertion does not reach here.
                check_and_expand_body(
                    &mut arm.body,
                    &TypeRef::new("void"),
                    owner,
                    sigs,
                    diagnostics,
                );
            }
        }
        ExprKind::Closure { body, .. } => {
            check_and_expand_expr(body, None, owner, sigs, diagnostics)
        }
        ExprKind::Slice { target, start, end } => {
            check_and_expand_expr(target, None, owner, sigs, diagnostics);
            if let Some(start) = start {
                check_and_expand_expr(start, None, owner, sigs, diagnostics);
            }
            if let Some(end) = end {
                check_and_expand_expr(end, None, owner, sigs, diagnostics);
            }
        }
        ExprKind::Combinator { operand, .. } => {
            check_and_expand_expr(operand, None, owner, sigs, diagnostics)
        }
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Char(_)
        | ExprKind::Variable(_) => {}
    }
}

/// The literal fill count of a `[v; k]` node, if `k` folded to a positive
/// integer. Constant folding already turned a named constant count into an
/// integer literal, so a non-`Integer` count is a non-constant fill count.
fn fill_count(count: &Expr) -> Option<i64> {
    match &count.kind {
        ExprKind::Integer(value) if *value > 0 => Some(*value),
        _ => None,
    }
}

/// Expand a fill literal `[value; count]` to an ordinary `count`-element array
/// literal (`count` copies of `value`). A non-constant or non-positive count is
/// rejected (`L0463`/`L0464`) and expanded to a single-element array so that no
/// `ArrayFill` node survives into the checker or any backend. The fill `value`
/// is materialized once per element.
fn expand_fill(
    value: &Expr,
    count: &Expr,
    span: Span,
    owner: &str,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) -> Expr {
    let repeat = match &count.kind {
        ExprKind::Integer(value) if *value > 0 => *value as usize,
        ExprKind::Integer(value) => {
            diagnostics.push(SemanticDiagnostic::at(
                "L0464",
                format!("array fill count must be a positive integer, but it is {value}"),
                Some(owner.to_string()),
                span,
            ));
            1
        }
        _ => {
            diagnostics.push(SemanticDiagnostic::at(
                "L0463",
                "array fill count `[value; N]` must be a constant integer (a literal or a named \
                 integer constant)",
                Some(owner.to_string()),
                span,
            ));
            1
        }
    };
    let items = std::iter::repeat_n(value.clone(), repeat).collect();
    Expr {
        kind: ExprKind::Array(items),
        span,
    }
}

fn count_mismatch(actual: i64, expected: i64, span: Span, owner: &str) -> SemanticDiagnostic {
    SemanticDiagnostic::at(
        "L0465",
        format!(
            "array construction has {actual} element(s) but the declared type requires exactly \
             {expected}"
        ),
        Some(owner.to_string()),
        span,
    )
}

#[cfg(test)]
mod tests {
    use lullaby_lexer::lex;
    use lullaby_parser::parse;

    use crate::{SemanticDiagnostic, validate};

    fn diagnostics(source: &str) -> Vec<SemanticDiagnostic> {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        validate(&program).err().unwrap_or_default()
    }

    fn has_code(source: &str, code: &str) -> bool {
        diagnostics(source).iter().any(|d| d.code == code)
    }

    fn is_clean(source: &str) -> bool {
        diagnostics(source).is_empty()
    }

    #[test]
    fn accepts_literal_and_named_const_extents() {
        assert!(
            is_clean("fn main -> i64\n    let a array<i64, 4> = [1, 2, 3, 4]\n    a[0]\n"),
            "{:?}",
            diagnostics("fn main -> i64\n    let a array<i64, 4> = [1, 2, 3, 4]\n    a[0]\n")
        );
        let named = concat!(
            "const SIZE i64 = 3\n",
            "fn main -> i64\n",
            "    let a array<i64, SIZE> = [1, 2, 3]\n",
            "    a[0]\n",
        );
        assert!(is_clean(named), "{:?}", diagnostics(named));
    }

    #[test]
    fn accepts_fill_literal_matching_extent() {
        let source = "fn main -> i64\n    let a array<i64, 512> = [0; 512]\n    len(a)\n";
        assert!(is_clean(source), "{:?}", diagnostics(source));
    }

    #[test]
    fn rejects_non_constant_extent() {
        // A runtime parameter is not a constant extent.
        let source = concat!(
            "fn f x i64 -> i64\n",
            "    let a array<i64, x> = [1]\n",
            "    a[0]\n\n",
            "fn main -> i64\n    f(3)\n",
        );
        assert!(has_code(source, "L0463"), "{:?}", diagnostics(source));
    }

    #[test]
    fn rejects_zero_and_negative_extent() {
        assert!(
            has_code(
                "fn main -> i64\n    let a array<i64, 0> = [1]\n    a[0]\n",
                "L0464"
            ),
            "zero extent is rejected"
        );
        let negative = concat!(
            "const NEG i64 = 0 - 2\n",
            "fn main -> i64\n",
            "    let a array<i64, NEG> = [1]\n",
            "    a[0]\n",
        );
        assert!(has_code(negative, "L0464"), "{:?}", diagnostics(negative));
    }

    #[test]
    fn rejects_construction_length_mismatch() {
        // Literal too short for the declared extent.
        assert!(
            has_code(
                "fn main -> i64\n    let a array<i64, 4> = [1, 2, 3]\n    a[0]\n",
                "L0465"
            ),
            "literal count mismatch is rejected"
        );
        // Fill count disagrees with the declared extent.
        assert!(
            has_code(
                "fn main -> i64\n    let a array<i64, 4> = [0; 3]\n    a[0]\n",
                "L0465"
            ),
            "fill count mismatch is rejected"
        );
    }

    #[test]
    fn rejects_length_mismatch_in_struct_field_and_call_arg() {
        let field = concat!(
            "struct Frame\n    px array<i64, 4>\n\n",
            "fn main -> i64\n",
            "    let f = Frame(px: [1, 2])\n",
            "    f.px[0]\n",
        );
        assert!(has_code(field, "L0465"), "{:?}", diagnostics(field));
        let call = concat!(
            "fn take b array<i64, 4> -> i64\n    b[0]\n\n",
            "fn main -> i64\n    take([1, 2, 3])\n",
        );
        assert!(has_code(call, "L0465"), "{:?}", diagnostics(call));
    }

    #[test]
    fn rejects_return_length_mismatch() {
        let source = concat!(
            "fn make -> array<i64, 4>\n    [1, 2, 3]\n\n",
            "fn main -> i64\n    make()[0]\n",
        );
        assert!(has_code(source, "L0465"), "{:?}", diagnostics(source));
    }

    #[test]
    fn fixed_extent_array_decays_to_length_agnostic_array() {
        // Passing an `array<T, N>` where an `array<T>` is expected is allowed:
        // the extent decays. No diagnostic.
        let source = concat!(
            "fn total b array<i64> -> i64\n    len(b)\n\n",
            "fn main -> i64\n",
            "    let a array<i64, 4> = [1, 2, 3, 4]\n",
            "    total(a)\n",
        );
        assert!(is_clean(source), "{:?}", diagnostics(source));
    }

    #[test]
    fn accepts_nested_and_optional_fixed_extents() {
        let nested = concat!(
            "fn main -> i64\n",
            "    let g array<array<i64, 3>, 2> = [[1, 2, 3], [4, 5, 6]]\n",
            "    g[1][2]\n",
        );
        assert!(is_clean(nested), "{:?}", diagnostics(nested));
        // A nested inner mismatch is still caught.
        let bad = concat!(
            "fn main -> i64\n",
            "    let g array<array<i64, 3>, 2> = [[1, 2], [4, 5, 6]]\n",
            "    g[0][0]\n",
        );
        assert!(has_code(bad, "L0465"), "{:?}", diagnostics(bad));
    }
}
