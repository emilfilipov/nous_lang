//! Actor message-ownership analysis (concurrency stage 3): the **sendability**
//! predicate and the **move-by-default / use-after-send** check.
//!
//! Two cooperating pieces live here, split out of `lib.rs` (which is already
//! over the size cap) as a cohesive unit — they share the copy/move/shared
//! classification of a type:
//!
//! - [`Checker::first_non_sendable`] — the **fully transitive** sendability
//!   predicate. A type is non-sendable if it *is* a non-atomic `rc`/`ref`/raw
//!   `ptr`, or if **any** of its generic arguments, struct fields, enum-variant
//!   payloads, array/list/map elements, or `option`/`result` inner types is
//!   non-sendable. A visited-set guards recursive types (`enum List: Cons(i64,
//!   List) | Nil`) against infinite recursion. This closes a soundness gap where
//!   a non-atomic `rc`/`ref`/`ptr` wrapped in a struct field or enum payload
//!   could smuggle past `L0353` when sent via `spawn`/`tell`/`ask` (argument or
//!   reply type).
//!
//! - [`Checker::check_message_ownership`] — the affine **use-after-send**
//!   analysis. A message crosses an actor boundary by *value*: a non-copy value
//!   passed as a bare-variable argument to `tell`/`ask`/`spawn` is **moved** into
//!   the message and the sender loses access; any later read, re-send, or
//!   mutation of that binding is `L0357`. A **copy** type (scalars, `Actor<T>`/
//!   `shared<T>` handles, and scalar-only aggregates) is copied, not moved, so it
//!   may be reused. A **`shared<T>`** value (the atomic-rc immutable share) is
//!   sendable *and* not consumed, so it can be handed to several actors.
//!
//! Both run at check time (all tiers see them); the AST interpreter realizes a
//! "move" as an ordinary value copy into the message (the sender's binding is
//! left physically intact — the analysis is what forbids its reuse), so no
//! runtime invalidation is needed.

use std::collections::{HashMap, HashSet};

use lullaby_parser::{AssignOp, Expr, ExprKind, Place, Stmt, TypeRef};

use super::{Checker, SemanticDiagnostic, split_named_type};

/// The scalar type names that are trivially copied by value. A value of one of
/// these is copied into a message, never moved, so it stays usable after a send.
fn is_scalar_type_name(name: &str) -> bool {
    matches!(
        name,
        "i64"
            | "f64"
            | "f32"
            | "i8"
            | "i16"
            | "i32"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "isize"
            | "usize"
            | "bool"
            | "char"
            | "byte"
            | "void"
    )
}

impl<'a> Checker<'a> {
    /// The first non-sendable type spelling embedded anywhere in `ty`, or `None`
    /// when every part of `ty` is sendable across an actor boundary.
    ///
    /// A non-atomic `rc<T>`, a borrowed `ref<T>`, and a raw `ptr<T>` are the
    /// non-sendable heads; everything else (scalars, `string`, `char`, `byte`,
    /// `Actor<T>` handles, the `shared<T>` atomic-rc share, and structural
    /// containers whose parts are all sendable) is sendable. The check is
    /// **fully transitive**: it recurses into generic type arguments **and** into
    /// the fields of a struct head and the payloads of an enum head, so a
    /// non-atomic `rc`/`ref`/`ptr` cannot hide inside a struct field or enum
    /// payload. A visited-set of head names guards recursive user types against
    /// infinite recursion.
    pub(crate) fn first_non_sendable(&self, ty: &TypeRef) -> Option<String> {
        let mut visited = HashSet::new();
        self.first_non_sendable_rec(ty, &mut visited)
    }

    fn first_non_sendable_rec(
        &self,
        ty: &TypeRef,
        visited: &mut HashSet<String>,
    ) -> Option<String> {
        let (head, args) = split_named_type(ty);
        // The non-sendable heads: a non-atomic reference count, a borrowed alias,
        // or a raw pointer — none may be aliased into a second actor's heap.
        if matches!(head.as_str(), "rc" | "ref" | "ptr") {
            return Some(ty.name.clone());
        }
        // Legacy `ptr_T` spelling produced by `alloc` is also a raw pointer.
        if ty.name.starts_with("ptr_") {
            return Some(ty.name.clone());
        }
        // Recurse into every generic argument (`list<rc<i64>>`, `Actor<T>`,
        // `shared<rc<i64>>`, `result<i64, ptr<byte>>`, ...).
        for arg in &args {
            if let Some(offender) = self.first_non_sendable_rec(arg, visited) {
                return Some(offender);
            }
        }
        // Recurse into a struct head's fields / an enum head's payloads, guarding
        // recursive types. `visited` tracks head names entered by definition; the
        // generic-argument recursion above already covers type-parameter
        // instantiations, so a raw (unsubstituted) definition lookup is sound —
        // any concretely-embedded non-sendable head is found regardless of the
        // type parameters.
        if visited.insert(head.clone()) {
            if let Some(fields) = self.structs.get(&head) {
                for field in fields {
                    if let Some(offender) = self.first_non_sendable_rec(&field.ty, visited) {
                        return Some(offender);
                    }
                }
            } else if let Some(variants) = self.enums.get(&head) {
                for variant in variants {
                    for payload in &variant.payload {
                        if let Some(offender) = self.first_non_sendable_rec(payload, visited) {
                            return Some(offender);
                        }
                    }
                }
            }
        }
        None
    }

