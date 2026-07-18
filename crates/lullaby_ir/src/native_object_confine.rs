//! Target-aware loop-confinement analysis (arena stage-2, increment I4).
//!
//! Split out of `native_object_eligibility.rs` (which is over the size backlog cap)
//! so the widened predicate lives in its own cohesive module. Sees the parent's
//! items — `BytecodeInstruction`, `BytecodeExpr`, `type_is_heap`, … — via
//! `use super::*`. The single public entry point is [`loop_body_confines_heap`],
//! recomputed identically by BOTH the arena-eligibility gate
//! (`arena_eligible_functions`) and the native emission (`arena_loop_reset_mark`),
//! so its verdict is one source of truth.
//!
//! # What "confines" means and why the target matters
//!
//! In an arena function (a leaf w.r.t. user code — no user/`extern` call can retain
//! a pointer, no `alloc` box — see the five eligibility criteria) the ONLY way a
//! heap value produced in an iteration becomes reachable AFTER the iteration edge is
//! by being **stored into a location that outlives the iteration**. Lullaby has
//! value semantics, so the only stores are `Assign` (a rebind or a container
//! mutation) and `Throw` (into the exception channel); a `Let` binds a fresh
//! iteration-local that dies at the iteration's end. A loop gets a per-iteration
//! sub-region whose bump-pointer rewind reclaims everything the iteration allocated,
//! so confinement must prove no such surviving store exists.
//!
//! The earlier rule denied confinement whenever ANY `Assign`/`Throw`/`Return` stored
//! a heap value, IGNORING the target. That is sound but over-conservative: it cannot
//! tell a genuinely-escaping loop-carried accumulator (`acc = acc + "x"`, `acc`
//! declared OUTSIDE the loop) from a per-iteration rebind of an iteration-local
//! (`s = fresh()`, `s` introduced INSIDE the loop body). This module makes the store
//! check **target-aware**: a whole-variable rebind of a provably iteration-local
//! binding does not escape and is admitted; everything else stays denied
//! (default-deny).
//!
//! # The exact "iteration-local target" predicate
//!
//! A store `Assign { name, path, value }` with a heap-typed `value` is treated as
//! **non-escaping** iff ALL hold:
//! 1. `path` is empty — a WHOLE-VARIABLE rebind (`name = <heap>`), never a
//!    field/element store `name.f = …` / `name[i] = …` (those could mutate an
//!    aggregate that outlives the iteration; kept denied, exactly as before).
//! 2. `name` is bound by a `Let` that is a **top-level statement of the loop body**
//!    of the loop directly enclosing the store — or, for a store nested inside an
//!    inner loop, a top-level `Let` of ANY loop from the confined loop down to that
//!    inner loop (the accumulated `locals` set). A top-level `Let` re-initializes the
//!    binding at the head of every iteration before any use, so the binding is never
//!    loop-carried, and it is lexically scoped to the loop body (the frontend clones
//!    the scope per loop — a post-loop read is a compile error), so it is never read
//!    after the loop. Both facts hold on every well-formed program; on a program that
//!    shadows a live outer binding of the same name the native backend already
//!    diverges from the interpreters (shared slot), so no CORRECT program is affected.
//! 3. The loop body is **widenable** — it contains no closure literal, no
//!    `ptr<…>`-typed expression (raw pointer / `addr_of` / `alloc`), and no inline
//!    `asm`. Any of these can capture or alias a local so that it outlives the
//!    iteration through a channel this value-semantic analysis does not model; when
//!    present, the module falls back to the strict "any heap store escapes" rule, so
//!    such loops keep their exact prior (pre-I4) classification.
//!
//! When the predicate cannot prove locality it DENIES — over-refusal costs only a
//! reclaim opportunity, whereas under-refusal would reclaim a live cell
//! (use-after-free). See `documents/lullaby_memory_management.md` and
//! `documents/execution_tiers_and_1_0_scope.md` for the soundness argument and the
//! escape-channel table.

use super::*;

