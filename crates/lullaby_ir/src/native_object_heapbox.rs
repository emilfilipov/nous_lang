//! Native backend: the **interim heap-box** builtins `alloc` / `dealloc`. Sees the
//! parent's items via `use super::*`.
//!
//! # What `alloc` actually is (it is NOT a byte allocator)
//!
//! The name misleads, and the roadmap/diagnostic text around it has been written as
//! though `alloc(n)` reserved `n` bytes. It does not. The type checker types
//! `alloc(v)` as `ptr_{typeof v}` (`semantics_checker_calls.rs`), and the
//! interpreters implement it as
//!
//! ```text
//! self.heap.push(Some(value));        // one cell, holding `value`
//! Ok(Value::Ptr(self.heap.len() - 1)) // the cell's INDEX
//! ```
//!
//! So `alloc(8)` is a **box holding the value 8** — one cell — and `ptr_read` of it
//! yields `8`, not uninitialized 8-byte storage. It is `box(v)`, not `malloc(n)`.
//!
//! Native lowering therefore allocates **one 8-byte cell** through the shared
//! bump/RC allocator (`__lullaby_alloc`, which carries the heap-exhaustion `ud2`
//! guard) and stores the initializer into it, returning the cell's real machine
//! address. `ptr_read`/`ptr_write` through that address reuse the existing
//! raw-pointer surface unchanged: `raw_pointee_name` already strips the legacy
//! `ptr_T` spelling alongside `ptr<T>`, so an `alloc`-derived pointer flows through
//! `native_object_rawptr.rs` with no new load/store path.
//!
//! # Default-deny scope
//!
//! Only an **8-byte cell** pointee is lowered (`is_addressable_word_type`:
//! `i64`/`u64`/`isize`/`usize`/`ptr<T>`), where the Lullaby normalized cell and the
//! C width coincide, so the store `alloc` emits and the load `ptr_read` emits agree
//! bit-for-bit. `alloc("s")` (`ptr_string`), `alloc(true)` (`ptr_bool`),
//! `alloc(1.5)` (`ptr_f64`) and a narrow `alloc(to_i32(x))` (`ptr_i32`) each skip
//! cleanly (`L0339`) rather than guess a representation.
//!
//! # Why `dealloc` is NOT lowered (it skips cleanly)
//!
//! `dealloc` deliberately has **no** native lowering. Every candidate is a
//! correctness regression against the interpreters:
//!
//! * **`rc_free` (return the block to the free list).** The interpreters' `dealloc`
//!   sets the cell to `None`, so a later read is a *detected* error (`L0406`
//!   "invalid pointer"). Natively the block would be readable free-list memory —
//!   a **detected error would become a silent wrong answer** — and a double free
//!   would push the same block onto the LIFO free list twice, making the list
//!   cyclic so two later allocations alias the same cell (silent corruption) where
//!   the interpreters cleanly raise `L0406`.
//! * **A no-op.** Then `ptr_read` after `dealloc` returns the value natively while
//!   the interpreters raise `L0406` — the same divergence class.
//!
//! The `L0350` "used after it was freed" check still does NOT make `rc_free` safe.
//! It now tracks **direct copies** — `let p = alloc(5)  let q = p  dealloc(p)
//! ptr_read(q)` and the matching double free are rejected statically
//! (`semantics_lifetime_alias.rs`) — but it is copy tracking, not alias analysis.
//! These still compile and reach the backend, failing only at interpreter run time
//! with `L0406`:
//!
//! ```text
//! fn id p ptr_i64 -> ptr_i64 = p                            # alias via a call
//! let p = alloc(5)   let q = id(p)   dealloc(p)   ptr_read(q)
//! let p = alloc(5)   s.field = p     dealloc(p)   ptr_read(s.field)   # via an aggregate
//! ```
//!
//! So the divergence class is narrowed, not eliminated: one untracked alias is all
//! `rc_free` needs to turn a detected `L0406` into silent corruption.
//!
//! A faithful native `dealloc` needs a per-block validity tombstone plus a check on
//! every raw read — and that check is not implementable on the shared raw-pointer
//! read path, which also serves `addr_of` and `int_to_ptr` (MMIO) pointers that
//! have no allocator header at all. So a function using `dealloc` skips cleanly and
//! runs on the interpreters, where `L0406` is raised correctly. Skipping loses
//! nothing measurable: the interpreters never reuse a freed cell either
//! (`builtin_alloc` always pushes), so `dealloc` reclaims no interpreter memory.
//!
//! # Reclamation: what actually happens to an `alloc`'d block natively
//!
//! Nothing frees it. An `alloc`'d block is manually managed, so no `rc_dec` drop
//! glue is emitted for it (drop glue covers `string`/`list`/`map` records only),
//! and `alloc_defeats_arena` (below) keeps every `alloc`-using function off the
//! arena path, so no bump rewind reclaims it either. The block lives until the
//! process exits — bounded by the 1 MiB region, whose exhaustion is the allocator's
//! defined `ud2` trap, never a silent overrun. That **matches the interpreters**,
//! whose `heap: Vec<Option<Value>>` also grows monotonically and never reuses a
//! cell.
//!
//! # The arena hazard this module defuses
//!
//! `alloc` is invisible to the arena escape analysis: `type_is_directly_heap` does
//! not include `ptr_*`, and `expr_touches_heap` on a `Call { name: "alloc" }` only
//! inspects the arguments — so `let p = alloc(0)` registers as *not touching the
//! heap*. Without a gate, this is a real **use-after-free**:
//!
//! ```text
//! fn f -> i64
//!     unsafe
//!         let mut q = alloc(0)
//!         for i from 0 to 10
//!             let s string = to_string(i)   # a heap touch -> the loop is "heap"
//!             q = alloc(i)                  # `ptr_i64` is not a heap type ->
//!                                           # not counted as an escape
//!         ptr_read(q)                       # reads a rewound block
//! ```
//!
//! The loop looks heap-touching (the `string`) AND confined (the only store is a
//! `ptr_i64`, which `type_is_heap` says is not heap), so stage 2 gives it a
//! per-iteration sub-region and rewinds the bump pointer at the iteration edge —
//! reclaiming `q`'s cell while `q` still points at it.
//!
//! Rather than teach the escape analysis to track raw-pointer provenance (which
//! would have to see through `ptr_cast`/`int_to_ptr` and is not soundly decidable
//! here), this takes the **conservative exclusion** the arena rules are already
//! built around: a function whose body contains any `alloc` is never arena-eligible
//! (see [`alloc_defeats_arena`], applied in `arena_eligible_functions`). It then
//! stays on the RC / free-list path, where nothing reclaims its cells, so both the
//! loop-edge rewind and the return-edge rewind are impossible. This costs only the
//! arena optimization for such a function; correctness and codegen are otherwise
//! unchanged.
//!
//! **The control experiment for this hazard is gate-removed-on-this-branch, NOT the
//! base commit.** Before `alloc` had native codegen the whole leaf simply skipped
//! (`L0339`), so the miscompile is one this module *introduces* and then defuses —
//! measured at native `92` vs the interpreters' `2116` with `alloc_defeats_arena`
//! disabled. Reproducing it at the base commit is not possible and is the wrong
//! experiment.
//!
//! # The `ptr_cast` laundering route (why the identity gate covers it)
//!
//! `refuse_legacy_box_pointer` in `native_object_rawptr.rs` keys on the *spelling*
//! `ptr_T`, and `ptr_cast` used to be free to CHANGE that spelling: it derived its
//! result type from the caller's annotation, not from the operand, so
//! `let q ptr<i64> = ptr_cast(p)` turned a box into a `ptr<i64>` and `ptr_offset(q, 1)`
//! would then stride 8 bytes past the one-cell payload into the NEXT block's `[size]`
//! header — the word the allocator's free-list scan reads — so a write through it
//! corrupts allocator metadata. **`check_ptr_cast` now takes the result's model from
//! the operand**, so that source is `L0303` at the frontend; `ptr_cast` stays gated on
//! a `ptr_T` operand as defense in depth, still reachable via the model-preserving
//! identity cast `let q = ptr_cast(p)`.
//!
//! **The gate is NOT complete, and the earlier claim that it was is retracted.** It
//! rested on `ptr_cast` being "the only builtin whose result type ignores its operand",
//! which a follow-up audit disproved: `int_to_ptr` and `arena_alloc` carried the same
//! annotation-driven pattern. `arena_alloc` is now filtered to the modern spelling;
//! **`int_to_ptr` is irreducibly annotation-governed** (an `i64` carries no provenance,
//! so neither model is derivable, and both round trips are fixture-pinned), so a
//! `ptr_T` is an `unsafe` assertion that may be false. The gate is also a prefix test
//! on the OUTER type name, so a box model nested in a pointee (`ptr<ptr_i64>`) is
//! invisible to it. It guards the cases it names and nothing more. See
//! `refuse_legacy_box_pointer`'s "What this gate does NOT do" and
//! `semantics_raw_ptr.rs`'s "What is, and is not, a whole-program property".