    /// Whether sending a value of type `ty` **consumes** (moves) it. A type is
    /// consumed unless it is a **copy** type: a scalar, an `Actor<T>`/`shared<T>`
    /// handle, or a struct/enum/`option`/`result` all of whose parts are
    /// themselves copy. Owned aggregates (`string`, `list`, `map`, `array`, and
    /// structs/enums containing them) are moved.
    pub(crate) fn send_consumes(&self, ty: &TypeRef) -> bool {
        let mut visited = HashSet::new();
        !self.is_copy_type(ty, &mut visited)
    }

    fn is_copy_type(&self, ty: &TypeRef, visited: &mut HashSet<String>) -> bool {
        let (head, args) = split_named_type(ty);
        if is_scalar_type_name(&head) {
            return true;
        }
        // Handles are shareable references, not owned payloads: sending one is a
        // copy of the address, so the sender keeps its handle. `Actor<T>` lets an
        // actor be addressed by many; `shared<T>` is the atomic-rc immutable
        // share explicitly meant to be referenced from more than one actor.
        if matches!(head.as_str(), "Actor" | "shared") {
            return true;
        }
        // Owned heap aggregates and reference/pointer/future forms are moved.
        if matches!(
            head.as_str(),
            "string" | "list" | "map" | "array" | "rc" | "ref" | "ptr" | "Future"
        ) || ty.name.starts_with("ptr_")
        {
            return false;
        }
        // `option<T>` / `result<T, E>` are copy iff their inner types are copy.
        if matches!(head.as_str(), "option" | "result") {
            return args.iter().all(|arg| self.is_copy_type(arg, visited));
        }
        // A user struct: copy iff every (substituted) field is copy. A recursive
        // struct (revisited head) needs indirection and is treated as moved.
        if self.structs.contains_key(&head) {
            if !visited.insert(head.clone()) {
                return false;
            }
            let copy = match self.struct_fields_for(ty) {
                Some(fields) => fields
                    .iter()
                    .all(|field| self.is_copy_type(&field.ty, visited)),
                None => false,
            };
            visited.remove(&head);
            return copy;
        }
        // A user enum: copy iff every variant payload is copy (raw payloads —
        // conservative for a generic enum whose payload is a bare parameter,
        // which is treated as non-copy, i.e. moved).
        if let Some(variants) = self.enums.get(&head) {
            if !visited.insert(head.clone()) {
                return false;
            }
            let copy = variants.iter().all(|variant| {
                variant
                    .payload
                    .iter()
                    .all(|p| self.is_copy_type(p, visited))
            });
            visited.remove(&head);
            return copy;
        }
        // Any other type (a bare generic parameter, an unresolved spelling) is
        // treated conservatively as moved so a genuine reuse is not missed.
        false
    }

    /// Run the affine use-after-send analysis over `body` (a function, actor
    /// `init`, or actor handler body named `fn_name`). Must run **after** the
    /// body has been type-checked so `self.expression_types` holds the type of
    /// every argument expression.
    ///
    /// **Rule (path model).** Straight-line code is order-sensitive and precise:
    /// a binding moved into a send is consumed from that point on. At a
    /// conditional/`match`/`try` join the analysis takes the **union** of moves
    /// along the branches (may-move), and each branch is analyzed from the
    /// pre-branch state so disjoint branches never cross-contaminate. Loop bodies
    /// are analyzed once with moves **propagated out** (a move inside a loop is
    /// conservatively visible after the loop). A full reassignment (`x = e`) or a
    /// fresh `let x = e` **revives** the binding. What counts as a *use*: any
    /// read of the binding, re-sending it, mutating it (a compound assignment or
    /// a field/index store on it), or capturing it in a closure.
    ///
    /// A remaining conservative edge (documented, deferred to a later stage): a
    /// move made inside a loop body is not re-checked against a *later* iteration
    /// that reads the binding before the send — matching the existing
    /// resource-lifetime (`L0350`) analysis's straight-line treatment of loops.
    pub(crate) fn check_message_ownership(&mut self, fn_name: &str, body: &[Stmt]) {
        // Build a (line, column) -> type map for this function's expressions so a
        // bare-variable send argument can be classified copy/move/shared.
        let mut types: HashMap<(usize, usize), TypeRef> = HashMap::new();
        for entry in &self.expression_types {
            if entry.function == fn_name {
                types.insert((entry.span.line, entry.span.column), entry.ty.clone());
            }
        }
        let mut moved: HashSet<String> = HashSet::new();
        self.walk_ownership(body, &mut moved, &types, fn_name);
    }