/// Whether a loop's body **confines** its heap allocations to the iteration — i.e.
/// no heap value it produces can survive past the iteration edge — under the
/// target-aware rule described in the module docs. DEFAULT-DENY: a loop is confined
/// only when nothing is proven to escape.
///
/// Recomputed identically by the arena-eligibility gate and by native emission, so a
/// loop is given a per-iteration sub-region iff this returns `true` for its body.
pub(crate) fn loop_body_confines_heap(
    body: &[BytecodeInstruction],
    heap_aggs: &std::collections::HashSet<String>,
) -> bool {
    // Widening is enabled only for a body with no closure literal, no raw pointer,
    // and no inline asm — the channels through which an iteration-local could be
    // captured/aliased past the iteration edge without a value-semantic store the
    // analysis can see. Otherwise fall back to the strict pre-I4 rule.
    let widenable = body_is_widenable(body);
    // Iteration-local names admissible as rebind targets for THIS loop = the names
    // bound by a top-level `Let` of its body. Nested inner loops union in their own
    // top-level lets as the scan descends (see `instruction_heap_escapes`).
    let locals = top_level_let_names(body);
    !body
        .iter()
        .any(|i| instruction_heap_escapes(i, heap_aggs, &locals, widenable))
}

/// The names bound by a `Let` that is a **direct (top-level) statement** of `body`.
/// A top-level `Let` runs unconditionally at the head of every iteration before any
/// use of the name, so the binding is fresh each pass (never loop-carried) and — by
/// the frontend's per-loop scope clone — invisible after the loop. Lets nested
/// inside an `if`/`match`/`try`/inner loop are intentionally NOT included here; an
/// inner loop contributes its own top-level lets via the accumulated set instead.
fn top_level_let_names(body: &[BytecodeInstruction]) -> std::collections::HashSet<String> {
    body.iter()
        .filter_map(|stmt| match stmt {
            BytecodeInstruction::Let { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Whether `instruction` stores a heap value that can survive the iteration, given
/// the accumulated set of iteration-`locals` admissible as rebind targets and
/// whether the enclosing loop body is `widenable`. Recurses through nested control
/// flow; a nested inner loop unions its own top-level lets into `locals` for the
/// descent (a name local to the inner loop dies at the inner edge, hence also within
/// the outer iteration).
fn instruction_heap_escapes(
    instruction: &BytecodeInstruction,
    heap_aggs: &std::collections::HashSet<String>,
    locals: &std::collections::HashSet<String>,
    widenable: bool,
) -> bool {
    match instruction {
        // A store of a heap value can outlive the iteration UNLESS it is a
        // whole-variable rebind of a provably iteration-local binding (widenable
        // body, empty path, target in `locals`). A heap-CARRYING aggregate (a struct
        // with a `string` field, an `option<string>`/user enum with a heap payload)
        // counts as a heap store too. `total = total + len(s)` stores an `i64`
        // (scalar) and never reaches the heap branch.
        BytecodeInstruction::Assign {
            name, path, value, ..
        } => {
            if !type_is_heap(&value.ty, heap_aggs) {
                return false;
            }
            !(widenable && path.is_empty() && locals.contains(name))
        }
        // `Throw`/`Return` of a heap value always escapes (an arena function returns a
        // scalar, so a heap `Return` cannot actually occur, but it is denied for
        // safety). These never widen — they leave the iteration by construction.
        BytecodeInstruction::Throw { value, .. } | BytecodeInstruction::Return(Some(value)) => {
            type_is_heap(&value.ty, heap_aggs)
        }
        // A `Let` binds a fresh iteration-local (dies each iteration); its value does
        // not escape. Break/Continue/Return(None)/Expr/Asm carry no surviving store.
        BytecodeInstruction::Let { .. }
        | BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Expr(_)
        | BytecodeInstruction::Asm { .. } => false,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            // A nested non-loop block sees the SAME iteration-locals (a top-level let
            // of the loop body is visible inside its `if`/`match`/`try`).
            branches
                .iter()
                .any(|b| body_heap_escapes(&b.body, heap_aggs, locals, widenable))
                || body_heap_escapes(else_body, heap_aggs, locals, widenable)
        }
        BytecodeInstruction::While { body, .. }
        | BytecodeInstruction::For { body, .. }
        | BytecodeInstruction::Loop { body, .. } => {
            // A nested inner loop: its own top-level lets are ALSO iteration-local
            // w.r.t. the loop being confined (they die at the inner edge, which is
            // within one outer iteration), so union them in for the descent. The
            // inner loop's OWN confinement is judged separately by its own
            // `loop_body_confines_heap` call.
            let mut nested = locals.clone();
            nested.extend(top_level_let_names(body));
            body_heap_escapes(body, heap_aggs, &nested, widenable)
        }
        BytecodeInstruction::Match { arms, .. } => arms
            .iter()
            .any(|arm| body_heap_escapes(&arm.body, heap_aggs, locals, widenable)),
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => {
            body_heap_escapes(body, heap_aggs, locals, widenable)
                || body_heap_escapes(catch_body, heap_aggs, locals, widenable)
        }
    }
}

fn body_heap_escapes(
    body: &[BytecodeInstruction],
    heap_aggs: &std::collections::HashSet<String>,
    locals: &std::collections::HashSet<String>,
    widenable: bool,
) -> bool {
    body.iter()
        .any(|i| instruction_heap_escapes(i, heap_aggs, locals, widenable))
}

// -- Widenability: closures, raw pointers, and inline asm disable the widening -----
//
// The target-aware rebind admission is sound only against value-semantic stores. A
// closure literal can CAPTURE an iteration-local (the capture block may outlive the
// iteration); a raw pointer (`addr_of`/`alloc`/`ptr_*`, all `ptr<…>`-typed) can
// ALIAS one; inline `asm` can read/write arbitrary memory. If any is present in the
// loop body we do not widen — the strict pre-I4 rule (any heap store escapes) is
// used, so these loops keep their exact prior classification. (Arena functions are
// already `alloc`-free via criterion (5); the `ptr<…>` scan re-catches `alloc`'s
// `ptr` result regardless, keeping this module self-contained.)

/// Whether `body` is safe to apply the target-aware widening to: no closure literal,
/// no `ptr<…>`-typed expression, and no inline `asm`, anywhere within it (recursing
/// through nested control flow and inner loops).
fn body_is_widenable(body: &[BytecodeInstruction]) -> bool {
    !body.iter().any(instruction_blocks_widening)
}

fn instruction_blocks_widening(instruction: &BytecodeInstruction) -> bool {
    match instruction {
        // Inline assembly can touch arbitrary memory — never widen across it.
        BytecodeInstruction::Asm { .. } => true,
        BytecodeInstruction::Let { value, .. }
        | BytecodeInstruction::Assign { value, .. }
        | BytecodeInstruction::Return(Some(value))
        | BytecodeInstruction::Expr(value)
        | BytecodeInstruction::Throw { value, .. } => expr_blocks_widening(value),
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_) => false,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches
                .iter()
                .any(|b| expr_blocks_widening(&b.condition) || body_has_widening_blocker(&b.body))
                || body_has_widening_blocker(else_body)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_blocks_widening(condition) || body_has_widening_blocker(body),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_blocks_widening(start)
                || expr_blocks_widening(end)
                || step.as_ref().is_some_and(expr_blocks_widening)
                || body_has_widening_blocker(body)
        }
        BytecodeInstruction::Loop { body, .. } => body_has_widening_blocker(body),
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            expr_blocks_widening(scrutinee)
                || arms.iter().any(|arm| body_has_widening_blocker(&arm.body))
        }
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => body_has_widening_blocker(body) || body_has_widening_blocker(catch_body),
    }
}

