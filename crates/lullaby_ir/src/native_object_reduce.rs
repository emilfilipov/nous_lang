//! Native backend: closed-form / strength-reduced loop reductions. Detection and
//! emission of counting-sum, general-polynomial, affine, and quadratic reduction
//! loops (both `while` and `for` forms) evaluated in O(1) or via ILP unrolling.
//! Split out of native_object_stmt.rs; shared items via `use super::super::*`.
use super::super::*;

/// Lower `while cond: body` as: top: eval cond; `test`; `jz end`; body;
/// `jmp top`; end:. `break` targets `end`, `continue` targets `top`.
/// A detected `while i < BOUND { acc = acc + i; i = i + 1 }` counting-sum loop,
/// with `acc` and `i` both promoted into callee-saved registers and `BOUND` a
/// positive `i32`-range constant. This is the ILP target: the serial `acc += i`
/// chain (one dependent add per iteration, so the loop is latency-bound at ~1
/// cycle/iter) is broken by summing a block of `K` consecutive iterations in a
/// single `acc` add — `acc += K*i + K*(K-1)/2` — which is exact under wrapping
/// arithmetic (`sum(i..i+K) mod 2^64` equals `K*i + K*(K-1)/2 mod 2^64`), so the
/// dependent add is paid once per `K` iterations instead of once per iteration.
pub(crate) struct SumReductionLoop {
    acc: PReg,
    counter: PReg,
    bound: SumBound,
}

/// The loop bound of a counting-sum reduction: either a compile-time constant
/// (`while i < 1000`) or a loop-invariant runtime `i64` value (`while i < n`).
/// The runtime case is the common one in real code and, without this, stays a
/// latency-bound scalar loop; folding it the same way makes it beat C too.
pub(crate) enum SumBound {
    Const(i64),
    /// A loop-invariant bound expression (a bare `i64` variable, distinct from
    /// the accumulator and counter, so the body never mutates it).
    Runtime(BytecodeExpr),
}

/// The promoted register a bare `i64` local occupies, by name, or `None`.
pub(crate) fn promoted_reg_of_name(ctx: &NativeCtx, name: &str) -> Option<PReg> {
    ctx.promoted_reg(ctx.local_slot(name).ok()?)
}

/// If `expr` is `<promoted i64 reg> + const` or `<promoted i64 reg> - const` with
/// the constant in `i32` range, return the register and the signed displacement to
/// add — so the value can be formed with a single `lea reg2, [reg + disp]`.
pub(crate) fn promoted_reg_plus_const(ctx: &NativeCtx, expr: &BytecodeExpr) -> Option<(PReg, i32)> {
    let BytecodeExprKind::Binary { left, op, right } = &expr.kind else {
        return None;
    };
    if left.ty.name != "i64" || right.ty.name != "i64" {
        return None;
    }
    let reg = promoted_var_reg(ctx, left)?;
    let BytecodeExprKind::Integer(value) = &right.kind else {
        return None;
    };
    let value = i32::try_from(*value).ok()?;
    match op {
        BinaryOp::Add => Some((reg, value)),
        // `reg - v` == `lea [reg + (-v)]`; `checked_neg` guards the `i32::MIN` edge.
        BinaryOp::Subtract => value.checked_neg().map(|neg| (reg, neg)),
        _ => None,
    }
}

