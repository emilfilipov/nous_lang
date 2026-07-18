//! Freestanding-tier (`no-runtime`) enforcement — stage 1 of the freestanding /
//! kernel tier (`documents/freestanding_tier_design.md`).
//!
//! A module that opens with the `no-runtime` directive is compiled in the
//! freestanding tier, which removes the safe-tier runtime (host allocator, RC,
//! actor scheduler, growable heap). This pass enforces the two hard rules of that
//! tier — **no hidden allocation** and **no hidden control flow / runtime
//! dependency** — by rejecting, with the single diagnostic **`L0441`**, any
//! construct that would need the runtime:
//!
//! - a growable / heap **data type** anywhere in a type spelling — `list<T>`,
//!   `map<K, V>`, the `string` heap type, the reference-counted handles `rc<T>` /
//!   `ref<T>`, and the concurrency handles `Future<T>` / `Actor<T>`;
//! - an **actor** declaration, a `spawn`, a `tell`, or an `await`;
//! - a **closure literal** (it needs a heap-allocated capture environment);
//! - a host-allocator builtin (`alloc` / `dealloc`);
//! - any **expression whose value type** is one of the heap/runtime types above
//!   (this catches string building such as `a + b` and collection builders such
//!   as `list_new()` / `to_string(x)` even without a type annotation).
//!
//! What stays **allowed** in `no-runtime` is the safe-arena-kernel core: scalars,
//! fixed `array<T>`, structs/enums over allowed fields, `option`/`result`,
//! control flow, functions, and the raw hardware surface (`unsafe` blocks, raw
//! `ptr<T>`, and the delivered `ptr_*` / `volatile_*` / `int_to_ptr` / `ptr_to_int`
//! builtins). The enforcement is purely compile-time: a `no-runtime` program that
//! stays inside the allowed subset type-checks and runs on the interpreters
//! exactly like any other program, and composes with the native `--freestanding`
//! output path unchanged.
//!
//! Static-buffer-backed arenas, inline `asm` operand binding, MMIO/port-IO,
//! interrupt/`naked`/`entry` functions, the pluggable panic handler, and
//! direct-ELF/flat-binary output are **later freestanding-tier stages** and are
//! intentionally not built here; stage 1 is the gate plus the allowed/rejected
//! boundary.

use std::collections::HashSet;

use lullaby_diagnostics::Span;
use lullaby_parser::{Expr, ExprKind, Function, Program, Stmt, TypeRef};

use crate::{ExpressionType, SemanticDiagnostic};

/// The heap/runtime type constructors a `no-runtime` module may not name. Each
/// implies the safe-tier runtime: `list`/`map` grow via the host allocator,
/// `string` is the growable heap string, `rc`/`ref` are reference-counted
/// handles, `Future`/`Actor` are the actor/async concurrency handles, and
/// `shared` is the actor-model atomic-rc immutable share.
const FORBIDDEN_TYPE_CTORS: [&str; 8] = [
    "list", "map", "string", "rc", "ref", "Future", "Actor", "shared",
];

/// Host-allocator / safe-tier builtins that are unavailable in `no-runtime`:
/// `alloc`/`dealloc` allocate via the host allocator and return/consume a raw
/// `ptr<T>` (an allowed type), and `share`/`shared_get` are the actor-model
/// immutable-share operations — none of which the type-based gate catches, so
/// they are rejected by name.
const FORBIDDEN_BUILTINS: [&str; 4] = ["alloc", "dealloc", "share", "shared_get"];

/// Enforce the freestanding-tier rules over a `no-runtime` `program`, appending an
/// `L0441` for every violation to `diagnostics`. `expression_types` is the checker's
/// recorded per-expression type table, consulted to catch any value whose type is
/// a heap/runtime type regardless of whether it was written with an annotation.
///
/// A no-op for a program without the `no-runtime` directive, so a module that does
/// not opt in is completely unaffected.
pub(crate) fn enforce(
    program: &Program,
    expression_types: &[ExpressionType],
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    if !program.is_no_runtime {
        return;
    }
    let mut checker = NoRuntimeChecker {
        expression_types,
        diagnostics,
        reported: HashSet::new(),
    };
    checker.run(program);
}