use super::*;

/// The heap-box builtin names this module owns.
pub(crate) const ALLOC_BUILTIN: &str = "alloc";
pub(crate) const DEALLOC_BUILTIN: &str = "dealloc";

/// Whether `name` is an interim heap-box builtin. Used by the call dispatcher so
/// these names never fall through to the unknown-function arm as if they were user
/// calls.
pub(crate) fn is_heap_box_builtin(name: &str) -> bool {
    matches!(name, ALLOC_BUILTIN | DEALLOC_BUILTIN)
}

/// Lower an interim heap-box builtin call. Returns `None` when `name` is not one,
/// so the caller falls through to its other dispatch arms.
///
/// `expr_ty` is the *call's* own type — `ptr_T` for `alloc`, `void` for `dealloc`.
pub(crate) fn lower_heap_box_call(
    ctx: &mut NativeCtx,
    name: &str,
    args: &[BytecodeExpr],
    expr_ty: &TypeRef,
    code: &mut Vec<u8>,
) -> Option<Result<(), String>> {
    if !is_heap_box_builtin(name) {
        return None;
    }
    Some(match name {
        ALLOC_BUILTIN => lower_alloc(ctx, args, expr_ty, code),
        // Skips cleanly — see the module docs. This is a deliberate, permanent-for-now
        // refusal, not an unimplemented case: every lowering of `dealloc` available on
        // this heap turns an interpreter-detected error into a silent wrong answer or
        // silent heap corruption.
        _ => Err(
            "`dealloc` is not lowered natively: the interpreters invalidate the freed cell \
             and DETECT a later use or a double free (`L0406`), which the native bump/RC \
             heap cannot reproduce — returning the block to the free list would make a \
             use-after-free read free-list memory silently and a double free alias two \
             live allocations, and a no-op would make a use-after-free succeed. A \
             function using `dealloc` runs on the interpreters, where the error is \
             raised correctly"
                .to_string(),
        ),
    })
}