fn body_has_widening_blocker(body: &[BytecodeInstruction]) -> bool {
    body.iter().any(instruction_blocks_widening)
}

/// Whether `expr` (or any sub-expression) is a closure literal or carries a raw
/// pointer. Pointer involvement is detected by TYPE — any node typed `ptr<…>`/`ptr`
/// — which catches an `addr_of`/`alloc` result, a `ptr_read`/`ptr_write`/`ptr_offset`
/// argument, and every other raw-pointer builtin uniformly without a fragile name
/// list.
fn expr_blocks_widening(expr: &BytecodeExpr) -> bool {
    if is_pointer_type(&expr.ty) {
        return true;
    }
    match &expr.kind {
        BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_blocks_widening(left) || expr_blocks_widening(right)
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            expr_blocks_widening(expr)
        }
        BytecodeExprKind::Call { args, .. } => args.iter().any(expr_blocks_widening),
        BytecodeExprKind::Array(elements) => elements.iter().any(expr_blocks_widening),
        BytecodeExprKind::Index { target, index } => {
            expr_blocks_widening(target) || expr_blocks_widening(index)
        }
        BytecodeExprKind::Field { target, .. } => expr_blocks_widening(target),
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_) => false,
    }
}

/// Whether a type is a raw pointer (`ptr<T>`, or the bare `ptr` spelling).
fn is_pointer_type(ty: &TypeRef) -> bool {
    ty.name == "ptr" || ty.name.starts_with("ptr<")
}