struct NoRuntimeChecker<'a> {
    expression_types: &'a [ExpressionType],
    diagnostics: &'a mut Vec<SemanticDiagnostic>,
    /// `(line, column)` positions already reported, so a violation surfaced
    /// through more than one path (a `list<i64>` annotation whose initializer is
    /// also `list`-typed, say) is reported once. `Span` is not `Hash`, so its
    /// coordinates are the key.
    reported: HashSet<(usize, usize)>,
}

impl NoRuntimeChecker<'_> {
    fn run(&mut self, program: &Program) {
        // Declaration signatures: an unavailable type in a parameter, return,
        // field, payload, alias target, or constant type.
        for function in &program.functions {
            self.check_function_signature(function);
        }
        for decl in &program.structs {
            for field in &decl.fields {
                self.reject_type(&field.ty, "field", decl.span, Some(&decl.name));
            }
        }
        for decl in &program.enums {
            for variant in &decl.variants {
                for payload in &variant.payload {
                    self.reject_type(payload, "enum payload", decl.span, Some(&decl.name));
                }
            }
        }
        for decl in &program.aliases {
            self.reject_type(&decl.target, "alias target", decl.span, None);
        }
        for decl in &program.consts {
            self.reject_type(&decl.ty, "constant", decl.span, None);
        }
        for decl in &program.traits {
            for method in &decl.methods {
                for param in &method.params {
                    self.reject_type(&param.ty, "parameter", method.span, Some(&decl.name));
                }
                self.reject_type(&method.return_type, "return", method.span, Some(&decl.name));
            }
        }
        for decl in &program.impls {
            for method in &decl.methods {
                self.check_function_signature(method);
            }
        }

        // Actors are a runtime construct: reject each declaration outright. Their
        // handler/init bodies are not walked — the whole actor is unavailable.
        for decl in &program.actors {
            self.report(
                decl.span,
                Some(&decl.name),
                "an `actor` declaration is unavailable in a `no-runtime` module \
                 (actors require the Lullaby runtime scheduler); express concurrency \
                 with the raw primitives outside a `no-runtime` module"
                    .to_string(),
            );
        }

        // Function bodies: constructs (spawn/tell/await/closure/host-allocator
        // builtins) and any value whose type is a heap/runtime type.
        for function in &program.functions {
            let name = function.name.clone();
            for stmt in &function.body {
                self.check_stmt(stmt, &name);
            }
        }
        for decl in &program.impls {
            for method in &decl.methods {
                let name = method.name.clone();
                for stmt in &method.body {
                    self.check_stmt(stmt, &name);
                }
            }
        }
    }

    fn check_function_signature(&mut self, function: &Function) {
        if function.is_async {
            self.report(
                function.span,
                Some(&function.name),
                "an `async fn` is unavailable in a `no-runtime` module \
                 (it requires the Lullaby runtime); use an ordinary `fn`"
                    .to_string(),
            );
        }
        for param in &function.params {
            self.reject_type(&param.ty, "parameter", function.span, Some(&function.name));
        }
        self.reject_type(
            &function.return_type,
            "return",
            function.span,
            Some(&function.name),
        );
    }

    /// Walk a statement, recursing into nested blocks and every expression.
    fn check_stmt(&mut self, stmt: &Stmt, function: &str) {
        match stmt {
            Stmt::Let { value, .. } => self.check_expr(value, function),
            Stmt::Assign { value, path, .. } => {
                use lullaby_parser::Place;
                for step in path {
                    if let Place::Index(index) = step {
                        self.check_expr(index, function);
                    }
                }
                self.check_expr(value, function);
            }
            Stmt::Return(Some(value)) => self.check_expr(value, function),
            Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm { .. } => {}
            Stmt::Expr(expr) => self.check_expr(expr, function),
            Stmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    self.check_expr(&branch.condition, function);
                    self.check_block(&branch.body, function);
                }
                self.check_block(else_body, function);
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.check_expr(condition, function);
                self.check_block(body, function);
            }
            Stmt::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                self.check_expr(start, function);
                self.check_expr(end, function);
                if let Some(step) = step {
                    self.check_expr(step, function);
                }
                self.check_block(body, function);
            }
            Stmt::ForEach { iterable, body, .. } => {
                self.check_expr(iterable, function);
                self.check_block(body, function);
            }
            Stmt::Loop { body, .. }
            | Stmt::Unsafe { body, .. }
            | Stmt::RegionBlock { body, .. } => self.check_block(body, function),
            Stmt::Region(_) => {}
            Stmt::Throw { value, .. } => self.check_expr(value, function),
            Stmt::Try {
                body, catch_body, ..
            } => {
                self.check_block(body, function);
                self.check_block(catch_body, function);
            }
        }
    }

    fn check_block(&mut self, body: &[Stmt], function: &str) {
        for stmt in body {
            self.check_stmt(stmt, function);
        }
    }

    /// Walk an expression. Runtime constructs (spawn/tell/await/closure, and the
    /// host-allocator builtins) are rejected with a construct-specific message;
    /// any other expression whose recorded value type is a heap/runtime type is
    /// rejected as the outermost such node (nested subexpressions of a rejected
    /// value are not additionally reported).
    fn check_expr(&mut self, expr: &Expr, function: &str) {
        match &expr.kind {
            ExprKind::Spawn { args, .. } => {
                self.report(
                    expr.span,
                    Some(function),
                    "`spawn` is unavailable in a `no-runtime` module \
                     (actors require the Lullaby runtime scheduler)"
                        .to_string(),
                );
                for arg in args {
                    self.check_expr(arg, function);
                }
            }
            ExprKind::Tell { target, args, .. } => {
                self.report(
                    expr.span,
                    Some(function),
                    "`tell` is unavailable in a `no-runtime` module \
                     (actors require the Lullaby runtime scheduler)"
                        .to_string(),
                );
                self.check_expr(target, function);
                for arg in args {
                    self.check_expr(arg, function);
                }
            }
            ExprKind::Await { expr: inner } => {
                self.report(
                    expr.span,
                    Some(function),
                    "`await` is unavailable in a `no-runtime` module \
                     (it requires the Lullaby runtime)"
                        .to_string(),
                );
                self.check_expr(inner, function);
            }
            ExprKind::Combinator { op, operand } => {
                self.report(
                    expr.span,
                    Some(function),
                    format!(
                        "`{}` is unavailable in a `no-runtime` module \
                         (the future combinators need the Lullaby runtime scheduler)",
                        op.as_str()
                    ),
                );
                self.check_expr(operand, function);
            }
            ExprKind::Closure { .. } => {
                self.report(
                    expr.span,
                    Some(function),
                    "a closure literal is unavailable in a `no-runtime` module \
                     (it needs a heap-allocated capture environment); refer to a \
                     top-level `fn` by name instead"
                        .to_string(),
                );
            }
            ExprKind::Call { name, args } if FORBIDDEN_BUILTINS.contains(&name.as_str()) => {
                self.report(
                    expr.span,
                    Some(function),
                    format!(
                        "`{name}` is unavailable in a `no-runtime` module \
                         (it allocates via the host allocator); use a raw `ptr<T>` \
                         over caller-provided memory instead"
                    ),
                );
                for arg in args {
                    self.check_expr(arg, function);
                }
            }
            _ => {
                // A non-construct expression. If its recorded value type is a
                // heap/runtime type, reject it here (the outermost such node) and
                // do not descend — otherwise walk its children.
                if let Some(ctor) = self.forbidden_value_type(expr.span) {
                    self.report_value(expr.span, function, &ctor);
                    return;
                }
                self.walk_children(expr, function);
            }
        }
    }

    /// Recurse into the sub-expressions of `expr` (used when `expr` itself is
    /// allowed, so a nested violation is still found).
    fn walk_children(&mut self, expr: &Expr, function: &str) {
        match &expr.kind {
            ExprKind::Integer(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::String(_)
            | ExprKind::Char(_)
            | ExprKind::Variable(_) => {}
            ExprKind::Array(items) => {
                for item in items {
                    self.check_expr(item, function);
                }
            }
            ExprKind::ArrayFill { value, count } => {
                self.check_expr(value, function);
                self.check_expr(count, function);
            }
            ExprKind::Index { target, index } => {
                self.check_expr(target, function);
                self.check_expr(index, function);
            }
            ExprKind::Unary { expr, .. } => self.check_expr(expr, function),
            ExprKind::Binary { left, right, .. } => {
                self.check_expr(left, function);
                self.check_expr(right, function);
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.check_expr(arg, function);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    self.check_expr(value, function);
                }
            }
            ExprKind::Field { target, .. } => self.check_expr(target, function),
            ExprKind::Match { scrutinee, arms } => {
                self.check_expr(scrutinee, function);
                for arm in arms {
                    self.check_block(&arm.body, function);
                }
            }
            ExprKind::Try(inner) => self.check_expr(inner, function),
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                self.check_expr(cond, function);
                self.check_expr(then_branch, function);
                self.check_expr(else_branch, function);
            }
            ExprKind::In { value, collection } => {
                self.check_expr(value, function);
                self.check_expr(collection, function);
            }
            ExprKind::Slice { target, start, end } => {
                self.check_expr(target, function);
                if let Some(start) = start {
                    self.check_expr(start, function);
                }
                if let Some(end) = end {
                    self.check_expr(end, function);
                }
            }
            // Handled as constructs in `check_expr`; never reached here.
            ExprKind::Spawn { .. }
            | ExprKind::Tell { .. }
            | ExprKind::Await { .. }
            | ExprKind::Combinator { .. }
            | ExprKind::Closure { .. } => {}
        }
    }

    /// The recorded value type at `span`, if it names a heap/runtime constructor.
    fn forbidden_value_type(&self, span: Span) -> Option<String> {
        let ty = self
            .expression_types
            .iter()
            .find(|entry| entry.span == span)
            .map(|entry| &entry.ty)?;
        forbidden_ctor(ty)
    }

    /// Reject a type spelling in a declaration signature (parameter, return,
    /// field, …) if it names a heap/runtime constructor.
    fn reject_type(&mut self, ty: &TypeRef, position: &str, span: Span, function: Option<&str>) {
        if let Some(ctor) = forbidden_ctor(ty) {
            self.report(
                span,
                function,
                format!(
                    "`{ctor}` is unavailable in a `no-runtime` module \
                     (it requires the Lullaby runtime); a {position} must use a scalar, \
                     a fixed `array<T>`, a struct/enum, or a raw `ptr<T>`"
                ),
            );
        }
    }

    fn report_value(&mut self, span: Span, function: &str, ctor: &str) {
        self.report(
            span,
            Some(function),
            format!(
                "a value of type `{ctor}` is unavailable in a `no-runtime` module \
                 (it requires the Lullaby runtime allocator)"
            ),
        );
    }

    fn report(&mut self, span: Span, function: Option<&str>, message: String) {
        // De-duplicate by source position so a violation reachable through more
        // than one path is reported once.
        if !self.reported.insert((span.line, span.column)) {
            return;
        }
        self.diagnostics.push(SemanticDiagnostic::at(
            "L0441",
            message,
            function.map(|name| name.to_string()),
            span,
        ));
    }
}