/// `alloc(v) -> ptr_T`: allocate one 8-byte cell through the shared bump/RC
/// allocator and store `v` into it, leaving the cell's real address in `rax`.
///
/// The initializer is evaluated **before** the allocation, matching the
/// interpreters (`eval` the argument, then `heap.push`), so a call or a nested
/// `alloc` inside `v` is staged in the same order.
fn lower_alloc(
    ctx: &mut NativeCtx,
    args: &[BytecodeExpr],
    expr_ty: &TypeRef,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let [value] = args else {
        return Err("`alloc` takes exactly one argument".to_string());
    };
    // The call must type as `ptr_T`/`ptr<T>` (the checker produces the legacy
    // `ptr_T` spelling); `T` is the boxed value's own type.
    let pointee = raw_pointee_name(&expr_ty.name).ok_or_else(|| {
        format!(
            "`alloc` must type as a pointer on the native backend, found `{}`",
            expr_ty.name
        )
    })?;
    // Default-deny: only an 8-byte cell, where the Lullaby normalized cell width and
    // the C width coincide, so the store here and the `ptr_read` load agree exactly.
    // A `string`/`bool`/`char`/float/narrow-integer box skips cleanly.
    if !is_addressable_word_type(pointee) {
        return Err(format!(
            "`alloc` of a `{pointee}` value is not lowered natively: the native heap box \
             holds an 8-byte cell (`i64`/`u64`/`isize`/`usize`/`ptr<T>`) only, where the \
             normalized cell width matches the C width a `ptr_read`/`ptr_write` through \
             it uses"
        ));
    }
    // Defensive agreement between the boxed value's type and the checker-inferred
    // pointee: `alloc(v)` must type as `ptr_{typeof v}`. A mismatch means an
    // assumption here no longer holds, so skip rather than store a word whose reader
    // would use a different width.
    if pointee != value.ty.name {
        return Err(format!(
            "`alloc` of a `{}` value typed as `{}` is not lowered natively (the pointee \
             must be the boxed value's own type)",
            value.ty.name, expr_ty.name
        ));
    }

    // Evaluate the initializer first (interpreter order), and park it in a scratch
    // slot rather than on the stack: `__lullaby_alloc` is a call, and the `push`/`pop`
    // idiom the binary ops use would leave `rsp` misaligned at the call site.
    let saved_scratch = ctx.scratch_next;
    let value_slot = ctx.alloc_scratch(1);
    lower_native_expr(ctx, value, code)?; // v -> rax
    store_local(code, value_slot);

    // rcx = 8 (one cell) ; call __lullaby_alloc -> rax = the cell's address.
    emit_mov_rcx_imm(code, 8);
    emit_call_symbol(ctx, HEAP_ALLOC_SYMBOL, code);

    // [rax] = v. The cell is a fresh 8-byte block, so this initializing store is the
    // whole box; `rax` already holds the address the call returns.
    emit_mov_rcx_from_slot(code, value_slot); // rcx = v
    code.extend_from_slice(&[0x48, 0x89, 0x08]); // mov [rax], rcx

    ctx.scratch_next = saved_scratch;
    Ok(())
}

// -- The arena gate ----------------------------------------------------------