    fn walk_ownership(
        &mut self,
        body: &[Stmt],
        moved: &mut HashSet<String>,
        types: &HashMap<(usize, usize), TypeRef>,
        fn_name: &str,
    ) {
        for statement in body {
            match statement {
                Stmt::Let { name, value, .. } => {
                    self.check_uses_and_sends(value, moved, types, fn_name);
                    // A fresh binding revives the name (a new value lives here).
                    moved.remove(name);
                }
                Stmt::Assign {
                    name,
                    path,
                    op,
                    value,
                    span,
                } => {
                    self.check_uses_and_sends(value, moved, types, fn_name);
                    for place in path {
                        if let Place::Index(index) = place {
                            self.check_uses_and_sends(index, moved, types, fn_name);
                        }
                    }
                    let full_rebind = path.is_empty() && matches!(op, AssignOp::Replace);
                    if full_rebind {
                        // A plain `x = e` reassigns the whole binding: it revives.
                        moved.remove(name);
                    } else if moved.contains(name) {
                        // A compound assignment (`x += e`) or a field/index store
                        // (`x.f = e`, `x[i] = e`) reads-and-mutates the moved
                        // binding, which is a use-after-send.
                        self.report_use_after_send(name, *span, fn_name);
                    }
                }
                Stmt::Expr(expr) | Stmt::Return(Some(expr)) => {
                    self.check_uses_and_sends(expr, moved, types, fn_name);
                }
                Stmt::Throw { value, .. } => {
                    self.check_uses_and_sends(value, moved, types, fn_name);
                }
                Stmt::If {
                    branches,
                    else_body,
                    ..
                } => {
                    // Analyze each branch from the pre-branch state; the moves
                    // that survive the `if` are the union across all branches.
                    let mut after = moved.clone();
                    for branch in branches {
                        self.check_uses_and_sends(&branch.condition, moved, types, fn_name);
                        let mut branch_moved = moved.clone();
                        self.walk_ownership(&branch.body, &mut branch_moved, types, fn_name);
                        after.extend(branch_moved);
                    }
                    let mut else_moved = moved.clone();
                    self.walk_ownership(else_body, &mut else_moved, types, fn_name);
                    after.extend(else_moved);
                    *moved = after;
                }
                Stmt::While {
                    condition, body, ..
                } => {
                    self.check_uses_and_sends(condition, moved, types, fn_name);
                    // Thread `moved` through the loop body so a move inside it is
                    // conservatively visible after the loop.
                    self.walk_ownership(body, moved, types, fn_name);
                }
                Stmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    self.check_uses_and_sends(start, moved, types, fn_name);
                    self.check_uses_and_sends(end, moved, types, fn_name);
                    if let Some(step) = step {
                        self.check_uses_and_sends(step, moved, types, fn_name);
                    }
                    self.walk_ownership(body, moved, types, fn_name);
                }
                Stmt::ForEach { iterable, body, .. } => {
                    self.check_uses_and_sends(iterable, moved, types, fn_name);
                    self.walk_ownership(body, moved, types, fn_name);
                }
                Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } => {
                    self.walk_ownership(body, moved, types, fn_name);
                }
                Stmt::Try {
                    body, catch_body, ..
                } => {
                    let mut body_moved = moved.clone();
                    self.walk_ownership(body, &mut body_moved, types, fn_name);
                    let mut catch_moved = moved.clone();
                    self.walk_ownership(catch_body, &mut catch_moved, types, fn_name);
                    body_moved.extend(catch_moved);
                    *moved = body_moved;
                }
                Stmt::Return(None)
                | Stmt::Break(_)
                | Stmt::Continue(_)
                | Stmt::Region(_)
                | Stmt::Asm { .. } => {}
            }
        }
    }

    /// Walk an expression: flag any read of an already-moved binding, and mark a
    /// binding moved when it is a bare-variable argument of a consuming send.
    fn check_uses_and_sends(
        &mut self,
        expr: &Expr,
        moved: &mut HashSet<String>,
        types: &HashMap<(usize, usize), TypeRef>,
        fn_name: &str,
    ) {
        match &expr.kind {
            ExprKind::Variable(name) => {
                if moved.contains(name) {
                    self.report_use_after_send(name, expr.span, fn_name);
                }
            }
            ExprKind::Spawn { args, .. } => {
                for arg in args {
                    self.check_uses_and_sends(arg, moved, types, fn_name);
                }
                self.mark_moved_args(args, moved, types);
            }
            ExprKind::Tell { target, args, .. } => {
                // The target is an `Actor<T>` handle (a copy type), never
                // consumed; still check it for a use of a moved binding.
                self.check_uses_and_sends(target, moved, types, fn_name);
                for arg in args {
                    self.check_uses_and_sends(arg, moved, types, fn_name);
                }
                self.mark_moved_args(args, moved, types);
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.check_uses_and_sends(value, moved, types, fn_name);
                }
            }
            ExprKind::Index { target, index } => {
                self.check_uses_and_sends(target, moved, types, fn_name);
                self.check_uses_and_sends(index, moved, types, fn_name);
            }
            ExprKind::Field { target, .. } => {
                self.check_uses_and_sends(target, moved, types, fn_name);
            }
            ExprKind::Await { expr } | ExprKind::Try(expr) | ExprKind::Unary { expr, .. } => {
                self.check_uses_and_sends(expr, moved, types, fn_name);
            }
            ExprKind::Binary { left, right, .. } => {
                self.check_uses_and_sends(left, moved, types, fn_name);
                self.check_uses_and_sends(right, moved, types, fn_name);
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.check_uses_and_sends(arg, moved, types, fn_name);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for (_, value) in fields {
                    self.check_uses_and_sends(value, moved, types, fn_name);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_uses_and_sends(scrutinee, moved, types, fn_name);
                let mut after = moved.clone();
                for arm in arms {
                    let mut arm_moved = moved.clone();
                    self.walk_ownership(&arm.body, &mut arm_moved, types, fn_name);
                    after.extend(arm_moved);
                }
                *moved = after;
            }
            // A closure captures the enclosing locals by value at evaluation
            // time, so referencing a moved binding through the body is a use.
            ExprKind::Closure { body, .. } => {
                self.check_uses_and_sends(body, moved, types, fn_name);
            }
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                self.check_uses_and_sends(cond, moved, types, fn_name);
                self.check_uses_and_sends(then_branch, moved, types, fn_name);
                self.check_uses_and_sends(else_branch, moved, types, fn_name);
            }
            ExprKind::In { value, collection } => {
                self.check_uses_and_sends(value, moved, types, fn_name);
                self.check_uses_and_sends(collection, moved, types, fn_name);
            }
            ExprKind::Slice { target, start, end } => {
                self.check_uses_and_sends(target, moved, types, fn_name);
                if let Some(start) = start {
                    self.check_uses_and_sends(start, moved, types, fn_name);
                }
                if let Some(end) = end {
                    self.check_uses_and_sends(end, moved, types, fn_name);
                }
            }
            ExprKind::Integer(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::String(_)
            | ExprKind::Char(_) => {}
        }
    }

    /// Mark every bare-variable argument whose type is a consuming (move) type as
    /// moved. Copy and `shared<T>`/`Actor<T>` arguments are left usable.
    fn mark_moved_args(
        &self,
        args: &[Expr],
        moved: &mut HashSet<String>,
        types: &HashMap<(usize, usize), TypeRef>,
    ) {
        for arg in args {
            if let ExprKind::Variable(name) = &arg.kind
                && let Some(ty) = types.get(&(arg.span.line, arg.span.column))
                && self.send_consumes(ty)
            {
                moved.insert(name.clone());
            }
        }
    }

    fn report_use_after_send(
        &mut self,
        name: &str,
        span: lullaby_diagnostics::Span,
        fn_name: &str,
    ) {
        self.diagnostics.push(SemanticDiagnostic::at(
            "L0357",
            format!(
                "`{name}` was moved into an actor message and cannot be used again: a non-copy value is moved by `tell`/`ask`/`spawn` and the sender loses access — copy it before the send, wrap it in a `shared<T>` handle, or restructure so the value is not used after it is sent"
            ),
            Some(fn_name.to_string()),
            span,
        ));
    }
}