/// The first heap/runtime constructor named anywhere in `ty`'s spelling, if any.
///
/// The scan is nesting-aware: `array<string>` and `option<list<i64>>` are both
/// rejected because they embed `string` / `list`. A function type `fn(A) -> R` is
/// itself allowed (a bare function pointer needs no runtime), but its parameter
/// and return types are scanned so `fn(list<i64>) -> i64` is still rejected.
fn forbidden_ctor(ty: &TypeRef) -> Option<String> {
    // Function type: allowed itself; scan its parameter and return types.
    if let Some((params, ret)) = ty.function_signature() {
        for param in &params {
            if let Some(ctor) = forbidden_ctor(param) {
                return Some(ctor);
            }
        }
        return forbidden_ctor(&ret);
    }
    let name = ty.name.as_str();
    let (head, args) = match name.find('<') {
        Some(open) if name.ends_with('>') => {
            let head = &name[..open];
            let inner = &name[open + 1..name.len() - 1];
            (head, split_top_level(inner))
        }
        _ => (name, Vec::new()),
    };
    if FORBIDDEN_TYPE_CTORS.contains(&head) {
        return Some(head.to_string());
    }
    for arg in args {
        if let Some(ctor) = forbidden_ctor(&TypeRef::new(arg)) {
            return Some(ctor);
        }
    }
    None
}

/// Split the inner text of a `ctor<...>` spelling into its top-level,
/// nesting-aware comma-separated arguments (commas inside nested `<...>` are not
/// splits).
fn split_top_level(inner: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in inner.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(inner[start..index].trim().to_string());
                start = index + 1;
            }
            _ => {}
        }
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        args.push(tail.to_string());
    }
    args
}
