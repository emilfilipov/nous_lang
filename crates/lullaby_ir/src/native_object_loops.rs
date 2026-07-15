//! Native backend: `while`/`loop`/`for` statement lowering plus RC/arena scope-based
//! drop insertion — borrow-only escape analysis and loop-body reclamation. Split out
//! of native_object_stmt.rs; shared items via `use super::super::*`.
use super::super::*;

pub(crate) fn lower_native_while(
    ctx: &mut NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    // ILP fast path: a promoted counting-sum loop folds a block of iterations per
    // step, breaking the serial `acc += i` dependency chain. Any deviation from
    // the exact shape falls through to the general loop lowering below.
    if let Some(plan) = detect_sum_reduction(ctx, condition, body) {
        emit_sum_reduction(ctx, &plan, code)?;
        return Ok(());
    }
    // Affine reduction: `acc += a*i + b`. The block sum folds K iterations into a
    // single `imul`+`add` (one dependent op per K iterations), beating C's
    // per-element loop. Tried before the multi-accumulator because the closed
    // form is strictly faster when the addend is affine.
    if let Some(plan) = detect_affine_reduction(ctx, condition, body) {
        emit_affine_reduction(ctx, &plan, code)?;
        return Ok(());
    }
    // Quadratic reduction `acc += c2*i² + c1*i + c0`: the closed form uses
    // `S2 = sum(i²)` (Faulhaber, via g(m) and the modular inverse of 3). O(1),
    // one degree above affine.
    if let Some(plan) = detect_quadratic_reduction(ctx, condition, body) {
        emit_quadratic_reduction(ctx, &plan, code)?;
        return Ok(());
    }
    // General multi-accumulator reduction: `acc = acc + EXPR` where `EXPR` is a
    // pure polynomial in the counter (sum-of-squares, weighted sums, …). Four
    // independent accumulators break the serial `acc` dependency chain that made
    // the naive scalar loop ~2.5× slower than C.
    if let Some(plan) = detect_general_reduction(ctx, condition, body) {
        emit_general_reduction(ctx, &plan, code)?;
        return Ok(());
    }

    // Arena stage-2 sub-region: if this loop confines its heap to the iteration,
    // save the entry bump pointer now (before `top:`) so each iteration edge can
    // rewind to it. Saved once; the mark is invariant because nothing escapes.
    let arena_reset_mark = arena_loop_reset_mark(
        ctx,
        expr_touches_heap(condition, &ctx.heap_aggregates)
            || body_touches_heap(body, &ctx.heap_aggregates),
        body,
        loops.len(),
    );
    if let Some(mark) = arena_reset_mark {
        emit_arena_loop_save(ctx, mark, code);
    }

    let top = code.len();
    // Fused `cmp`+conditional-jump for an i64 comparison; else the generic
    // "evaluate to 0/1 in rax, `test rax,rax`, `jz`" path. Both jump to `end`
    // when the loop condition is false.
    let exit_site = match try_emit_fused_i64_condition_branch(ctx, condition, code)? {
        Some(site) => site,
        None => {
            lower_native_expr(ctx, condition, code)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.extend_from_slice(&[0x0F, 0x84]); // jz end (patched)
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            site
        }
    };

    loops.push(NativeLoop {
        continue_target: Some(top),
        continue_sites: Vec::new(),
        break_sites: Vec::new(),
        live_drops: Vec::new(),
        arena_reset_mark,
    });
    lower_loop_body_with_drops(ctx, body, code, loops)?;
    // Reclaim per-iteration owned string temporaries on the fallthrough back-edge
    // (RC drop insertion). `break`/`continue` now drop the live owned locals on their
    // own edges (see `lower_loop_body_with_drops`); each dynamic iteration takes
    // exactly one of {fallthrough, break, continue}, so no value is dropped twice.
    emit_loop_body_string_drops(ctx, body, code)?;
    // Arena stage-2: rewind the sub-region on the fallthrough back-edge.
    if let Some(mark) = arena_reset_mark {
        emit_arena_loop_rewind(ctx, mark, code);
    }
    let loop_ctx = loops.pop().expect("loop pushed");

    emit_jmp_to(code, top); // jmp top

    let end = code.len();
    patch_rel32_to(code, exit_site, end);
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

/// Lower an infinite `loop`: top: body; `jmp top`; end:. `break` exits.
pub(crate) fn lower_native_loop(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    // Arena stage-2 sub-region: save the entry bump pointer before `top:` when the
    // loop confines its heap to the iteration.
    let arena_reset_mark = arena_loop_reset_mark(
        ctx,
        body_touches_heap(body, &ctx.heap_aggregates),
        body,
        loops.len(),
    );
    if let Some(mark) = arena_reset_mark {
        emit_arena_loop_save(ctx, mark, code);
    }

    let top = code.len();
    loops.push(NativeLoop {
        continue_target: Some(top),
        continue_sites: Vec::new(),
        break_sites: Vec::new(),
        live_drops: Vec::new(),
        arena_reset_mark,
    });
    lower_loop_body_with_drops(ctx, body, code, loops)?;
    emit_loop_body_string_drops(ctx, body, code)?;
    // Arena stage-2: rewind the sub-region on the fallthrough back-edge.
    if let Some(mark) = arena_reset_mark {
        emit_arena_loop_rewind(ctx, mark, code);
    }
    let loop_ctx = loops.pop().expect("loop pushed");

    emit_jmp_to(code, top);

    let end = code.len();
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

// -- Scope-based drop insertion (RC memory model, stage 2) --------------------
//
// Reference-counted heap blocks are reclaimed by inserting `rc_dec` (free-at-zero)
// at scope-exit. The FIRST increment targets the highest-value, provably-safe
// case: a `string` local declared directly in a LOOP body that is uniquely owned
// (a fresh allocation, never reassigned) and only ever BORROWED (used solely as
// the argument of `len`). Such a local is dead at the end of each iteration, so a
// single `rc_dec` on the fallthrough loop-body edge frees it — reclaiming what
// would otherwise leak and, for a long loop, exhaust the fixed heap region.
//
// Everything here is DEFAULT-DENY: any use that could alias, store, return, or
// pass ownership elsewhere disqualifies the local, which is then simply not
// dropped (it leaks exactly as before — never double-freed). Early-exit edges
// (`return`/`break`/`continue`) skip the fallthrough drop and leak on that path,
// which is safe; only the fallthrough (loop back-edge) frees, exactly once.

/// Whether `value` is a freshly-allocated `string` record this scope uniquely owns:
/// a string literal (materialized into a new record), a `+` concatenation, or a
/// `substring`/`trim`/`repeat` call (each always allocates a fresh record in the
/// native backend), or `to_string` of a non-string scalar. NOT a bare variable
/// (an alias), a container read, or a user-function result (unknown ownership).
pub(crate) fn is_owning_string_alloc(value: &BytecodeExpr) -> bool {
    if value.ty.name != "string" {
        return false;
    }
    match &value.kind {
        BytecodeExprKind::String(_) => true,
        BytecodeExprKind::Binary {
            op: BinaryOp::Add, ..
        } => true,
        BytecodeExprKind::Call { name, args } => match name.as_str() {
            "substring" | "trim" | "repeat" => true,
            "to_string" => args.len() == 1 && args[0].ty.name != "string",
            _ => false,
        },
        _ => false,
    }
}

/// Whether `value` is a freshly-allocated `array<string>` (`list<string>`-layout)
/// this scope uniquely owns: the result of `split`/`words`. (A user-function call
/// or a bare variable is not — ownership is unknown / borrowed.)
pub(crate) fn is_owning_string_array(value: &BytecodeExpr) -> bool {
    heap_string_array_element(&value.ty).is_some()
        && matches!(&value.kind, BytecodeExprKind::Call { name, .. } if name == "split" || name == "words")
}

/// Whether every use of the heap local `name` within `expr` is a pure borrow. For a
/// `string` local (`allow_index == false`) the only borrow is the sole argument of
/// `len(name)`. For an `array<string>` local (`allow_index == true`) `len(name[i])`
/// — reading an element's length — is additionally allowed; a bare `name[i]` (which
/// would alias an element the block owns) is NOT, since the block-drop frees the
/// elements. Any other mention lets ownership escape, so `name` is not droppable.
pub(crate) fn string_local_borrow_only_expr(
    name: &str,
    expr: &BytecodeExpr,
    allow_index: bool,
) -> bool {
    match &expr.kind {
        BytecodeExprKind::Variable(v) => v != name,
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::Call { name: fname, args } => {
            if fname == "len" && args.len() == 1 {
                match &args[0].kind {
                    BytecodeExprKind::Variable(v) if v == name => true,
                    BytecodeExprKind::Index { target, index }
                        if allow_index
                            && matches!(&target.kind, BytecodeExprKind::Variable(v) if v == name) =>
                    {
                        // `len(name[i])`: the element is read for its length, not kept.
                        string_local_borrow_only_expr(name, index, allow_index)
                    }
                    _ => string_local_borrow_only_expr(name, &args[0], allow_index),
                }
            } else {
                args.iter()
                    .all(|a| string_local_borrow_only_expr(name, a, allow_index))
            }
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            string_local_borrow_only_expr(name, left, allow_index)
                && string_local_borrow_only_expr(name, right, allow_index)
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            string_local_borrow_only_expr(name, expr, allow_index)
        }
        BytecodeExprKind::Index { target, index } => {
            string_local_borrow_only_expr(name, target, allow_index)
                && string_local_borrow_only_expr(name, index, allow_index)
        }
        BytecodeExprKind::Field { target, .. } => {
            string_local_borrow_only_expr(name, target, allow_index)
        }
        BytecodeExprKind::Array(elems) => elems
            .iter()
            .all(|e| string_local_borrow_only_expr(name, e, allow_index)),
    }
}

/// Whether every use of `name` across `stmts` (recursing into nested blocks) is a
/// pure borrow, and `name` is never reassigned, shadowed, or rebound. Any
/// violation disqualifies the local from dropping.
pub(crate) fn string_local_borrow_only_stmts(
    name: &str,
    stmts: &[BytecodeInstruction],
    allow_index: bool,
) -> bool {
    stmts
        .iter()
        .all(|s| string_local_borrow_only_stmt(name, s, allow_index))
}

pub(crate) fn string_local_borrow_only_stmt(
    name: &str,
    stmt: &BytecodeInstruction,
    allow_index: bool,
) -> bool {
    match stmt {
        BytecodeInstruction::Let { name: n, value, .. } => {
            n != name && string_local_borrow_only_expr(name, value, allow_index)
        }
        BytecodeInstruction::Assign {
            name: n,
            path,
            value,
            ..
        } => {
            // Any assignment targeting `name` (a rebind, or a container mutation of
            // `name`) breaks the unique-ownership assumption.
            n != name
                && path.iter().all(|p| match p {
                    BytecodePlace::Index(e) => string_local_borrow_only_expr(name, e, allow_index),
                    BytecodePlace::Field(_) => true,
                })
                && string_local_borrow_only_expr(name, value, allow_index)
        }
        BytecodeInstruction::Return(Some(e)) | BytecodeInstruction::Expr(e) => {
            string_local_borrow_only_expr(name, e, allow_index)
        }
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Asm { .. } => true,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().all(|b| {
                string_local_borrow_only_expr(name, &b.condition, allow_index)
                    && string_local_borrow_only_stmts(name, &b.body, allow_index)
            }) && string_local_borrow_only_stmts(name, else_body, allow_index)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => {
            string_local_borrow_only_expr(name, condition, allow_index)
                && string_local_borrow_only_stmts(name, body, allow_index)
        }
        BytecodeInstruction::For {
            name: v,
            start,
            end,
            step,
            body,
            ..
        } => {
            v != name
                && string_local_borrow_only_expr(name, start, allow_index)
                && string_local_borrow_only_expr(name, end, allow_index)
                && step
                    .as_ref()
                    .is_none_or(|s| string_local_borrow_only_expr(name, s, allow_index))
                && string_local_borrow_only_stmts(name, body, allow_index)
        }
        BytecodeInstruction::Loop { body, .. } => {
            string_local_borrow_only_stmts(name, body, allow_index)
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            string_local_borrow_only_expr(name, scrutinee, allow_index)
                && arms.iter().all(|a| {
                    let binds = matches!(&a.pattern, BytecodeMatchPattern::Variant { bindings, .. }
                        if bindings.iter().any(|b| b == name));
                    !binds && string_local_borrow_only_stmts(name, &a.body, allow_index)
                })
        }
        BytecodeInstruction::Throw { value, .. } => {
            string_local_borrow_only_expr(name, value, allow_index)
        }
        BytecodeInstruction::Try {
            body,
            catch_name,
            catch_body,
            ..
        } => {
            catch_name != name
                && string_local_borrow_only_stmts(name, body, allow_index)
                && string_local_borrow_only_stmts(name, catch_body, allow_index)
        }
    }
}

// -- Heap-field aggregate (`struct` with `string` field) drop analysis -----------
//
// A stack `struct` local whose fields are scalars plus one or more immutable
// `string` fields owns those string records only when each was constructed from a
// FRESH (owning) string expression. If the local is provably uniquely owned and
// borrow-only within a loop body — used only via `len(r.Fstring)` header reads and
// scalar-field reads `r.Fscalar`, never copied / passed / returned / reassigned /
// field-mutated — then each of its owned string fields is dead at the iteration edge
// and can be reclaimed by an `rc_dec` per string field (the recursive drop-glue for
// a heap-field aggregate). This mirrors the plain `string`-local drop exactly, one
// `rc_dec` per heap field, so it composes with BOTH the RC/free-list path (the
// `rc_dec` frees the record) and the arena path (in arena mode `rc_free` no-ops and
// the bump rewind reclaims — no double-free, no leak either way).

/// Whether every use of the heap struct local `name` across `expr` is a pure borrow
/// that permits dropping its owned string fields. `string_fields` names the local's
/// `string` fields. `name` may appear ONLY as `len(name.F)` for a `string` field `F`
/// (a header read that never retains the pointer) or as `name.F` for a SCALAR field
/// `F` (a scalar read never aliases the heap). A bare `name` (copy/pass/return), a
/// bare `string`-field read not directly wrapped by `len`, or `name` used anywhere
/// else lets an owned string pointer escape, so the local is not droppable.
pub(crate) fn struct_field_borrow_only_expr(
    name: &str,
    string_fields: &[&str],
    expr: &BytecodeExpr,
) -> bool {
    match &expr.kind {
        // A bare mention of `name` (an alias, a copy, a call argument, a return
        // value) lets the whole struct — and thus its owned string fields — escape.
        BytecodeExprKind::Variable(v) => v != name,
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::Call { name: fname, args } => {
            // `len(name.Fstring)` is the one borrow that reads a string field's
            // header without retaining its pointer.
            if fname == "len"
                && args.len() == 1
                && let BytecodeExprKind::Field { target, field } = &args[0].kind
                && matches!(&target.kind, BytecodeExprKind::Variable(v) if v == name)
                && string_fields.contains(&field.as_str())
            {
                return true;
            }
            args.iter()
                .all(|a| struct_field_borrow_only_expr(name, string_fields, a))
        }
        BytecodeExprKind::Field { target, field } => {
            // A SCALAR field read `name.Fscalar` is safe (it never aliases the heap).
            // A `string`-field read reaching here (i.e. NOT the direct `len` arg
            // handled above) would retain the pointer, so it is rejected.
            if matches!(&target.kind, BytecodeExprKind::Variable(v) if v == name) {
                return !string_fields.contains(&field.as_str());
            }
            struct_field_borrow_only_expr(name, string_fields, target)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            struct_field_borrow_only_expr(name, string_fields, left)
                && struct_field_borrow_only_expr(name, string_fields, right)
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            struct_field_borrow_only_expr(name, string_fields, expr)
        }
        BytecodeExprKind::Index { target, index } => {
            struct_field_borrow_only_expr(name, string_fields, target)
                && struct_field_borrow_only_expr(name, string_fields, index)
        }
        BytecodeExprKind::Array(elems) => elems
            .iter()
            .all(|e| struct_field_borrow_only_expr(name, string_fields, e)),
    }
}

/// Whether every use of the struct local `name` across `stmts` (recursing into
/// nested blocks) is a pure borrow, and `name` is never reassigned, shadowed,
/// field-mutated, or rebound. Any violation disqualifies the local from dropping.
pub(crate) fn struct_field_borrow_only_stmts(
    name: &str,
    string_fields: &[&str],
    stmts: &[BytecodeInstruction],
) -> bool {
    stmts
        .iter()
        .all(|s| struct_field_borrow_only_stmt(name, string_fields, s))
}

pub(crate) fn struct_field_borrow_only_stmt(
    name: &str,
    string_fields: &[&str],
    stmt: &BytecodeInstruction,
) -> bool {
    match stmt {
        BytecodeInstruction::Let { name: n, value, .. } => {
            n != name && struct_field_borrow_only_expr(name, string_fields, value)
        }
        BytecodeInstruction::Assign {
            name: n,
            path,
            value,
            ..
        } => {
            // Any assignment targeting `name` (a rebind, or a field mutation of
            // `name`) breaks the unique-ownership assumption (a field mutation would
            // orphan a string field the drop set no longer tracks).
            n != name
                && path.iter().all(|p| match p {
                    BytecodePlace::Index(e) => {
                        struct_field_borrow_only_expr(name, string_fields, e)
                    }
                    BytecodePlace::Field(_) => true,
                })
                && struct_field_borrow_only_expr(name, string_fields, value)
        }
        BytecodeInstruction::Return(Some(e)) | BytecodeInstruction::Expr(e) => {
            struct_field_borrow_only_expr(name, string_fields, e)
        }
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Asm { .. } => true,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().all(|b| {
                struct_field_borrow_only_expr(name, string_fields, &b.condition)
                    && struct_field_borrow_only_stmts(name, string_fields, &b.body)
            }) && struct_field_borrow_only_stmts(name, string_fields, else_body)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => {
            struct_field_borrow_only_expr(name, string_fields, condition)
                && struct_field_borrow_only_stmts(name, string_fields, body)
        }
        BytecodeInstruction::For {
            name: v,
            start,
            end,
            step,
            body,
            ..
        } => {
            v != name
                && struct_field_borrow_only_expr(name, string_fields, start)
                && struct_field_borrow_only_expr(name, string_fields, end)
                && step
                    .as_ref()
                    .is_none_or(|s| struct_field_borrow_only_expr(name, string_fields, s))
                && struct_field_borrow_only_stmts(name, string_fields, body)
        }
        BytecodeInstruction::Loop { body, .. } => {
            struct_field_borrow_only_stmts(name, string_fields, body)
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            struct_field_borrow_only_expr(name, string_fields, scrutinee)
                && arms.iter().all(|a| {
                    let binds = matches!(&a.pattern, BytecodeMatchPattern::Variant { bindings, .. }
                        if bindings.iter().any(|b| b == name));
                    !binds && struct_field_borrow_only_stmts(name, string_fields, &a.body)
                })
        }
        BytecodeInstruction::Throw { value, .. } => {
            struct_field_borrow_only_expr(name, string_fields, value)
        }
        BytecodeInstruction::Try {
            body,
            catch_name,
            catch_body,
            ..
        } => {
            catch_name != name
                && struct_field_borrow_only_stmts(name, string_fields, body)
                && struct_field_borrow_only_stmts(name, string_fields, catch_body)
        }
    }
}

/// Whether `value` constructs a `struct` whose every `string` field is a fresh
/// (owning) string allocation, so this scope uniquely owns each string record and
/// may `rc_dec` it on scope exit. `string_field_words` lists the field-index of each
/// `string` field; the constructor's argument in that position must be an owning
/// string alloc (a literal, `+` concat, `to_string`, `substring`/`trim`/`repeat`) —
/// never a borrowed variable/field, whose pointer another scope still owns.
pub(crate) fn is_owning_struct_with_strings(
    value: &BytecodeExpr,
    struct_name: &str,
    string_field_words: &[usize],
) -> bool {
    let BytecodeExprKind::Call { name, args } = &value.kind else {
        return false;
    };
    if name != struct_name {
        return false;
    }
    string_field_words
        .iter()
        .all(|&w| w < args.len() && is_owning_string_alloc(&args[w]))
}

/// After lowering a loop `body`, emit a drop (free-at-zero) for each heap local
/// declared directly in `body` that is uniquely owned and only borrowed —
/// reclaiming the per-iteration allocation on the fallthrough back-edge. Handles
/// two cases: a `string` local (dropped by `rc_dec`), and an `array<string>` local
/// (a `split`/`words` result, dropped recursively by `__lullaby_drop_string_array`
/// — each element then the block). All are stack locals (a heap-using function is
/// never register-promoted, so the pointer is always in a stack slot).
pub(crate) fn emit_loop_body_string_drops(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
) -> Result<(), String> {
    for (_idx, slot, drop_symbol) in collect_loop_body_drops(ctx, body) {
        // mov rcx, [rbp - slot] ; call <drop_symbol>
        code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
        code.extend_from_slice(&(-slot).to_le_bytes());
        emit_call_symbol(ctx, drop_symbol, code);
    }
    Ok(())
}

/// Identify the uniquely-owned, borrow-only heap locals declared directly in a loop
/// `body` that are droppable at scope exit. Returns `(body index, frame slot,
/// drop-helper symbol)` for each — the SINGLE source of truth shared by the
/// fallthrough back-edge drop ([`emit_loop_body_string_drops`]) and the early-exit
/// (`break`/`continue`) drops ([`lower_loop_body_with_drops`]). Deriving both edge
/// families from one predicate guarantees the early-exit drop set is exactly the
/// fallthrough drop set, so a value dropped on an early edge is provably one that is
/// also uniquely owned and dead — never a spurious slot.
///
/// The two cases mirror the shipped drop coverage: a `string` local (fresh alloc,
/// borrow-only via `len(name)`; dropped by `rc_dec`) and an `array<string>` local (a
/// `split`/`words` result, borrow-only via `len(name)`/`len(name[i])`; dropped
/// recursively by `__lullaby_drop_string_array`). All are stack locals (a heap-using
/// function is never register-promoted, so the pointer is always in a stack slot).
pub(crate) fn collect_loop_body_drops(
    ctx: &NativeCtx,
    body: &[BytecodeInstruction],
) -> Vec<(usize, i32, &'static str)> {
    let mut drops = Vec::new();
    for (idx, stmt) in body.iter().enumerate() {
        let BytecodeInstruction::Let { name, value, .. } = stmt else {
            continue;
        };
        let Ok(local) = ctx.local(name) else {
            continue;
        };
        let slot = local.slot;
        // A `struct` local with `string` field(s): the recursive drop-glue for a
        // heap-field aggregate. Each owned string field is reclaimed by an `rc_dec`
        // (one per field) at the iteration edges — the same drop shape as a plain
        // `string` local, so it reuses the `(slot, RC_DEC_SYMBOL)` model with the
        // slot pointing at each string-field word. Requires: every string field
        // constructed from a FRESH owning alloc (so this scope uniquely owns each
        // record), and the local borrow-only afterward (used only via `len(r.F)` /
        // scalar-field reads, never copied / passed / returned / reassigned).
        if let NativeType::Struct {
            name: sname,
            fields,
        } = &local.ty
        {
            let mut word = 0i32;
            let mut string_slots: Vec<i32> = Vec::new();
            let mut string_names: Vec<&str> = Vec::new();
            let mut string_field_indices: Vec<usize> = Vec::new();
            for (index, (fname, fty)) in fields.iter().enumerate() {
                if matches!(fty, NativeType::String) {
                    string_slots.push(slot + word * 8);
                    string_names.push(fname.as_str());
                    string_field_indices.push(index);
                }
                word += fty.words() as i32;
            }
            if !string_slots.is_empty()
                && is_owning_struct_with_strings(value, sname, &string_field_indices)
                && struct_field_borrow_only_stmts(name, &string_names, &body[idx + 1..])
            {
                for field_slot in string_slots {
                    drops.push((idx, field_slot, RC_DEC_SYMBOL));
                }
            }
            continue;
        }
        // A plain `string` local: fresh alloc, borrow-only (only `len(name)`).
        let is_string = matches!(local.ty, NativeType::String);
        // An `array<string>` local: a `split`/`words` result, borrow-only
        // (`len(name)` / `len(name[i])`; a bare `name[i]` would alias an element).
        let is_string_array = matches!(&local.ty, NativeType::List { elem } if matches!(**elem, NativeType::String))
            && heap_string_array_element(&stmt_let_ty(stmt)).is_some();
        let drop_symbol = if is_string && is_owning_string_alloc(value) {
            RC_DEC_SYMBOL
        } else if is_string_array && is_owning_string_array(value) {
            DROP_STRING_ARRAY_SYMBOL
        } else {
            continue;
        };
        let allow_index = is_string_array;
        if !string_local_borrow_only_stmts(name, &body[idx + 1..], allow_index) {
            continue;
        }
        drops.push((idx, slot, drop_symbol));
    }
    drops
}

/// Emit `mov rcx, [rbp - slot]; call <symbol>` for each owned local — the drop-site
/// encoding shared by the fallthrough and early-exit edges. Identical bytes to the
/// fallthrough loop-body drop, so no new free-list-touching machine code is
/// introduced by the early-exit path.
pub(crate) fn emit_owned_local_drops(
    ctx: &mut NativeCtx,
    drops: &[(i32, &'static str)],
    code: &mut Vec<u8>,
) {
    for &(slot, symbol) in drops {
        code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
        code.extend_from_slice(&(-slot).to_le_bytes());
        emit_call_symbol(ctx, symbol, code);
    }
}

/// Lower a loop `body`, revealing each uniquely-owned droppable local into the
/// innermost loop's `live_drops` set the moment its `let` has been lowered. Because
/// the reveal happens AFTER lowering the declaring statement, a `break`/`continue`
/// lowered inside any later top-level statement sees exactly the owned locals whose
/// declaration textually precedes it — and never a slot whose `let` has not run
/// (those are added only once their statement is reached). Droppable `let`s are
/// always direct children of the loop body, so a `break`/`continue` located inside
/// top-level statement `j` is reached only after every earlier statement (including
/// each droppable `let` at index `< j`) has executed. This is what makes the
/// early-exit drop provably fire on a LIVE value exactly once per dynamic path.
pub(crate) fn lower_loop_body_with_drops(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    let drops = collect_loop_body_drops(ctx, body);
    for (j, stmt) in body.iter().enumerate() {
        lower_native_stmt(ctx, stmt, code, loops)?;
        // Reveal EVERY drop declared by statement `j` (a `struct` local with several
        // `string` fields contributes one `rc_dec` drop per field, all keyed to the
        // same declaring index) into the innermost loop's early-exit drop set.
        for &(_, slot, symbol) in drops.iter().filter(|(idx, _, _)| *idx == j) {
            if let Some(top) = loops.last_mut() {
                top.live_drops.push((slot, symbol));
            }
        }
    }
    Ok(())
}

/// The declared type of a `Let` instruction (its `ty` field). Used to distinguish a
/// heap `array<string>` local (which resolves to a `list<string>` NativeType) from a
/// genuine `list<string>` by the source spelling.
pub(crate) fn stmt_let_ty(stmt: &BytecodeInstruction) -> TypeRef {
    match stmt {
        BytecodeInstruction::Let { ty, .. } => ty.clone(),
        _ => TypeRef::new(""),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_native_for(
    ctx: &mut NativeCtx,
    name: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    // Close-form an affine `for` reduction (`for i from a to b { acc += a*i+b }`)
    // to O(1) — no loop. A distinct shape from the array reductions below (an
    // `a[i]` addend is not affine in the counter, so it is rejected here and
    // falls through).
    if let Some(plan) = detect_for_affine_reduction(ctx, name, start, end, step, body) {
        return emit_for_affine_reduction(ctx, &plan, code);
    }
    // Same for a quadratic `for` addend (`acc += c2*i²+c1*i+c0`) via the S2 = Σi²
    // Faulhaber closed form.
    if let Some(plan) = detect_for_quadratic_reduction(ctx, name, start, end, step, body) {
        return emit_for_quadratic_reduction(ctx, &plan, code);
    }
    // Auto-vectorize a recognized `for i from S to E: acc += a[i]` sum reduction
    // over an `array<i64>` into an SSE2 packed loop. Anything that does not match
    // the exact shape falls through to the scalar lowering below, so correctness
    // never depends on the pattern matcher.
    if let Some(reduction) = detect_reduction(ctx, name, step, body) {
        return lower_native_vectorized_reduction(ctx, name, start, end, &reduction, code);
    }
    // Auto-vectorize `for i: acc = max(acc, a[i])` / `min(...)` via SSE4.2 with a
    // runtime CPUID gate (scalar fallback on older CPUs). Same matcher discipline.
    if let Some(minmax) = detect_minmax_reduction(ctx, name, step, body) {
        return lower_native_minmax_reduction(ctx, name, start, end, &minmax, code);
    }
    // Auto-vectorize f64 sum/dot reductions (`acc += a[i]` / `acc += a[i]*b[i]`)
    // ONLY under `--fast-math`: a 2-lane packed accumulator reorders the additions
    // (float `+` is not associative), so the result can differ from the scalar fold
    // in the last ULP. Off by default -> the reduction runs scalar and stays
    // bit-exact with the interpreters.
    if ctx.fast_math
        && let Some(red) = detect_f64_reduction(ctx, name, step, body)
    {
        return lower_native_f64_reduction(ctx, name, start, end, &red, code);
    }
    // Auto-vectorize `for i: c[i] = a[i] <op> b[i]` element-wise map. Same exact
    // matcher-with-scalar-fallback discipline as the reduction.
    if let Some(map) = detect_elementwise_map(ctx, name, step, body) {
        return lower_native_vectorized_map(ctx, name, start, end, &map, code);
    }

    // The counter and its two hidden slots (bound, step) were reserved during
    // frame planning, keyed by the counter name.
    let i_slot = ctx.local_slot(name)?;
    let end_slot = ctx.local_slot(&format!("{name}__end"))?;
    let step_slot = ctx.local_slot(&format!("{name}__step"))?;
    // The counter may be register-promoted (the bound/step stay on the stack as
    // loop-invariant memory operands). `i_reg` drives register vs stack access.
    let i_reg = ctx.promoted_reg(i_slot);
    let store_counter = |code: &mut Vec<u8>| match i_reg {
        Some(reg) => reg.from_rax(code), // reg = rax
        None => store_local(code, i_slot),
    };
    let load_counter = |code: &mut Vec<u8>| match i_reg {
        Some(reg) => reg.to_rax(code), // rax = reg
        None => load_local(code, i_slot),
    };

    // i = start
    lower_native_expr(ctx, start, code)?;
    store_counter(code);
    // end_local = end
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // step_local = step (default 1)
    match step {
        Some(step_expr) => lower_native_expr(ctx, step_expr, code)?,
        None => emit_mov_rax_imm(code, 1),
    }
    store_local(code, step_slot);

    // Arena stage-2 sub-region: save the entry bump pointer once the bounds are
    // seated (so the mark excludes only their one-time temps), before `top:`.
    let arena_reset_mark = arena_loop_reset_mark(
        ctx,
        expr_touches_heap(start, &ctx.heap_aggregates)
            || expr_touches_heap(end, &ctx.heap_aggregates)
            || step.is_some_and(|s| expr_touches_heap(s, &ctx.heap_aggregates))
            || body_touches_heap(body, &ctx.heap_aggregates),
        body,
        loops.len(),
    );
    if let Some(mark) = arena_reset_mark {
        emit_arena_loop_save(ctx, mark, code);
    }

    let top = code.len();
    // Loop guard: decide whether to run another iteration.
    // cond = (step >= 0) ? (i <= end) : (i >= end), placed in al.
    load_local(code, step_slot); // mov rax, [step]
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    // js descending  (jump if step < 0)
    code.extend_from_slice(&[0x0F, 0x88]);
    let js_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Ascending: cond = (i <= end)  ->  setle al
    load_counter(code);
    emit_for_compare(code, end_slot, 0x9E);
    code.push(0xE9); // jmp check
    let asc_done = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Descending: cond = (i >= end)  ->  setge al
    patch_rel32(code, js_site);
    load_counter(code);
    emit_for_compare(code, end_slot, 0x9D);

    // check: test al, al; jz end
    patch_rel32(code, asc_done);
    code.extend_from_slice(&[0x84, 0xC0]); // test al, al
    code.extend_from_slice(&[0x0F, 0x84]); // jz end (patched)
    let exit_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // `continue` jumps forward to the step block, so its target is not yet known.
    loops.push(NativeLoop {
        continue_target: None,
        continue_sites: Vec::new(),
        break_sites: Vec::new(),
        live_drops: Vec::new(),
        arena_reset_mark,
    });
    lower_loop_body_with_drops(ctx, body, code, loops)?;
    // Reclaim uniquely-owned per-iteration string temporaries on the fallthrough
    // back-edge (RC drop insertion). Placed BEFORE the step label. A `continue`
    // (which jumps to the step label) and a `break` now drop the live owned locals
    // on their OWN edge (see `lower_loop_body_with_drops`) before jumping, so every
    // path frees each owned temporary exactly once — the fallthrough here, or the
    // early-exit edge, never both.
    emit_loop_body_string_drops(ctx, body, code)?;
    // Arena stage-2: rewind the sub-region on the fallthrough back-edge, BEFORE the
    // step block. A `continue` (which jumps to the step label) skips this and does
    // its own rewind on its edge, so every path rewinds exactly once.
    if let Some(mark) = arena_reset_mark {
        emit_arena_loop_rewind(ctx, mark, code);
    }
    let loop_ctx = loops.pop().expect("loop pushed");

    // Step block (target of `continue`): i += step.
    let step_label = code.len();
    for site in loop_ctx.continue_sites {
        patch_rel32_to(code, site, step_label);
    }
    load_counter(code); // mov rax, i (register or stack)
    code.push(0x50); // push rax
    load_local(code, step_slot); // mov rax, [step]
    emit_i64_binop_from_stack(code, BinaryOp::Add)?;
    store_counter(code); // i = rax

    emit_jmp_to(code, top);

    let end = code.len();
    patch_rel32_to(code, exit_site, end);
    for site in loop_ctx.break_sites {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

/// Emit `cmp rax, [end]; set<cc> al` where the counter `i` is already in `rax`
/// and `set_opcode` is the second byte of the `0F` `setcc` form (e.g. `0x9E` =
/// setle, `0x9D` = setge). The bound stays a stack memory operand.
pub(crate) fn emit_for_compare(code: &mut Vec<u8>, end_slot: i32, set_opcode: u8) {
    // cmp rax, [rbp - end_slot]  ->  48 3B 85 disp32
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    // set<cc> al
    code.extend_from_slice(&[0x0F, set_opcode, 0xC0]);
}