/// `lea rcx, [<reg> + disp32]` — form `reg ± const` directly into the first
/// argument register.
pub(crate) fn emit_lea_rcx_reg_disp(code: &mut Vec<u8>, reg: PReg, disp: i32) {
    // REX.W 8D /r ; ModRM mod=10 (disp32) reg=rcx(001) rm=<reg>. rbx/rsi need no SIB.
    let modrm = 0x80 | (0x01 << 3) | reg.code3();
    code.extend_from_slice(&[0x48, 0x8D, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}

/// True when `stmt` is `target = target <Add> addend` (via `=` with a `+` RHS or
/// via `+=`), i.e. an in-place add of `addend` into the promoted local `target`.
pub(crate) fn is_promoted_self_add(
    stmt: &BytecodeInstruction,
    target: &str,
    addend: &AddendCheck,
) -> bool {
    let BytecodeInstruction::Assign {
        name,
        path,
        op,
        value,
        ..
    } = stmt
    else {
        return false;
    };
    if name != target || !path.is_empty() {
        return false;
    }
    match op {
        // `target += addend`
        AssignOp::Add => addend.matches(&value.kind),
        // `target = target + addend`
        AssignOp::Replace => matches!(
            &value.kind,
            BytecodeExprKind::Binary { left, op: BinaryOp::Add, right }
                if matches!(&left.kind, BytecodeExprKind::Variable(v) if v == target)
                    && addend.matches(&right.kind)
        ),
        _ => false,
    }
}

/// What the added value must be: either the counter variable, or the literal `1`.
pub(crate) enum AddendCheck<'a> {
    Var(&'a str),
    One,
}

impl AddendCheck<'_> {
    fn matches(&self, kind: &BytecodeExprKind) -> bool {
        match (self, kind) {
            (AddendCheck::Var(name), BytecodeExprKind::Variable(v)) => v == name,
            (AddendCheck::One, BytecodeExprKind::Integer(1)) => true,
            _ => false,
        }
    }
}

/// Recognize the counting-sum loop `while i < CONST { acc = acc + i; i = i + 1 }`
/// where `acc` and `i` are distinct promoted `i64` locals and `CONST` is a
/// positive `i32`-range constant large enough (≥ 8) that the blocked main loop is
/// worthwhile and its guard arithmetic (`bound - (K-1)`) cannot underflow. Any
/// deviation returns `None`, so the caller emits the ordinary loop unchanged.
pub(crate) fn detect_sum_reduction(
    ctx: &NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
) -> Option<SumReductionLoop> {
    // Condition: `i < BOUND`, `i` a promoted i64, BOUND a positive i32 constant.
    let BytecodeExprKind::Binary {
        left,
        op: BinaryOp::Less,
        right,
    } = &condition.kind
    else {
        return None;
    };
    let BytecodeExprKind::Variable(counter_name) = &left.kind else {
        return None;
    };
    if left.ty.name != "i64" {
        return None;
    }

    // Body: exactly `[ acc = acc + i, i = i + 1 ]`.
    let [acc_stmt, step_stmt] = body else {
        return None;
    };
    let BytecodeInstruction::Assign { name: acc_name, .. } = acc_stmt else {
        return None;
    };
    if acc_name == counter_name {
        return None;
    }
    if !is_promoted_self_add(acc_stmt, acc_name, &AddendCheck::Var(counter_name)) {
        return None;
    }
    if !is_promoted_self_add(step_stmt, counter_name, &AddendCheck::One) {
        return None;
    }

    // Bound: a constant `≥ 8` (so the blocked loop is worthwhile and its
    // `bound - (K-1)` guard cannot underflow), or a loop-invariant `i64`
    // variable distinct from the accumulator and counter (so the body never
    // mutates it). The runtime case guards `n < K` at run time instead.
    let bound = match &right.kind {
        BytecodeExprKind::Integer(value) => {
            if *value < 8 || i32::try_from(*value).is_err() {
                return None;
            }
            SumBound::Const(*value)
        }
        BytecodeExprKind::Variable(bound_name)
            if right.ty.name == "i64" && bound_name != counter_name && bound_name != acc_name =>
        {
            SumBound::Runtime(right.as_ref().clone())
        }
        _ => return None,
    };

    let acc = promoted_reg_of_name(ctx, acc_name)?;
    let counter = promoted_reg_of_name(ctx, counter_name)?;
    if acc == counter {
        return None;
    }
    Some(SumReductionLoop {
        acc,
        counter,
        bound,
    })
}

/// Emit the ILP-unrolled counting-sum loop: a blocked main loop summing `K`
/// consecutive counter values per iteration into `acc` (one dependent add per
/// block), then a scalar remainder loop for the final `< K` iterations.
/// `cmp <counter>, <reg>` — a promoted-register counter compared directly
/// against a scratch register given by its full number (`rcx`=1, `rdx`=2,
/// `r11`=11, …). `cmp r/m64, r64` is REX.W(+R for r8..r15) 39 /r; ModRM =
/// 11 (reg&7) <counter>.
fn emit_cmp_counter_scratch(code: &mut Vec<u8>, counter: PReg, reg: u8) {
    let rex = 0x48 | if reg >= 8 { 0x04 } else { 0 };
    code.extend_from_slice(&[rex, 0x39, 0xC0 | ((reg & 7) << 3) | counter.code3()]);
}

/// Pad `code` with 1-byte NOPs until its length is a 16-byte multiple. Every
/// native function starts at a 16-aligned offset in `.text` (the object writer
/// pads between functions), so a function-buffer offset that is a multiple of 16
/// is also 16-aligned in the final image. Calling this right before a hot loop's
/// top aligns the loop entry, keeping the tight body off the 32-byte
/// fetch/uop-cache boundaries that otherwise roughly halve throughput — the
/// difference measured between two byte-identical ILP loops (0.066 vs 0.126
/// ns/iter). The pad executes once on loop entry (fall-through), so it is free.
fn emit_align_loop_top(code: &mut Vec<u8>) {
    while !code.len().is_multiple_of(16) {
        code.push(0x90); // nop
    }
}

/// `sub rdx, <counter>` (`counter` is rbx or rsi).
fn emit_sub_rdx_counter(code: &mut Vec<u8>, counter: PReg) {
    // sub r/m64, r64 = REX.W 29 /r ; ModRM = 11 <counter> rdx(010).
    code.extend_from_slice(&[0x48, 0x29, 0xC0 | (counter.code3() << 3) | 0x02]);
}

/// `mov <counter>, rcx` — seat the exit value `n` into the counter register.
fn emit_mov_counter_rcx(code: &mut Vec<u8>, counter: PReg) {
    // mov r/m64, r64 = REX.W 89 /r ; ModRM = 11 rcx(001) <counter>.
    code.extend_from_slice(&[0x48, 0x89, 0xC0 | (0x01 << 3) | counter.code3()]);
}

/// Emit the counting sum `while i < n { acc += i; i += 1 }` as a **closed form**
/// — no loop at all. The sum of the run `i = i0 .. n-1` is
/// `(i0 + n-1) * (n - i0) / 2`; since `(i0+n-1) + (n-i0) = 2n-1` is odd, exactly
/// one factor is even, so the `/2` is exact under wrapping (halve the even factor
/// first, then multiply). `i0`, `n`, and the running `acc` are read at run time,
/// so this is fully general (any start value, constant or runtime bound). A
/// `count <= 0` guard leaves `acc`/`i` untouched (an empty loop). O(1) instead of
/// O(n) — orders of magnitude faster than C's per-element loop.
pub(crate) fn emit_sum_reduction(
    ctx: &mut NativeCtx,
    plan: &SumReductionLoop,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let SumReductionLoop { acc, counter, .. } = *plan;
    const RCX: u8 = 1;

    // rcx = n (the bound), constant immediate or the runtime value.
    match &plan.bound {
        SumBound::Const(bound) => emit_mov_scratch_imm(code, RCX, *bound as i32),
        SumBound::Runtime(bound_expr) => {
            lower_native_expr(ctx, bound_expr, code)?; // rax = n
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
        }
    }

    // rdx = count = n - i0. If count <= 0 the loop body never runs; skip, leaving
    // acc and i unchanged.
    code.extend_from_slice(&[0x48, 0x89, 0xCA]); // mov rdx, rcx
    emit_sub_rdx_counter(code, counter); // sub rdx, i0
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx
    code.extend_from_slice(&[0x0F, 0x8E]); // jle done (count <= 0)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // rax = A = i0 + n - 1.
    counter.to_rax(code); // mov rax, i0
    code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx  (+ n)
    code.extend_from_slice(&[0x48, 0x83, 0xE8, 0x01]); // sub rax, 1

    // Exact `A * count / 2`: halve whichever of A/count is even, then multiply.
    // count (rdx) > 0 here. If count is odd then A is even (their sum is odd).
    code.extend_from_slice(&[0xF6, 0xC2, 0x01]); // test dl, 1
    code.extend_from_slice(&[0x0F, 0x84]); // jz count_even
    let count_even_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xD1, 0xF8]); // sar rax, 1   (A even -> A/2, exact)
    code.extend_from_slice(&[0xE9]); // jmp mul
    let jmp_mul_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    let count_even = code.len();
    patch_rel32_to(code, count_even_site, count_even);
    code.extend_from_slice(&[0x48, 0xD1, 0xEA]); // shr rdx, 1   (count even -> count/2, exact)
    let mul = code.len();
    patch_rel32_to(code, jmp_mul_site, mul);
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx  (rax = A*count/2)

    acc.add_rax(code); // acc += sum
    emit_mov_counter_rcx(code, counter); // i = n (the loop's exit value)

    let done = code.len();
    patch_rel32_to(code, done_site, done);
    Ok(())
}