/// Whether `instrs` contain any `alloc` call. A function that boxes a value on the
/// heap is excluded from arena eligibility: an `alloc`'d cell is manually managed
/// and invisible to the escape analysis, so an arena rewind at a loop edge or a
/// return edge could reclaim a cell a live pointer still names (see the module docs
/// for the exact use-after-free shape).
///
/// The gate is deliberately whole-function and coarse rather than per-allocation,
/// exactly like [`body_takes_address`]'s register-promotion gate: it cannot be
/// defeated by an aliasing pattern a finer analysis failed to see through, and it
/// only ever DENIES an optimization — it never changes emitted semantics.
///
/// `closures` is the module's closure table, so a `Closure { id }` literal is
/// resolved and its BODY scanned too. A closure body is a separate expression tree
/// reachable only by id, so without this an `alloc` inside one would be invisible to
/// the gate. (Today the Stage-1 closure rules make that shape hard to reach, but this
/// gate guards a demonstrated miscompile, so it does not rely on a *different*
/// subsystem's restrictions staying as they are.)
///
/// Scanning only the function's own body (plus its closures) is sound because arena
/// eligibility independently requires the function to be a **leaf w.r.t. user code**
/// (condition (3) of [`arena_eligible_functions`]): it calls no user or `extern`
/// function, so an `alloc` cannot reach it from a callee.
pub(crate) fn alloc_defeats_arena(
    instrs: &[BytecodeInstruction],
    closures: &[BytecodeClosureDef],
) -> bool {
    instrs.iter().any(|i| instr_allocates_box(i, closures))
}

fn instr_allocates_box(instr: &BytecodeInstruction, cls: &[BytecodeClosureDef]) -> bool {
    match instr {
        BytecodeInstruction::Let { value, .. } | BytecodeInstruction::Assign { value, .. } => {
            expr_allocates_box(value, cls)
        }
        BytecodeInstruction::Return(Some(expr))
        | BytecodeInstruction::Expr(expr)
        | BytecodeInstruction::Throw { value: expr, .. } => expr_allocates_box(expr, cls),
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Asm { .. } => false,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches
                .iter()
                .any(|b| expr_allocates_box(&b.condition, cls) || alloc_defeats_arena(&b.body, cls))
                || alloc_defeats_arena(else_body, cls)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_allocates_box(condition, cls) || alloc_defeats_arena(body, cls),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_allocates_box(start, cls)
                || expr_allocates_box(end, cls)
                || step.as_ref().is_some_and(|s| expr_allocates_box(s, cls))
                || alloc_defeats_arena(body, cls)
        }
        BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::RegionBlock { body, .. } => {
            alloc_defeats_arena(body, cls)
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            expr_allocates_box(scrutinee, cls)
                || arms.iter().any(|a| alloc_defeats_arena(&a.body, cls))
        }
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => alloc_defeats_arena(body, cls) || alloc_defeats_arena(catch_body, cls),
    }
}

fn expr_allocates_box(expr: &BytecodeExpr, cls: &[BytecodeClosureDef]) -> bool {
    expr_allocates_box_at(expr, cls, 0)
}

/// `depth` bounds closure-into-closure resolution. Ids are assigned in parse order
/// and a body can only contain literals nested *within* it, so a cycle cannot occur
/// in a well-formed module; the bound makes a malformed one terminate (returning
/// `true` — conservative: it only denies the arena) instead of recursing forever.
fn expr_allocates_box_at(expr: &BytecodeExpr, cls: &[BytecodeClosureDef], depth: u32) -> bool {
    const MAX_CLOSURE_DEPTH: u32 = 16;
    if depth > MAX_CLOSURE_DEPTH {
        return true;
    }
    let go = |e: &BytecodeExpr| expr_allocates_box_at(e, cls, depth);
    match &expr.kind {
        BytecodeExprKind::Call { name, args } => name == ALLOC_BUILTIN || args.iter().any(go),
        BytecodeExprKind::Unary { expr: inner, .. } | BytecodeExprKind::Await { expr: inner } => {
            go(inner)
        }
        BytecodeExprKind::Binary { left, right, .. } => go(left) || go(right),
        BytecodeExprKind::Array(values) => values.iter().any(go),
        BytecodeExprKind::Index { target, index } => go(target) || go(index),
        BytecodeExprKind::Field { target, .. } => go(target),
        // A closure literal carries only its parse-order `id`; the body lives in the
        // module's closure table. Resolve and scan it, so an `alloc` inside a closure
        // is not invisible to the gate.
        BytecodeExprKind::Closure { id } => cls
            .iter()
            .find(|c| c.id == *id)
            .is_some_and(|c| expr_allocates_box_at(&c.body, cls, depth + 1)),
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Variable(_) => false,
    }
}