/// `mov <scratch>, imm32` (sign-extended) for rcx/rdx (`REX.W C7 /0 id`).
fn emit_mov_scratch_imm(code: &mut Vec<u8>, scratch3: u8, imm: i32) {
    code.extend_from_slice(&[0x48, 0xC7, 0xC0 | scratch3]);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// A general reduction `while i < BOUND { acc = acc + EXPR; i = i + 1 }` where
/// `EXPR` is a **pure polynomial in the counter** (integer literals, the counter
/// `i`, and `+`/`-`/`*` only — no other variable, no division, no call, so it
/// cannot fault and is side-effect free). Distinct from the counting-sum
/// reduction (`EXPR == i`, which has a cheaper closed form). The naive lowering
/// serializes on the `acc` dependency (one add per element, latency-bound —
/// ~2.5× slower than C on `acc += i*i`); the multi-accumulator unroll spreads
/// the adds across four independent accumulators to break that chain, matching
/// C's ILP on sum-of-squares / dot-product / weighted-sum shapes.
pub(crate) struct GeneralReductionLoop {
    acc: PReg,
    counter: PReg,
    bound: SumBound,
    addend: BytecodeExpr,
}

/// True when `expr` is a pure polynomial in `counter` — the safe class the
/// multi-accumulator reduction may re-evaluate at several counter values: an
/// integer literal, the counter itself, or `+`/`-`/`*` of such. Any other
/// variable, operator, index, or call disqualifies it (a non-counter variable
/// could be loop-variant; `/`/`%` could divide by zero; a call could fault or
/// have effects).
fn is_pure_counter_poly(expr: &BytecodeExprKind, counter: &str) -> bool {
    match expr {
        BytecodeExprKind::Integer(_) => true,
        BytecodeExprKind::Variable(name) => name == counter,
        BytecodeExprKind::Binary { left, op, right } => {
            matches!(op, BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply)
                && is_pure_counter_poly(&left.kind, counter)
                && is_pure_counter_poly(&right.kind, counter)
        }
        _ => false,
    }
}

/// If `stmt` is `acc = acc + EXPR` (via `=` with an `acc + …` RHS) or
/// `acc += EXPR`, return `EXPR`.
fn reduction_addend(stmt: &BytecodeInstruction, acc: &str) -> Option<BytecodeExpr> {
    let BytecodeInstruction::Assign {
        name,
        path,
        op,
        value,
        ..
    } = stmt
    else {
        return None;
    };
    if name != acc || !path.is_empty() {
        return None;
    }
    match op {
        AssignOp::Add => Some(value.clone()),
        AssignOp::Replace => match &value.kind {
            BytecodeExprKind::Binary {
                left,
                op: BinaryOp::Add,
                right,
            } if matches!(&left.kind, BytecodeExprKind::Variable(v) if v == acc) => {
                Some(right.as_ref().clone())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Parse `i < BOUND` where `BOUND` is a constant `≥ 8` or a loop-invariant `i64`
/// variable distinct from `counter`/`acc`, shared by both reduction detectors.
fn parse_reduction_bound(
    right: &BytecodeExpr,
    counter_name: &str,
    acc_name: &str,
) -> Option<SumBound> {
    match &right.kind {
        BytecodeExprKind::Integer(value) => {
            if *value < 8 || i32::try_from(*value).is_err() {
                return None;
            }
            Some(SumBound::Const(*value))
        }
        BytecodeExprKind::Variable(bound_name)
            if right.ty.name == "i64" && bound_name != counter_name && bound_name != acc_name =>
        {
            Some(SumBound::Runtime(right.clone()))
        }
        _ => None,
    }
}

pub(crate) fn detect_general_reduction(
    ctx: &NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
) -> Option<GeneralReductionLoop> {
    let BytecodeExprKind::Binary {
        left,
        op: BinaryOp::Less,
        right,
    } = &condition.kind
    else {
        return None;
    };
    let BytecodeExprKind::Variable(counter_name) = &left.kind else {
        return None;
    };
    if left.ty.name != "i64" {
        return None;
    }
    // Body: exactly `[ acc = acc + EXPR, i = i + 1 ]`.
    let [acc_stmt, step_stmt] = body else {
        return None;
    };
    let BytecodeInstruction::Assign { name: acc_name, .. } = acc_stmt else {
        return None;
    };
    if acc_name == counter_name {
        return None;
    }
    let addend = reduction_addend(acc_stmt, acc_name)?;
    // The addend must be a pure polynomial in the counter — never the bare
    // counter (that is the cheaper closed-form counting sum, detected first) and
    // never referencing the accumulator (`is_pure_counter_poly` forbids any
    // non-counter variable).
    if !is_pure_counter_poly(&addend.kind, counter_name) {
        return None;
    }
    if !is_promoted_self_add(step_stmt, counter_name, &AddendCheck::One) {
        return None;
    }
    let bound = parse_reduction_bound(right, counter_name, acc_name)?;
    let acc = promoted_reg_of_name(ctx, acc_name)?;
    let counter = promoted_reg_of_name(ctx, counter_name)?;
    if acc == counter {
        return None;
    }
    Some(GeneralReductionLoop {
        acc,
        counter,
        bound,
        addend,
    })
}

/// `add <r8|r9|r10>, rax` — accumulate rax into an extra scratch accumulator.
fn emit_add_scratch_acc_rax(code: &mut Vec<u8>, acc_index: u8) {
    // add r/m64, r64 = REX.WB 01 /r ; reg = rax(000), rm = r8+acc_index.
    code.extend_from_slice(&[0x49, 0x01, 0xC0 | acc_index]);
}

/// `add <acc>, <r8|r9|r10>` — fold an extra accumulator into the promoted `acc`.
fn emit_add_acc_scratch(code: &mut Vec<u8>, acc: PReg, acc_index: u8) {
    // add r/m64, r64 = REX.WR 01 /r ; reg = r8+acc_index (REX.R), rm = acc.
    code.extend_from_slice(&[0x4C, 0x01, 0xC0 | (acc_index << 3) | acc.code3()]);
}

/// Emit the multi-accumulator ILP reduction. Four iterations per block, each
/// evaluating `EXPR` at the running counter and adding into one of four
/// independent accumulators (the promoted `acc` plus `r8`/`r9`/`r10`), so the
/// per-iteration `acc` adds no longer form a single serial chain. A scalar
/// remainder finishes the final `< 4` iterations. Bit-identical to the serial
/// loop under wrapping arithmetic (addition is associative mod 2^64).
pub(crate) fn emit_general_reduction(
    ctx: &mut NativeCtx,
    plan: &GeneralReductionLoop,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    const K: i64 = 4;
    let GeneralReductionLoop {
        acc,
        counter,
        addend,
        ..
    } = plan;
    let (acc, counter) = (*acc, *counter);
    const RDX: u8 = 2; // limit = bound - (K-1)
    const R11: u8 = 11; // bound

    // Zero the three extra accumulators FIRST — before the runtime `n < K` guard,
    // which skips the blocked loop but still falls through to the accumulator
    // combine, so they must already be zero on that path.
    code.extend_from_slice(&[0x4D, 0x31, 0xC0]); // xor r8, r8
    code.extend_from_slice(&[0x4D, 0x31, 0xC9]); // xor r9, r9
    code.extend_from_slice(&[0x4D, 0x31, 0xD2]); // xor r10, r10

    // Materialize the bounds. `EXPR` lowering uses only rax/rcx and the stack, so
    // the bound (r11) and limit (rdx) survive it; the three extra accumulators
    // (r8/r9/r10) do too. r11/rdx/r8/r9/r10 are all caller-saved scratch, dead
    // before and after the loop.
    let skip_main_site = match &plan.bound {
        SumBound::Const(bound) => {
            // mov r11, bound ; mov rdx, bound-(K-1).
            code.extend_from_slice(&[0x49, 0xC7, 0xC3]); // mov r11, imm32
            code.extend_from_slice(&(*bound as i32).to_le_bytes());
            emit_mov_scratch_imm(code, RDX, (bound - (K - 1)) as i32);
            None
        }
        SumBound::Runtime(bound_expr) => {
            lower_native_expr(ctx, bound_expr, code)?; // rax = n
            code.extend_from_slice(&[0x49, 0x89, 0xC3]); // mov r11, rax
            code.extend_from_slice(&[0x49, 0x83, 0xFB, K as u8]); // cmp r11, K
            code.extend_from_slice(&[0x0F, 0x8C]); // jl skip_main
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            code.extend_from_slice(&[0x4C, 0x89, 0xDA]); // mov rdx, r11
            code.extend_from_slice(&[0x48, 0x83, 0xEA, (K - 1) as u8]); // sub rdx, K-1
            Some(site)
        }
    };

    // Blocked main loop: while i < bound-(K-1), do K iterations into K accumulators.
    emit_align_loop_top(code);
    let main_top = code.len();
    emit_cmp_counter_scratch(code, counter, RDX); // cmp i, rdx
    code.extend_from_slice(&[0x0F, 0x8D]); // jge main_end
    let main_exit = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    for lane in 0..K {
        lower_native_expr(ctx, addend, code)?; // rax = EXPR at current i
        if lane == 0 {
            acc.add_rax(code); // acc += rax
        } else {
            emit_add_scratch_acc_rax(code, (lane - 1) as u8); // r{8+lane-1} += rax
        }
        counter.add_imm(code, 1); // i += 1
    }
    emit_jmp_to(code, main_top);
    let main_end = code.len();
    patch_rel32_to(code, main_exit, main_end);

    if let Some(site) = skip_main_site {
        let here = code.len();
        patch_rel32_to(code, site, here);
    }

    // Fold the extra accumulators into `acc`: acc += r8 + r9 + r10.
    emit_add_acc_scratch(code, acc, 0); // acc += r8
    emit_add_acc_scratch(code, acc, 1); // acc += r9
    emit_add_acc_scratch(code, acc, 2); // acc += r10

    // Scalar remainder: while i < bound, acc += EXPR; i += 1.
    let rem_top = code.len();
    emit_cmp_counter_scratch(code, counter, R11); // cmp i, r11 (bound)
    code.extend_from_slice(&[0x0F, 0x8D]); // jge rem_end
    let rem_exit = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    lower_native_expr(ctx, addend, code)?; // rax = EXPR
    acc.add_rax(code); // acc += rax
    counter.add_imm(code, 1); // i += 1
    emit_jmp_to(code, rem_top);
    let rem_end = code.len();
    patch_rel32_to(code, rem_exit, rem_end);
    Ok(())
}

/// A reduction whose addend is **affine** in the counter — `acc += a*i + b` for
/// integer constants `a`, `b` (the counting sum is `a=1, b=0`; `2*i`, `3*i+5`,
/// `(i-2)`, `i+i` all qualify). The block sum `sum_{j<K}(a*(i+j)+b) = a*K*i +
/// (a*K(K-1)/2 + K*b)` is exact under wrapping, so K iterations fold into a
/// single `imul`+`add`+`acc-add` — one dependent op per K iterations, beating C's
/// per-element loop. Quadratic-and-up addends fall to the multi-accumulator path.
pub(crate) struct AffineReductionLoop {
    acc: PReg,
    counter: PReg,
    bound: SumBound,
    /// The addend is `a*i + b`; the whole loop closed-forms to
    /// `acc += a*S + b*count` where `S = sum(i0..n-1)` and `count = n - i0`.
    a: i32,
    b: i32,
}

/// The affine form `(a, b)` of `expr` as `a*counter + b` (wrapping i64
/// coefficients), or `None` if it is not affine in the counter (a non-counter
/// variable, a product of two counter-dependent factors, division, or a call).
fn affine_form(expr: &BytecodeExprKind, counter: &str) -> Option<(i64, i64)> {
    match expr {
        BytecodeExprKind::Integer(c) => Some((0, *c)),
        BytecodeExprKind::Variable(name) if name == counter => Some((1, 0)),
        BytecodeExprKind::Binary { left, op, right } => {
            let (la, lb) = affine_form(&left.kind, counter)?;
            let (ra, rb) = affine_form(&right.kind, counter)?;
            match op {
                BinaryOp::Add => Some((la.wrapping_add(ra), lb.wrapping_add(rb))),
                BinaryOp::Subtract => Some((la.wrapping_sub(ra), lb.wrapping_sub(rb))),
                // Affine × affine is affine only if a factor is constant (its
                // slope is 0); otherwise the product is quadratic.
                BinaryOp::Multiply if la == 0 => Some((lb.wrapping_mul(ra), lb.wrapping_mul(rb))),
                BinaryOp::Multiply if ra == 0 => Some((rb.wrapping_mul(la), rb.wrapping_mul(lb))),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The polynomial coefficients `[c0, c1, c2]` of `expr` as `c2*i² + c1*i + c0`
/// (wrapping i64), or `None` if it is not a polynomial of degree ≤ 2 in the
/// counter (a non-counter variable, division, a call, or a product whose degree
/// would exceed 2). Generalizes [`affine_form`] one degree higher.
fn poly_form(expr: &BytecodeExprKind, counter: &str) -> Option<[i64; 3]> {
    match expr {
        BytecodeExprKind::Integer(c) => Some([*c, 0, 0]),
        BytecodeExprKind::Variable(name) if name == counter => Some([0, 1, 0]),
        BytecodeExprKind::Binary { left, op, right } => {
            let l = poly_form(&left.kind, counter)?;
            let r = poly_form(&right.kind, counter)?;
            let degree = |p: &[i64; 3]| {
                if p[2] != 0 {
                    2
                } else if p[1] != 0 {
                    1
                } else {
                    0
                }
            };
            match op {
                BinaryOp::Add => Some([
                    l[0].wrapping_add(r[0]),
                    l[1].wrapping_add(r[1]),
                    l[2].wrapping_add(r[2]),
                ]),
                BinaryOp::Subtract => Some([
                    l[0].wrapping_sub(r[0]),
                    l[1].wrapping_sub(r[1]),
                    l[2].wrapping_sub(r[2]),
                ]),
                // A product stays degree ≤ 2 only if the operand degrees sum to
                // ≤ 2; then the degree-3/4 cross terms vanish.
                BinaryOp::Multiply if degree(&l) + degree(&r) <= 2 => Some([
                    l[0].wrapping_mul(r[0]),
                    l[0].wrapping_mul(r[1])
                        .wrapping_add(l[1].wrapping_mul(r[0])),
                    l[0].wrapping_mul(r[2])
                        .wrapping_add(l[1].wrapping_mul(r[1]))
                        .wrapping_add(l[2].wrapping_mul(r[0])),
                ]),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn detect_affine_reduction(
    ctx: &NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
) -> Option<AffineReductionLoop> {
    let BytecodeExprKind::Binary {
        left,
        op: BinaryOp::Less,
        right,
    } = &condition.kind
    else {
        return None;
    };
    let BytecodeExprKind::Variable(counter_name) = &left.kind else {
        return None;
    };
    if left.ty.name != "i64" {
        return None;
    }
    let [acc_stmt, step_stmt] = body else {
        return None;
    };
    let BytecodeInstruction::Assign { name: acc_name, .. } = acc_stmt else {
        return None;
    };
    if acc_name == counter_name {
        return None;
    }
    let addend = reduction_addend(acc_stmt, acc_name)?;
    if !is_promoted_self_add(step_stmt, counter_name, &AddendCheck::One) {
        return None;
    }
    let (a64, b64) = affine_form(&addend.kind, counter_name)?;
    // The coefficients must fit an i32 immediate for the closed-form `imul`s;
    // otherwise fall through to the multi-accumulator reduction.
    let a = i32::try_from(a64).ok()?;
    let b = i32::try_from(b64).ok()?;
    let bound = parse_reduction_bound(right, counter_name, acc_name)?;
    let acc = promoted_reg_of_name(ctx, acc_name)?;
    let counter = promoted_reg_of_name(ctx, counter_name)?;
    if acc == counter {
        return None;
    }
    Some(AffineReductionLoop {
        acc,
        counter,
        bound,
        a,
        b,
    })
}

/// `sub r8, <counter>` — `r8 -= i0` to form `count = n - i0`.
fn emit_sub_r8_counter(code: &mut Vec<u8>, counter: PReg) {
    // sub r/m64, r64 = REX.WB 29 /r ; ModRM = 11 <counter> r8(000).
    code.extend_from_slice(&[0x49, 0x29, 0xC0 | (counter.code3() << 3)]);
}

/// Emit the affine reduction `while i < n { acc += a*i + b; i += 1 }` as the O(1)
/// closed form `acc += a*S + b*count`, where `count = n - i0` (kept in r8) and
/// `S = sum(i0..n-1) = (i0+n-1)*count/2` (the exact wrapping halve of the
/// counting sum). Reads i0/n/acc at run time; a `count <= 0` guard skips an empty
/// loop and it seats `i = n` afterward. O(1) instead of O(n).
pub(crate) fn emit_affine_reduction(
    ctx: &mut NativeCtx,
    plan: &AffineReductionLoop,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let AffineReductionLoop {
        acc, counter, a, b, ..
    } = *plan;
    const RCX: u8 = 1;

    // rcx = n.
    match &plan.bound {
        SumBound::Const(bound) => emit_mov_scratch_imm(code, RCX, *bound as i32),
        SumBound::Runtime(bound_expr) => {
            lower_native_expr(ctx, bound_expr, code)?; // rax = n
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
        }
    }

    // r8 = count = n - i0. Skip (acc/i unchanged) when count <= 0.
    code.extend_from_slice(&[0x49, 0x89, 0xC8]); // mov r8, rcx
    emit_sub_r8_counter(code, counter); // sub r8, i0
    code.extend_from_slice(&[0x4D, 0x85, 0xC0]); // test r8, r8
    code.extend_from_slice(&[0x0F, 0x8E]); // jle done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // rax = A = i0 + n - 1.
    counter.to_rax(code);
    code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx
    code.extend_from_slice(&[0x48, 0x83, 0xE8, 0x01]); // sub rax, 1

    // rax = S = A*count/2 (exact halve of whichever of A/count is even).
    code.extend_from_slice(&[0x4C, 0x89, 0xC2]); // mov rdx, r8  (count copy)
    code.extend_from_slice(&[0xF6, 0xC2, 0x01]); // test dl, 1
    code.extend_from_slice(&[0x0F, 0x84]); // jz count_even
    let count_even_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xD1, 0xF8]); // sar rax, 1
    code.extend_from_slice(&[0xE9]); // jmp mul
    let jmp_mul_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    let count_even = code.len();
    patch_rel32_to(code, count_even_site, count_even);
    code.extend_from_slice(&[0x48, 0xD1, 0xEA]); // shr rdx, 1
    let mul = code.len();
    patch_rel32_to(code, jmp_mul_site, mul);
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx -> rax = S

    // result = a*S + b*count.
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, a
    code.extend_from_slice(&a.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x69, 0xD0]); // imul rdx, r8, b
    code.extend_from_slice(&b.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx

    acc.add_rax(code); // acc += a*S + b*count
    emit_mov_counter_rcx(code, counter); // i = n

    let done = code.len();
    patch_rel32_to(code, done_site, done);
    Ok(())
}

/// A degree-2 reduction `acc += c2*i² + c1*i + c0` over the counter — the closed
/// form `acc += c2*S2 + c1*S1 + c0*count`, where `S2 = sum(i²)`. O(1), like the
/// affine case, one degree higher.
pub(crate) struct QuadraticReductionLoop {
    acc: PReg,
    counter: PReg,
    bound: SumBound,
    c0: i32,
    c1: i32,
    c2: i32,
}

pub(crate) fn detect_quadratic_reduction(
    ctx: &NativeCtx,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
) -> Option<QuadraticReductionLoop> {
    let BytecodeExprKind::Binary {
        left,
        op: BinaryOp::Less,
        right,
    } = &condition.kind
    else {
        return None;
    };
    let BytecodeExprKind::Variable(counter_name) = &left.kind else {
        return None;
    };
    if left.ty.name != "i64" {
        return None;
    }
    let [acc_stmt, step_stmt] = body else {
        return None;
    };
    let BytecodeInstruction::Assign { name: acc_name, .. } = acc_stmt else {
        return None;
    };
    if acc_name == counter_name {
        return None;
    }
    let addend = reduction_addend(acc_stmt, acc_name)?;
    if !is_promoted_self_add(step_stmt, counter_name, &AddendCheck::One) {
        return None;
    }
    let [c0_64, c1_64, c2_64] = poly_form(&addend.kind, counter_name)?;
    // Degree must be exactly 2 (degree ≤ 1 is the cheaper affine path).
    if c2_64 == 0 {
        return None;
    }
    let c0 = i32::try_from(c0_64).ok()?;
    let c1 = i32::try_from(c1_64).ok()?;
    let c2 = i32::try_from(c2_64).ok()?;
    let bound = parse_reduction_bound(right, counter_name, acc_name)?;
    let acc = promoted_reg_of_name(ctx, acc_name)?;
    let counter = promoted_reg_of_name(ctx, counter_name)?;
    if acc == counter {
        return None;
    }
    Some(QuadraticReductionLoop {
        acc,
        counter,
        bound,
        c0,
        c1,
        c2,
    })
}

/// `rax = g(m) = m(m+1)(2m+1)/6` for `m` in `rcx` (clobbers rax/rdx, preserves
/// rcx). Computed exactly mod 2^64: `m(m+1)/2` by halving the even factor of
/// `{m, m+1}` (avoiding an overflow-before-halve), `* (2m+1)`, then `/3` via the
/// modular inverse of 3 (`0xAAAA…AB`) — exact because `m(m+1)(2m+1)/2` is always
/// a multiple of 3, so `X * inv3 ≡ X/3 (mod 2^64)`.
fn emit_g_of_m(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x8D, 0x51, 0x01]); // lea rdx, [rcx + 1]   (m+1)
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx              (m)
    code.extend_from_slice(&[0xF6, 0xC1, 0x01]); // test cl, 1
    code.extend_from_slice(&[0x0F, 0x84]); // jz m_even
    let m_even_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xD1, 0xFA]); // sar rdx, 1  (m odd -> (m+1)/2)
    code.extend_from_slice(&[0xE9]); // jmp mul_half
    let mul_half_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    let m_even = code.len();
    patch_rel32_to(code, m_even_site, m_even);
    code.extend_from_slice(&[0x48, 0xD1, 0xF8]); // sar rax, 1  (m even -> m/2)
    let mul_half = code.len();
    patch_rel32_to(code, mul_half_site, mul_half);
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx  -> m(m+1)/2
    code.extend_from_slice(&[0x48, 0x8D, 0x54, 0x09, 0x01]); // lea rdx, [rcx + rcx + 1]  (2m+1)
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx  -> Q = m(m+1)(2m+1)/2
    code.extend_from_slice(&[0x48, 0xBA]); // mov rdx, inv3
    code.extend_from_slice(&0xAAAA_AAAA_AAAA_AAABu64.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx  -> Q/3 = g(m)
}

/// Emit the quadratic reduction closed form `acc += c2*S2 + c1*S1 + c0*count`.
/// `count = n - i0` (r8), `S1 = (i0+n-1)*count/2` (r9), `S2 = sum(i²) = g(n-1) -
/// g(i0-1)` (r10). `i0` stays in the promoted counter register throughout; `n`
/// in r11. O(1). A `count <= 0` guard leaves acc/i untouched; `i = n` afterward.
pub(crate) fn emit_quadratic_reduction(
    ctx: &mut NativeCtx,
    plan: &QuadraticReductionLoop,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let QuadraticReductionLoop {
        acc,
        counter,
        c0,
        c1,
        c2,
        ..
    } = *plan;

    // r11 = n.
    match &plan.bound {
        SumBound::Const(bound) => {
            code.extend_from_slice(&[0x49, 0xC7, 0xC3]); // mov r11, imm32
            code.extend_from_slice(&(*bound as i32).to_le_bytes());
        }
        SumBound::Runtime(bound_expr) => {
            lower_native_expr(ctx, bound_expr, code)?; // rax = n
            code.extend_from_slice(&[0x49, 0x89, 0xC3]); // mov r11, rax
        }
    }

    // r8 = count = n - i0. Skip when count <= 0.
    code.extend_from_slice(&[0x4D, 0x89, 0xD8]); // mov r8, r11
    emit_sub_r8_counter(code, counter); // sub r8, i0
    code.extend_from_slice(&[0x4D, 0x85, 0xC0]); // test r8, r8
    code.extend_from_slice(&[0x0F, 0x8E]); // jle done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // r9 = S1 = (i0 + n - 1)*count/2.
    counter.to_rax(code); // rax = i0
    code.extend_from_slice(&[0x4C, 0x01, 0xD8]); // add rax, r11
    code.extend_from_slice(&[0x48, 0x83, 0xE8, 0x01]); // sub rax, 1
    code.extend_from_slice(&[0x4C, 0x89, 0xC2]); // mov rdx, r8  (count copy)
    code.extend_from_slice(&[0xF6, 0xC2, 0x01]); // test dl, 1
    code.extend_from_slice(&[0x0F, 0x84]); // jz s1_even
    let s1_even_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xD1, 0xF8]); // sar rax, 1
    code.extend_from_slice(&[0xE9]);
    let s1_mul_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    let s1_even = code.len();
    patch_rel32_to(code, s1_even_site, s1_even);
    code.extend_from_slice(&[0x48, 0xD1, 0xEA]); // shr rdx, 1
    let s1_mul = code.len();
    patch_rel32_to(code, s1_mul_site, s1_mul);
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx -> S1
    code.extend_from_slice(&[0x49, 0x89, 0xC1]); // mov r9, rax  (S1)

    // r10 = S2 = g(n-1) - g(i0-1).
    code.extend_from_slice(&[0x49, 0x8D, 0x4B, 0xFF]); // lea rcx, [r11 - 1]  (n-1)
    emit_g_of_m(code);
    code.extend_from_slice(&[0x49, 0x89, 0xC2]); // mov r10, rax  (g(n-1))
    code.extend_from_slice(&[0x48, 0x8D, 0x48 | counter.code3(), 0xFF]); // lea rcx, [i0 - 1]
    emit_g_of_m(code);
    code.extend_from_slice(&[0x49, 0x29, 0xC2]); // sub r10, rax  -> S2 = g(n-1) - g(i0-1)

    // acc += c2*S2 + c1*S1 + c0*count.
    code.extend_from_slice(&[0x4C, 0x89, 0xD0]); // mov rax, r10  (S2)
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, c2
    code.extend_from_slice(&c2.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0xCA]); // mov rdx, r9  (S1)
    code.extend_from_slice(&[0x48, 0x69, 0xD2]); // imul rdx, rdx, c1
    code.extend_from_slice(&c1.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx
    code.extend_from_slice(&[0x4C, 0x89, 0xC2]); // mov rdx, r8  (count)
    code.extend_from_slice(&[0x48, 0x69, 0xD2]); // imul rdx, rdx, c0
    code.extend_from_slice(&c0.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx

    acc.add_rax(code); // acc += sum
    // i = n : mov counter, r11.
    code.extend_from_slice(&[0x4C, 0x89, 0xD8 | counter.code3()]); // mov counter, r11

    let done = code.len();
    patch_rel32_to(code, done_site, done);
    Ok(())
}

/// Lower a range `for i = start..=end step s` to an `i64` counter loop mirroring
/// the interpreter's inclusive range: ascending stops when `i > end`, descending
/// when `i < end`. `continue` jumps to the step, `break` exits.
/// A `for i from a to b { acc += EXPR }` whose addend is **affine** in the
/// (inclusive) counter — the O(1) closed form applies to `for` loops even though
/// the ILP unroll could not (the `for` counter is stack-resident, but the closed
/// form needs no counter at all, and the counter is not observable after the
/// loop). `acc += a_coef*S1 + b_coef*count` where the run is `i = a..=b`,
/// `count = b - a + 1`, and `S1 = (a+b)*count/2`.
pub(crate) struct ForAffineReduction {
    acc_reg: Option<PReg>,
    acc_slot: i32,
    start: BytecodeExpr,
    end: BytecodeExpr,
    a_coef: i32,
    b_coef: i32,
}

pub(crate) fn detect_for_affine_reduction(
    ctx: &NativeCtx,
    name: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<ForAffineReduction> {
    // Default step of 1 only (a strided range needs a different closed form).
    match step {
        None => {}
        Some(expr) => match &expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    // Body: exactly `[ acc = acc + EXPR ]` (the `for` handles the increment).
    let [acc_stmt] = body else {
        return None;
    };
    let BytecodeInstruction::Assign { name: acc_name, .. } = acc_stmt else {
        return None;
    };
    if acc_name == name {
        return None;
    }
    let addend = reduction_addend(acc_stmt, acc_name)?;
    let (a64, b64) = affine_form(&addend.kind, name)?;
    let a_coef = i32::try_from(a64).ok()?;
    let b_coef = i32::try_from(b64).ok()?;
    let acc_local = ctx.local(acc_name).ok()?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    let acc_slot = acc_local.slot;
    let acc_reg = ctx.promoted_reg(acc_slot);
    Some(ForAffineReduction {
        acc_reg,
        acc_slot,
        start: start.clone(),
        end: end.clone(),
        a_coef,
        b_coef,
    })
}

/// Emit the `for` affine reduction closed form. The inclusive range `a..=b` has
/// `count = b - a + 1` and `S1 = (a+b)*count/2` (exact wrapping halve, as in the
/// while-loop closed form); `acc += a_coef*S1 + b_coef*count`. `a`/`b`/`acc` are
/// read at run time; `count <= 0` (i.e. `b < a`) leaves `acc` untouched.
pub(crate) fn emit_for_affine_reduction(
    ctx: &mut NativeCtx,
    plan: &ForAffineReduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // r8 = a, r9 = b (spill a across b's evaluation so both survive any expr).
    lower_native_expr(ctx, &plan.start, code)?; // rax = a
    code.push(0x50); // push rax
    lower_native_expr(ctx, &plan.end, code)?; // rax = b
    code.extend_from_slice(&[0x49, 0x89, 0xC1]); // mov r9, rax
    code.extend_from_slice(&[0x41, 0x58]); // pop r8  (a)

    // r10 = count = b - a + 1. Skip when count <= 0.
    code.extend_from_slice(&[0x4D, 0x89, 0xCA]); // mov r10, r9
    code.extend_from_slice(&[0x4D, 0x29, 0xC2]); // sub r10, r8
    code.extend_from_slice(&[0x49, 0x83, 0xC2, 0x01]); // add r10, 1
    code.extend_from_slice(&[0x4D, 0x85, 0xD2]); // test r10, r10
    code.extend_from_slice(&[0x0F, 0x8E]); // jle done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // rax = A2 = a + b.
    code.extend_from_slice(&[0x4C, 0x89, 0xC0]); // mov rax, r8
    code.extend_from_slice(&[0x4C, 0x01, 0xC8]); // add rax, r9

    // rax = S1 = A2 * count / 2 (halve whichever of A2/count is even).
    code.extend_from_slice(&[0x4C, 0x89, 0xD2]); // mov rdx, r10  (count copy)
    code.extend_from_slice(&[0xF6, 0xC2, 0x01]); // test dl, 1
    code.extend_from_slice(&[0x0F, 0x84]); // jz count_even
    let count_even_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xD1, 0xF8]); // sar rax, 1
    code.extend_from_slice(&[0xE9]); // jmp mul
    let jmp_mul_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    let count_even = code.len();
    patch_rel32_to(code, count_even_site, count_even);
    code.extend_from_slice(&[0x48, 0xD1, 0xEA]); // shr rdx, 1
    let mul = code.len();
    patch_rel32_to(code, jmp_mul_site, mul);
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx -> S1

    // sum = a_coef*S1 + b_coef*count.
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, a_coef
    code.extend_from_slice(&plan.a_coef.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x69, 0xD2]); // imul rdx, r10, b_coef
    code.extend_from_slice(&plan.b_coef.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx

    // acc += sum.
    match plan.acc_reg {
        Some(reg) => reg.add_rax(code),
        None => {
            code.extend_from_slice(&[0x48, 0x01, 0x85]); // add [rbp + disp32], rax
            code.extend_from_slice(&(-plan.acc_slot).to_le_bytes());
        }
    }

    let done = code.len();
    patch_rel32_to(code, done_site, done);
    Ok(())
}

/// A `for i from a to b { acc += c2*i² + c1*i + c0 }` — the O(1) quadratic closed
/// form for `for` loops, mirroring [`ForAffineReduction`] one degree higher.
pub(crate) struct ForQuadraticReduction {
    acc_reg: Option<PReg>,
    acc_slot: i32,
    start: BytecodeExpr,
    end: BytecodeExpr,
    c0: i32,
    c1: i32,
    c2: i32,
}

pub(crate) fn detect_for_quadratic_reduction(
    ctx: &NativeCtx,
    name: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<ForQuadraticReduction> {
    match step {
        None => {}
        Some(expr) => match &expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [acc_stmt] = body else {
        return None;
    };
    let BytecodeInstruction::Assign { name: acc_name, .. } = acc_stmt else {
        return None;
    };
    if acc_name == name {
        return None;
    }
    let addend = reduction_addend(acc_stmt, acc_name)?;
    let [c0_64, c1_64, c2_64] = poly_form(&addend.kind, name)?;
    if c2_64 == 0 {
        return None; // degree ≤ 1 is the cheaper for-affine path
    }
    let c0 = i32::try_from(c0_64).ok()?;
    let c1 = i32::try_from(c1_64).ok()?;
    let c2 = i32::try_from(c2_64).ok()?;
    let acc_local = ctx.local(acc_name).ok()?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    let acc_slot = acc_local.slot;
    let acc_reg = ctx.promoted_reg(acc_slot);
    Some(ForQuadraticReduction {
        acc_reg,
        acc_slot,
        start: start.clone(),
        end: end.clone(),
        c0,
        c1,
        c2,
    })
}

/// Emit the `for` quadratic closed form over the inclusive range `a..=b`:
/// `count = b-a+1`, `S1 = (a+b)*count/2`, `S2 = Σi² = g(b) - g(a-1)`,
/// `acc += c2*S2 + c1*S1 + c0*count`. `a`/`b` are read at run time; `count <= 0`
/// leaves `acc` untouched. O(1).
pub(crate) fn emit_for_quadratic_reduction(
    ctx: &mut NativeCtx,
    plan: &ForQuadraticReduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // r8 = a, r9 = b.
    lower_native_expr(ctx, &plan.start, code)?; // rax = a
    code.push(0x50); // push rax
    lower_native_expr(ctx, &plan.end, code)?; // rax = b
    code.extend_from_slice(&[0x49, 0x89, 0xC1]); // mov r9, rax
    code.extend_from_slice(&[0x41, 0x58]); // pop r8

    // r10 = count = b - a + 1. Skip when count <= 0.
    code.extend_from_slice(&[0x4D, 0x89, 0xCA]); // mov r10, r9
    code.extend_from_slice(&[0x4D, 0x29, 0xC2]); // sub r10, r8
    code.extend_from_slice(&[0x49, 0x83, 0xC2, 0x01]); // add r10, 1
    code.extend_from_slice(&[0x4D, 0x85, 0xD2]); // test r10, r10
    code.extend_from_slice(&[0x0F, 0x8E]); // jle done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // r11 = S2 = g(b) - g(a-1).
    code.extend_from_slice(&[0x4C, 0x89, 0xC9]); // mov rcx, r9  (b)
    emit_g_of_m(code);
    code.extend_from_slice(&[0x49, 0x89, 0xC3]); // mov r11, rax  (g(b))
    code.extend_from_slice(&[0x49, 0x8D, 0x48, 0xFF]); // lea rcx, [r8 - 1]  (a-1)
    emit_g_of_m(code);
    code.extend_from_slice(&[0x49, 0x29, 0xC3]); // sub r11, rax  -> S2

    // rax = S1 = (a+b)*count/2.
    code.extend_from_slice(&[0x4C, 0x89, 0xC0]); // mov rax, r8
    code.extend_from_slice(&[0x4C, 0x01, 0xC8]); // add rax, r9  (a+b)
    code.extend_from_slice(&[0x4C, 0x89, 0xD2]); // mov rdx, r10  (count copy)
    code.extend_from_slice(&[0xF6, 0xC2, 0x01]); // test dl, 1
    code.extend_from_slice(&[0x0F, 0x84]); // jz s1_even
    let s1_even_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xD1, 0xF8]); // sar rax, 1
    code.extend_from_slice(&[0xE9]);
    let s1_mul_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    let s1_even = code.len();
    patch_rel32_to(code, s1_even_site, s1_even);
    code.extend_from_slice(&[0x48, 0xD1, 0xEA]); // shr rdx, 1
    let s1_mul = code.len();
    patch_rel32_to(code, s1_mul_site, s1_mul);
    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC2]); // imul rax, rdx -> S1

    // acc += c2*S2 + c1*S1 + c0*count.
    code.extend_from_slice(&[0x48, 0x69, 0xC0]); // imul rax, rax, c1  (c1*S1)
    code.extend_from_slice(&plan.c1.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0xDA]); // mov rdx, r11  (S2)
    code.extend_from_slice(&[0x48, 0x69, 0xD2]); // imul rdx, rdx, c2
    code.extend_from_slice(&plan.c2.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx
    code.extend_from_slice(&[0x4C, 0x89, 0xD2]); // mov rdx, r10  (count)
    code.extend_from_slice(&[0x48, 0x69, 0xD2]); // imul rdx, rdx, c0
    code.extend_from_slice(&plan.c0.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx

    match plan.acc_reg {
        Some(reg) => reg.add_rax(code),
        None => {
            code.extend_from_slice(&[0x48, 0x01, 0x85]); // add [rbp + disp32], rax
            code.extend_from_slice(&(-plan.acc_slot).to_le_bytes());
        }
    }

    let done = code.len();
    patch_rel32_to(code, done_site, done);
    Ok(())
}
