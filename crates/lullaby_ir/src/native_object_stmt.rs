//! Native statement, loop, and aggregate lowering: register promotion, the
//! per-function lowering driver, statement/aggregate-init lowering, the SSE2/SSE4.2
//! auto-vectorizers, and RC scope-based drop insertion. Split out of
//! native_object.rs; recurses into expression/op lowering via `use super::*`.

use super::*;

// -- Scalar local register promotion -----------------------------------------
//
// A purely-i64-scalar function's lowering only ever touches the caller-saved
// scratch registers (rax/rcx/rdx/r8/r9); the callee-saved rbx/rsi are used only
// by the shared `.text` string/aggregate helpers, which save and restore them.
// So for such a function we can keep a couple of its hot `i64` locals in rbx/rsi
// for the whole body instead of the stack — and because they are callee-saved,
// they survive every `call` (each callee that uses them saves/restores them),
// exactly as a C compiler keeps a hot local in a register across recursion.
//
// This is deliberately conservative: any construct that could stray outside the
// scalar register set (strings, floats, aggregates, arrays, indexing, `for`/
// `match`, non-i64 params) disqualifies the whole function, which then keeps its
// existing, unchanged codegen. Correctness never depends on the analysis being
// generous — only on it never promoting a function that isn't purely scalar.

/// A callee-saved register a scalar `i64` local can be promoted into.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PReg {
    Rbx,
    Rsi,
}

// The `to_rax`/`from_rax`/`from_arg` names are the byte emitter's intentional
// direction-of-move vocabulary (rax←reg vs reg←rax), not a fallible constructor
// convention, so the `wrong_self_convention` heuristic does not apply here.
#[allow(clippy::wrong_self_convention)]
impl PReg {
    /// `mov rax, <reg>`.
    pub(crate) fn to_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x89, 0xD8],
            PReg::Rsi => &[0x48, 0x89, 0xF0],
        });
    }
    /// `mov <reg>, rax`.
    pub(crate) fn from_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x89, 0xC3],
            PReg::Rsi => &[0x48, 0x89, 0xC6],
        });
    }
    /// `mov <reg>, <arg-register>` where arg is the Win64 integer arg index
    /// (0..3 = rcx/rdx/r8/r9). Used to seat a promoted parameter on entry.
    fn from_arg(self, code: &mut Vec<u8>, arg: usize) {
        let bytes: &[u8] = match (self, arg) {
            (PReg::Rbx, 0) => &[0x48, 0x89, 0xCB], // mov rbx, rcx
            (PReg::Rbx, 1) => &[0x48, 0x89, 0xD3], // mov rbx, rdx
            (PReg::Rbx, 2) => &[0x4C, 0x89, 0xC3], // mov rbx, r8
            (PReg::Rbx, 3) => &[0x4C, 0x89, 0xCB], // mov rbx, r9
            (PReg::Rsi, 0) => &[0x48, 0x89, 0xCE], // mov rsi, rcx
            (PReg::Rsi, 1) => &[0x48, 0x89, 0xD6], // mov rsi, rdx
            (PReg::Rsi, 2) => &[0x4C, 0x89, 0xC6], // mov rsi, r8
            (PReg::Rsi, 3) => &[0x4C, 0x89, 0xCE], // mov rsi, r9
            _ => unreachable!("promoted parameters are among the first four (register) args"),
        };
        code.extend_from_slice(bytes);
    }
    /// `mov <reg>, [rbp - slot]` (save the incoming callee-saved value into its
    /// spill slot) and its inverse `mov [rbp - slot], <reg>`.
    fn spill_to_slot(self, code: &mut Vec<u8>, slot: i32) {
        // mov [rbp + disp32], <reg>
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x89, 0x9D],
            PReg::Rsi => &[0x48, 0x89, 0xB5],
        });
        code.extend_from_slice(&(-slot).to_le_bytes());
    }
    fn restore_from_slot(self, code: &mut Vec<u8>, slot: i32) {
        // mov <reg>, [rbp + disp32]
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x8B, 0x9D],
            PReg::Rsi => &[0x48, 0x8B, 0xB5],
        });
        code.extend_from_slice(&(-slot).to_le_bytes());
    }
    /// `add/sub <reg>, imm32` and `add/sub <reg>, rax` for the memory-destination
    /// self-assign fast path when the target is promoted.
    pub(crate) fn add_imm(self, code: &mut Vec<u8>, imm: i32) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x81, 0xC3],
            PReg::Rsi => &[0x48, 0x81, 0xC6],
        });
        code.extend_from_slice(&imm.to_le_bytes());
    }
    fn sub_imm(self, code: &mut Vec<u8>, imm: i32) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x81, 0xEB],
            PReg::Rsi => &[0x48, 0x81, 0xEE],
        });
        code.extend_from_slice(&imm.to_le_bytes());
    }
    fn add_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x01, 0xC3],
            PReg::Rsi => &[0x48, 0x01, 0xC6],
        });
    }
    fn sub_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x29, 0xC3],
            PReg::Rsi => &[0x48, 0x29, 0xC6],
        });
    }
    /// `add <self>, <src>` / `sub <self>, <src>` (both promoted registers), for
    /// `acc = acc + i` where the right operand is itself a promoted-register local
    /// — skips the `mov rax, <src>` round-trip through the scratch register.
    /// `add/sub r/m64, r64` is REX.W 01/29 /r; ModRM = 11 <src> <self>.
    pub(crate) fn add_reg(self, code: &mut Vec<u8>, src: PReg) {
        code.extend_from_slice(&[0x48, 0x01, modrm_reg_reg(src, self)]);
    }
    fn sub_reg(self, code: &mut Vec<u8>, src: PReg) {
        code.extend_from_slice(&[0x48, 0x29, modrm_reg_reg(src, self)]);
    }
    /// `cmp <self>, imm32` (sign-extended) — a fused-comparison left operand that
    /// is a promoted register compares directly instead of `mov rax, <reg>` first.
    pub(crate) fn cmp_imm(self, code: &mut Vec<u8>, imm: i32) {
        // cmp r/m64, imm32 -> REX.W 81 /7 id ; ModRM = 11 111 <reg>.
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x81, 0xFB],
            PReg::Rsi => &[0x48, 0x81, 0xFE],
        });
        code.extend_from_slice(&imm.to_le_bytes());
    }
    /// The register's 3-bit encoding in a ModRM field.
    fn code3(self) -> u8 {
        match self {
            PReg::Rbx => 3,
            PReg::Rsi => 6,
        }
    }
}

/// ModRM byte for a register-direct `op r/m64, r64`: mod=11, reg=source (the
/// `/r` field), rm=destination.
pub(crate) fn modrm_reg_reg(src: PReg, dst: PReg) -> u8 {
    0xC0 | (src.code3() << 3) | dst.code3()
}

/// If `expr` is a bare local variable currently promoted into a callee-saved
/// register, return that register so a consumer can read it directly instead of
/// materializing it into `rax` first. Any non-variable, unresolvable, or
/// stack-resident local returns `None` (the unchanged `mov rax, …` path).
pub(crate) fn promoted_var_reg(ctx: &NativeCtx, expr: &BytecodeExpr) -> Option<PReg> {
    if let BytecodeExprKind::Variable(name) = &expr.kind {
        let slot = ctx.local_slot(name).ok()?;
        return ctx.promoted_reg(slot);
    }
    None
}

/// Whether an expression lowers entirely within the scalar register set (never
/// touching rbx/rsi). Conservative: only plain `i64` integer arithmetic/
/// comparison over `i64` operands, `i64` variables/literals, and all-`i64` calls.
pub(crate) fn expr_reg_promotable(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        BytecodeExprKind::Integer(_) | BytecodeExprKind::Bool(_) | BytecodeExprKind::Char(_) => {
            true
        }
        BytecodeExprKind::Variable(_) => expr.ty.name == "i64",
        BytecodeExprKind::Unary { expr: inner, .. } => {
            inner.ty.name == "i64" && expr_reg_promotable(inner)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            left.ty.name == "i64"
                && right.ty.name == "i64"
                && expr_reg_promotable(left)
                && expr_reg_promotable(right)
        }
        BytecodeExprKind::Call { args, .. } => {
            expr.ty.name == "i64"
                && args
                    .iter()
                    .all(|a| a.ty.name == "i64" && expr_reg_promotable(a))
        }
        _ => false,
    }
}

/// Whether an instruction lowers entirely within the scalar register set.
pub(crate) fn instr_reg_promotable(instr: &BytecodeInstruction) -> bool {
    match instr {
        BytecodeInstruction::Let { ty, value, .. } => {
            ty.name == "i64" && expr_reg_promotable(value)
        }
        // A path-less assignment to a scalar local (no field/index hop).
        BytecodeInstruction::Assign { path, value, .. } => {
            path.is_empty() && expr_reg_promotable(value)
        }
        BytecodeInstruction::Return(Some(e)) => expr_reg_promotable(e),
        BytecodeInstruction::Return(None) => true,
        BytecodeInstruction::Expr(e) => expr_reg_promotable(e),
        BytecodeInstruction::Break(_) | BytecodeInstruction::Continue(_) => true,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().all(|b| {
                expr_reg_promotable(&b.condition) && b.body.iter().all(instr_reg_promotable)
            }) && else_body.iter().all(instr_reg_promotable)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_reg_promotable(condition) && body.iter().all(instr_reg_promotable),
        // A range `for` is promotable when its bounds/step and body are scalar.
        // The counter and its hidden `__end`/`__step` slots stay on the stack
        // (see `for_counter_slots`), because `lower_native_for` accesses them
        // directly; the body's other scalar locals (accumulators) get registers.
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_reg_promotable(start)
                && expr_reg_promotable(end)
                && step.as_ref().is_none_or(expr_reg_promotable)
                && body.iter().all(instr_reg_promotable)
        }
        // Loop / Match / Asm / Throw / Try are conservatively excluded.
        _ => false,
    }
}

/// Collect the stack slots that a range `for` needs to keep off registers: each
/// loop's hidden `{name}__end` / `{name}__step` bound/step slots, which
/// `lower_native_for` reads as stack memory operands. The counter itself may be
/// promoted — `lower_native_for` honors `promoted_reg` for it.
pub(crate) fn for_counter_slots(
    instrs: &[BytecodeInstruction],
    locals: &HashMap<String, NativeLocal>,
    out: &mut std::collections::HashSet<i32>,
) {
    for instr in instrs {
        match instr {
            BytecodeInstruction::For { name, body, .. } => {
                for key in [format!("{name}__end"), format!("{name}__step")] {
                    if let Some(local) = locals.get(&key) {
                        out.insert(local.slot);
                    }
                }
                for_counter_slots(body, locals, out);
            }
            BytecodeInstruction::While { body, .. } | BytecodeInstruction::Loop { body, .. } => {
                for_counter_slots(body, locals, out)
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    for_counter_slots(&branch.body, locals, out);
                }
                for_counter_slots(else_body, locals, out);
            }
            BytecodeInstruction::Match { arms, .. } => {
                for arm in arms {
                    for_counter_slots(&arm.body, locals, out);
                }
            }
            _ => {}
        }
    }
}

/// Decide which of a purely-scalar function's `i64` locals to keep in callee-saved
/// registers. Returns (local-slot -> register) and the ordered registers to
/// preserve. Empty (no promotion) unless the whole function is scalar-only with
/// `i64` params and an `i64` return.
pub(crate) fn plan_register_promotion(
    function: &BytecodeFunction,
    locals: &HashMap<String, NativeLocal>,
) -> (HashMap<i32, PReg>, Vec<PReg>) {
    let none = (HashMap::new(), Vec::new());
    if function.return_type.name != "i64" {
        return none;
    }
    if !function.params.iter().all(|p| p.ty.name == "i64") {
        return none;
    }
    if !function.instructions.iter().all(instr_reg_promotable) {
        return none;
    }
    // A range `for`'s counter and its hidden bound/step slots must stay on the
    // stack (`lower_native_for` addresses them as memory), so exclude them.
    let mut excluded = std::collections::HashSet::new();
    for_counter_slots(&function.instructions, locals, &mut excluded);
    // Rank the promotable `i64` locals by loop-weighted usage and keep the two
    // busiest in registers. A use inside a loop counts far more than a
    // straight-line one, so a hot loop counter/accumulator wins over a
    // loop-invariant parameter — which is what makes patterns like the
    // runtime-bound counting-sum reduction (`while i < n`) fire. Correctness does
    // not depend on the choice (any local is correct promoted or on the stack);
    // this only picks the fastest two.
    let mut scores: HashMap<String, u64> = HashMap::new();
    score_instr_usage(&function.instructions, 0, &mut scores);
    let mut candidates: Vec<(u64, i32)> = locals
        .iter()
        .filter(|(_, l)| matches!(l.ty, NativeType::I64))
        .filter(|(_, l)| !excluded.contains(&l.slot))
        .map(|(name, l)| (scores.get(name).copied().unwrap_or(0), l.slot))
        .collect();
    // Most-used first; ties broken by slot ascending so the choice is
    // deterministic regardless of the locals map's iteration order.
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let regs = [PReg::Rbx, PReg::Rsi];
    let mut promoted = HashMap::new();
    let mut saved = Vec::new();
    for ((_, slot), reg) in candidates.into_iter().zip(regs) {
        promoted.insert(slot, reg);
        saved.push(reg);
    }
    (promoted, saved)
}

/// Add each `i64`-local occurrence in `expr` to `scores`, weighted by `weight`
/// (the enclosing loop-nesting frequency estimate). Reads of a `Variable` count;
/// literals and other leaves do not.
fn score_expr_usage(expr: &BytecodeExpr, weight: u64, scores: &mut HashMap<String, u64>) {
    match &expr.kind {
        BytecodeExprKind::Variable(name) => {
            *scores.entry(name.clone()).or_default() += weight;
        }
        BytecodeExprKind::Index { target, index } => {
            score_expr_usage(target, weight, scores);
            score_expr_usage(index, weight, scores);
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            score_expr_usage(expr, weight, scores);
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            score_expr_usage(left, weight, scores);
            score_expr_usage(right, weight, scores);
        }
        BytecodeExprKind::Call { args, .. } | BytecodeExprKind::Array(args) => {
            for arg in args {
                score_expr_usage(arg, weight, scores);
            }
        }
        BytecodeExprKind::Field { target, .. } => score_expr_usage(target, weight, scores),
        _ => {}
    }
}

/// Accumulate loop-weighted usage counts for every local across `instrs`. Each
/// loop level multiplies a use's weight (`8^depth`, capped), so loop-carried
/// locals dominate. Both reads and assignment/binding targets count.
fn score_instr_usage(
    instrs: &[BytecodeInstruction],
    depth: u32,
    scores: &mut HashMap<String, u64>,
) {
    let weight = 8u64.saturating_pow(depth.min(8));
    // A loop's condition and body run at the inner (per-iteration) frequency.
    let inner = weight.saturating_mul(8);
    for instr in instrs {
        match instr {
            BytecodeInstruction::Let { name, value, .. } => {
                *scores.entry(name.clone()).or_default() += weight;
                score_expr_usage(value, weight, scores);
            }
            BytecodeInstruction::Assign { name, value, .. } => {
                *scores.entry(name.clone()).or_default() += weight;
                score_expr_usage(value, weight, scores);
            }
            BytecodeInstruction::Return(Some(expr))
            | BytecodeInstruction::Expr(expr)
            | BytecodeInstruction::Throw { value: expr, .. } => {
                score_expr_usage(expr, weight, scores);
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    score_expr_usage(&branch.condition, weight, scores);
                    score_instr_usage(&branch.body, depth, scores);
                }
                score_instr_usage(else_body, depth, scores);
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => {
                score_expr_usage(condition, inner, scores);
                score_instr_usage(body, depth + 1, scores);
            }
            BytecodeInstruction::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                *scores.entry(name.clone()).or_default() += inner;
                score_expr_usage(start, weight, scores);
                score_expr_usage(end, weight, scores);
                if let Some(step) = step {
                    score_expr_usage(step, weight, scores);
                }
                score_instr_usage(body, depth + 1, scores);
            }
            BytecodeInstruction::Loop { body, .. } => score_instr_usage(body, depth + 1, scores),
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                score_expr_usage(scrutinee, weight, scores);
                for arm in arms {
                    score_instr_usage(&arm.body, depth, scores);
                }
            }
            BytecodeInstruction::Try {
                body, catch_body, ..
            } => {
                score_instr_usage(body, depth, scores);
                score_instr_usage(catch_body, depth, scores);
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_native_function(
    function: &BytecodeFunction,
    callable: &std::collections::HashSet<&str>,
    extern_sigs: &HashMap<&str, &crate::IrExternSignature>,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    strings: &mut StringPool,
    signatures: &HashMap<String, NativeSignature>,
    array_lengths: &ArrayLengths,
    fast_math: bool,
) -> Result<LoweredNativeFunction, String> {
    let mut ctx = NativeCtx::plan(
        function,
        callable,
        extern_sigs,
        structs,
        enums,
        strings,
        signatures,
        array_lengths,
    )?;
    ctx.fast_math = fast_math;
    let mut code = Vec::new();

    // Prologue: push rbp; mov rbp, rsp; sub rsp, frame_size.
    code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5]);
    emit_sub_rsp(&mut code, ctx.frame_size);

    // Preserve the callee-saved registers used to hold promoted locals, spilling
    // each caller value into its reserved frame slot (restored in the epilogue).
    // Done before parameters are seated so a promoted parameter can overwrite its
    // register next.
    for (reg, slot) in &ctx.saved_reg_slots {
        reg.spill_to_slot(&mut code, *slot);
    }

    // Register argument order: `mov [rbp - slot], reg`. When the function returns
    // an aggregate, the hidden result pointer consumes the first register (rcx),
    // shifting the visible parameters down by one.
    const PARAM_STORE: [&[u8]; 4] = [
        &[0x48, 0x89, 0x8D], // mov [rbp+disp32], rcx
        &[0x48, 0x89, 0x95], // mov [rbp+disp32], rdx
        &[0x4C, 0x89, 0x85], // mov [rbp+disp32], r8
        &[0x4C, 0x89, 0x8D], // mov [rbp+disp32], r9
    ];
    // Load an integer argument register (by index) into rax: `mov rax, reg`.
    const ARG_TO_RAX: [&[u8]; 4] = [
        &[0x48, 0x89, 0xC8], // mov rax, rcx
        &[0x48, 0x89, 0xD0], // mov rax, rdx
        &[0x4C, 0x89, 0xC0], // mov rax, r8
        &[0x4C, 0x89, 0xC8], // mov rax, r9
    ];

    // The hidden return pointer (if any) is register 0; parameters follow.
    let mut reg = 0usize;
    if let Some(sret_slot) = ctx.sret_slot {
        // Spill the caller-provided result pointer into its frame slot.
        code.extend_from_slice(PARAM_STORE[reg]);
        code.extend_from_slice(&(-sret_slot).to_le_bytes());
        reg += 1;
    }
    for param in &function.params {
        let local = ctx.local(&param.name)?.clone();
        // Arguments 5, 6, … (register slots 4, 5, … already consumed) arrive on the
        // stack above the caller's 32-byte shadow space. On entry the callee sees
        // the return address at `[rsp]`; after `push rbp` + `mov rbp, rsp` the saved
        // rbp is at `[rbp]`, the return address at `[rbp+8]`, the caller's shadow at
        // `[rbp+16 .. rbp+48]`, and the first stack argument at `[rbp+48]`. So the
        // Nth stack argument (0-indexed `reg-4`) sits at `[rbp + 48 + 8*(reg-4)]`.
        // The first four (`reg < 4`) arrive in registers.
        let on_stack = reg >= 4;
        let stack_disp = 48 + (reg as i32 - 4) * 8;
        match local.ty {
            NativeType::F64 | NativeType::F32 => {
                if on_stack {
                    // A float stack argument is already a raw 8-byte word; copy it
                    // bit-for-bit into the parameter's slot (the slot holds the raw
                    // float bits, so no XMM round-trip is needed).
                    emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                    store_local(&mut code, local.slot);
                } else {
                    // A float register parameter arrives in the SSE register at this
                    // position (`xmm N`, positionally aligned with the integer
                    // registers). Spill it into the parameter's slot.
                    let width = match local.ty {
                        NativeType::F64 => FloatWidth::F64,
                        NativeType::F32 => FloatWidth::F32,
                        _ => unreachable!("guarded by the match arm"),
                    };
                    emit_store_xmm_to_slot(&mut code, reg as u8, local.slot, width);
                }
            }
            NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. } => {
                // The argument holds a pointer to the caller's copy (in a register
                // for `reg < 4`, on the stack otherwise). Copy the aggregate words
                // into the parameter's frame slots (value semantics: the callee owns
                // an independent snapshot and never mutates the caller's copy). rax =
                // source pointer (addresses word 0, the aggregate's highest stack
                // address). Words descend in memory, so word k is at `[rax - 8*k]`,
                // matching the caller's `[rbp - (base + 8*k)]` layout.
                if on_stack {
                    emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                } else {
                    code.extend_from_slice(ARG_TO_RAX[reg]);
                }
                for word in 0..local.ty.words() as i32 {
                    // mov rcx, [rax - 8*word]
                    emit_mov_rcx_from_rax_disp(&mut code, -word * 8);
                    // mov [rbp - (slot + 8*word)], rcx
                    emit_mov_slot_from_rcx(&mut code, local.slot + word * 8);
                }
            }
            NativeType::I64
            | NativeType::String
            | NativeType::List { .. }
            | NativeType::Map { .. }
            | NativeType::HeapStruct { .. } => {
                // An integer/pointer scalar parameter — or a string/list/map/heap
                // struct (a heap pointer word) — spills its register (or its incoming
                // stack word)
                // directly into its slot. A string parameter shares the caller's
                // record by pointer, which is safe because strings are immutable. A
                // list/map parameter also shares by pointer safely: their mutators
                // (`push`/`set`/`pop`, `map_set`) deep-copy their source, so the
                // callee cannot alter the caller's value through the shared pointer.
                // A promoted parameter is seated in its callee-saved register
                // instead of a stack slot (promotion only picks i64 params, which
                // are among the first register args, so `from_arg` always applies;
                // the on-stack arm is defensive).
                match (on_stack, ctx.promoted_reg(local.slot)) {
                    (false, Some(preg)) => preg.from_arg(&mut code, reg),
                    (true, Some(preg)) => {
                        emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                        preg.from_rax(&mut code);
                    }
                    (false, None) => {
                        code.extend_from_slice(PARAM_STORE[reg]);
                        code.extend_from_slice(&(-local.slot).to_le_bytes());
                    }
                    (true, None) => {
                        emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                        store_local(&mut code, local.slot);
                    }
                }
            }
        }
        reg += 1;
    }

    let mut loops: Vec<NativeLoop> = Vec::new();

    // A function whose last statement is a value-producing tail expression (e.g.
    // a body of just `a + b`) returns that value. Lower the leading statements
    // normally, then lower the tail expression and emit the return epilogue so
    // the result in rax is returned rather than being clobbered by the
    // fallthrough safety epilogue below.
    let instructions = &function.instructions;
    let tail_is_value_expr = matches!(
        instructions.last(),
        Some(BytecodeInstruction::Expr(expr)) if !expr.ty.is_void()
    );
    // A function whose last statement is an `asm` block is trusted to leave the
    // return value in `rax`. Lower the head, emit the asm bytes, then emit the
    // normal epilogue so `rax` is returned intact rather than being clobbered by
    // the fallthrough `xor eax,eax` below. (The programmer must not emit their
    // own `ret` — the epilogue restores `rbp` and `rsp` and returns.)
    let tail_is_asm = matches!(instructions.last(), Some(BytecodeInstruction::Asm { .. }));
    // A function whose last statement is a `match` producing the function's value:
    // each arm leaves its result in `rax`; after the whole match, the epilogue
    // returns it. (An arm that itself ends in an explicit `return` emits its own
    // epilogue and never reaches the shared match end.)
    let tail_is_value_match = !function.return_type.is_void()
        && matches!(instructions.last(), Some(BytecodeInstruction::Match { .. }));
    // A function whose last statement is an `if`/`elif`/`else` producing the
    // function's value (e.g. a body ending in `if c\n a\n else\n b`): each branch
    // leaves its value in `rax` and converges after the chain, where the epilogue
    // returns it. Without this the tail `if` lowers as a plain statement and the
    // fallthrough `xor rax,rax` below overwrites the branch result (returning 0).
    // Restricted to a scalar-register return (not float/aggregate — those tail
    // `if`s stay deferred): a value-producing `if` is exhaustive (has an `else`),
    // so control always reaches the epilogue with `rax` set.
    let tail_is_value_if = !function.return_type.is_void()
        && ctx.sret_slot.is_none()
        && !matches!(ctx.return_ty, NativeType::F64 | NativeType::F32)
        && matches!(instructions.last(), Some(BytecodeInstruction::If { .. }));
    if tail_is_asm {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Asm { bytes, .. } = &tail[0] {
            code.extend_from_slice(bytes);
        }
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_expr {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Expr(expr) = &tail[0] {
            // An aggregate-valued tail expression is the function's by-pointer
            // result: materialize it through the hidden return pointer. A float
            // tail expression leaves its value in `xmm0` (the Win64 SSE return
            // register). A scalar tail expression leaves its value in rax.
            if ctx.sret_slot.is_some() {
                lower_aggregate_return(&mut ctx, expr, &mut code)?;
            } else if matches!(ctx.return_ty, NativeType::F64 | NativeType::F32) {
                lower_native_float_expr(&mut ctx, expr, &mut code)?;
            } else {
                lower_native_expr(&mut ctx, expr, &mut code)?;
            }
        }
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_match {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Match {
            scrutinee, arms, ..
        } = &tail[0]
        {
            lower_native_match(&mut ctx, scrutinee, arms, true, &mut code, &mut loops)?;
        }
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_if {
        // The tail `if` lowers as a statement; each branch leaves the function's
        // value in rax and jumps to the convergence point right before this
        // epilogue, which returns it. Emitting the epilogue here makes the
        // fallthrough `xor rax,rax` below unreachable (dead safety code).
        lower_native_stmts(&mut ctx, instructions, &mut code, &mut loops)?;
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else {
        lower_native_stmts(&mut ctx, instructions, &mut code, &mut loops)?;
    }

    // Fallthrough epilogue: functions in this subset are non-void and expected to
    // return on every path, but emit a safe `xor eax,eax` + epilogue so a missing
    // tail return cannot run off the end of the section.
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
    emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);

    Ok(LoweredNativeFunction {
        name: function.name.clone(),
        code,
        relocations: ctx.relocations,
        line: function.span.line as u32,
    })
}

/// Emit `sub rsp, imm` (imm >= 0). Uses imm8 form when it fits, else imm32.
pub(crate) fn emit_sub_rsp(code: &mut Vec<u8>, amount: i32) {
    if amount == 0 {
        return;
    }
    if (0..=127).contains(&amount) {
        code.extend_from_slice(&[0x48, 0x83, 0xEC, amount as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x81, 0xEC]);
        code.extend_from_slice(&amount.to_le_bytes());
    }
}

/// Emit the function epilogue: restore any promoted callee-saved registers from
/// their spill slots (rbp-relative, still valid), then `add rsp, imm; pop rbp;
/// ret`. `saved_reg_slots` is empty for functions without register promotion.
pub(crate) fn emit_native_epilogue(
    code: &mut Vec<u8>,
    frame_size: i32,
    saved_reg_slots: &[(PReg, i32)],
) {
    for (reg, slot) in saved_reg_slots {
        reg.restore_from_slot(code, *slot);
    }
    if frame_size != 0 {
        if (0..=127).contains(&frame_size) {
            code.extend_from_slice(&[0x48, 0x83, 0xC4, frame_size as u8]);
        } else {
            code.extend_from_slice(&[0x48, 0x81, 0xC4]);
            code.extend_from_slice(&frame_size.to_le_bytes());
        }
    }
    code.extend_from_slice(&[0x5D, 0xC3]); // pop rbp; ret
}

pub(crate) fn lower_native_stmts(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    for stmt in body {
        lower_native_stmt(ctx, stmt, code, loops)?;
    }
    Ok(())
}

pub(crate) fn lower_native_stmt(
    ctx: &mut NativeCtx,
    stmt: &BytecodeInstruction,
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    match stmt {
        BytecodeInstruction::Let { name, value, .. } => {
            // A scalar `let` uses the register path; a float `let` evaluates into
            // xmm0 and stores the whole word; an aggregate `let` materializes each
            // flattened scalar word directly into its slots.
            match ctx.local(name)?.ty {
                // An `i64` scalar or a string/list/map/heap-struct (a pointer word)
                // uses the register path: evaluate into `rax` and store the whole
                // word. (A `HeapStruct` never appears as a top-level local; kept in
                // this arm for match exhaustiveness.)
                NativeType::I64
                | NativeType::String
                | NativeType::List { .. }
                | NativeType::Map { .. }
                | NativeType::HeapStruct { .. } => {
                    lower_native_expr(ctx, value, code)?;
                    let slot = ctx.local_slot(name)?;
                    match ctx.promoted_reg(slot) {
                        Some(reg) => reg.from_rax(code), // mov <reg>, rax
                        None => store_local(code, slot), // mov [rbp - slot], rax
                    }
                }
                NativeType::F64 | NativeType::F32 => {
                    let slot = ctx.local_slot(name)?;
                    let width = lower_native_float_expr(ctx, value, code)?;
                    store_float_local(code, slot, width); // movs[sd] [rbp - slot], xmm0
                }
                NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. } => {
                    let base = ctx.local(name)?.slot;
                    let ty = ctx.local(name)?.ty.clone();
                    lower_aggregate_init(ctx, base, &ty, value, code)?;
                }
            }
            Ok(())
        }
        BytecodeInstruction::Assign {
            name,
            path,
            op,
            value,
            ..
        } => {
            // A float local assigned through no path (a plain `f = ...`) uses the
            // XMM path. A float target reached through a struct/array path is out
            // of scope (float aggregate members are rejected at layout time), so
            // only the pathless case can be a float here.
            if path.is_empty()
                && let NativeType::F64 | NativeType::F32 = ctx.local(name)?.ty
            {
                let slot = ctx.local_slot(name)?;
                let store_width = match ctx.local(name)?.ty {
                    NativeType::F64 => FloatWidth::F64,
                    NativeType::F32 => FloatWidth::F32,
                    _ => unreachable!("guarded above"),
                };
                match op {
                    AssignOp::Replace => {
                        let width = lower_native_float_expr(ctx, value, code)?;
                        debug_assert_eq!(width, store_width, "float assign width mismatch");
                        store_float_local(code, slot, store_width);
                    }
                    AssignOp::Add | AssignOp::Subtract | AssignOp::Multiply | AssignOp::Divide => {
                        // `f op= rhs`: load current into xmm0, rhs into xmm1, apply
                        // the scalar op, re-store. `op` maps to the same arithmetic
                        // as the binary form.
                        let bin = match op {
                            AssignOp::Add => BinaryOp::Add,
                            AssignOp::Subtract => BinaryOp::Subtract,
                            AssignOp::Multiply => BinaryOp::Multiply,
                            AssignOp::Divide => BinaryOp::Divide,
                            AssignOp::Replace | AssignOp::Remainder => unreachable!(),
                        };
                        // Compute the RHS into xmm0, spill it, load current into
                        // xmm0, restore RHS into xmm1, then apply left <op> right.
                        let rhs_width = lower_native_float_expr(ctx, value, code)?;
                        debug_assert_eq!(rhs_width, store_width, "float assign width mismatch");
                        push_xmm0(code); // save RHS
                        load_float_local(code, slot, store_width); // xmm0 = current (left)
                        pop_xmm1(code); // xmm1 = RHS (right)
                        emit_float_arith(code, bin, store_width);
                        store_float_local(code, slot, store_width);
                    }
                    AssignOp::Remainder => {
                        unreachable!("`%=` requires integer operands (rejected by semantics)")
                    }
                }
                return Ok(());
            }
            // A path-less whole-value assignment to an enum (or other aggregate)
            // local re-materializes the value into the local's words. Only
            // `Replace` is meaningful for an aggregate (there is no `+=` on a
            // struct/array/enum), so a compound op on one is rejected as a skip.
            if path.is_empty()
                && matches!(
                    ctx.local(name)?.ty,
                    NativeType::Struct { .. } | NativeType::Array { .. } | NativeType::Enum { .. }
                )
            {
                if !matches!(op, AssignOp::Replace) {
                    return Err("compound assignment on an aggregate is not supported".to_string());
                }
                let base = ctx.local(name)?.slot;
                let ty = ctx.local(name)?.ty.clone();
                return lower_aggregate_init(ctx, base, &ty, value, code);
            }
            // A path-less whole-value assignment to a string, list, or map local
            // (`s = a + b`, `l = push(l, x)`, `l = list_new()`,
            // `m = map_set(m, k, v)`, `m = map_new()`, …) re-stores the pointer word
            // through the register path. Only `Replace` is meaningful for such a
            // pointer value; a compound op is a skip. (String `+` is concatenation,
            // which yields a fresh record — a whole-value `Replace`, never a `+=`.)
            if path.is_empty()
                && matches!(
                    ctx.local(name)?.ty,
                    NativeType::String | NativeType::List { .. } | NativeType::Map { .. }
                )
            {
                if !matches!(op, AssignOp::Replace) {
                    return Err(
                        "compound assignment on a string, list, or map is not supported"
                            .to_string(),
                    );
                }
                lower_native_expr(ctx, value, code)?;
                let slot = ctx.local_slot(name)?;
                store_local(code, slot);
                return Ok(());
            }
            // A float array element / float struct field store (`a[i] = <f64>`):
            // resolve permitting a float element and store through xmm0. Only a
            // plain `Replace` is supported (a float compound `a[i] += ...` is
            // deferred, mirroring the string/list rejection above).
            let (typed_place, elem_ty) = resolve_scalar_place_typed(ctx, name, path)?;
            if matches!(elem_ty, NativeType::F64 | NativeType::F32) {
                if !matches!(op, AssignOp::Replace) {
                    return Err(
                        "compound assignment on a float array element is not supported".to_string(),
                    );
                }
                let width = match elem_ty {
                    NativeType::F32 => FloatWidth::F32,
                    _ => FloatWidth::F64,
                };
                match typed_place {
                    ScalarPlace::Const { slot } => {
                        lower_native_float_expr(ctx, value, code)?; // xmm0 = value
                        store_float_local(code, slot, width);
                    }
                    ScalarPlace::Dynamic { .. } => {
                        lower_native_float_expr(ctx, value, code)?; // xmm0 = value
                        push_xmm0(code); // spill (address calc clobbers gprs)
                        emit_dynamic_addr_into_rcx(ctx, &typed_place, code)?; // rcx = &elem
                        pop_xmm0(code); // xmm0 = value
                        store_float_from_rcx(code, width); // movsd [rcx], xmm0
                    }
                }
                return Ok(());
            }
            let place = resolve_scalar_place(ctx, name, path)?;
            match op {
                AssignOp::Replace => {
                    // Evaluate the RHS, then store into the resolved scalar slot.
                    match place {
                        ScalarPlace::Const { slot } => {
                            // `x = x + rhs` / `x = x - rhs`, where the assigned
                            // local is also the left operand, folds the update into
                            // the destination: a memory-destination `add`/`sub
                            // [rbp-slot], …`, or `add`/`sub <reg>, …` when the target
                            // is a promoted register — skipping the load of the
                            // target and the store back (the dominant per-iteration
                            // cost in a counting loop). Plain i64 only (fixed-width
                            // kinds need width re-normalization; floats/aggregates
                            // handled above), and only when the left operand
                            // resolves to this exact slot. `add`/`sub` keep the low
                            // 64 bits, matching the interpreters' wrapping add/sub.
                            if let BytecodeExprKind::Binary {
                                left,
                                op: bop,
                                right,
                            } = &value.kind
                                && matches!(bop, BinaryOp::Add | BinaryOp::Subtract)
                                && left.ty.name == "i64"
                                && right.ty.name == "i64"
                                && let BytecodeExprKind::Variable(lname) = &left.kind
                                && ctx.local_slot(lname).ok() == Some(slot)
                            {
                                let is_add = matches!(bop, BinaryOp::Add);
                                let imm = match &right.kind {
                                    BytecodeExprKind::Integer(rhs) => i32::try_from(*rhs).ok(),
                                    _ => None,
                                };
                                match ctx.promoted_reg(slot) {
                                    Some(reg) => match imm {
                                        Some(imm) if is_add => reg.add_imm(code, imm),
                                        Some(imm) => reg.sub_imm(code, imm),
                                        None => {
                                            // `acc = acc + rhs`: if `rhs` is itself a
                                            // promoted-register local, add/sub the two
                                            // registers directly, skipping the `mov rax,
                                            // <rhs>` round-trip.
                                            match promoted_var_reg(ctx, right) {
                                                Some(src) if is_add => reg.add_reg(code, src),
                                                Some(src) => reg.sub_reg(code, src),
                                                None => {
                                                    lower_native_expr(ctx, right, code)?; // rhs → rax
                                                    if is_add {
                                                        reg.add_rax(code)
                                                    } else {
                                                        reg.sub_rax(code)
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    None => {
                                        let disp = (-slot).to_le_bytes();
                                        match imm {
                                            Some(imm) => {
                                                // add/sub qword ptr [rbp-slot], imm32
                                                let modrm = if is_add { 0x85 } else { 0xAD };
                                                code.extend_from_slice(&[0x48, 0x81, modrm]);
                                                code.extend_from_slice(&disp);
                                                code.extend_from_slice(&imm.to_le_bytes());
                                            }
                                            None => {
                                                lower_native_expr(ctx, right, code)?; // rhs → rax
                                                // add/sub qword ptr [rbp-slot], rax
                                                let opcode = if is_add { 0x01 } else { 0x29 };
                                                code.extend_from_slice(&[0x48, opcode, 0x85]);
                                                code.extend_from_slice(&disp);
                                            }
                                        }
                                    }
                                }
                                return Ok(());
                            }
                            lower_native_expr(ctx, value, code)?;
                            match ctx.promoted_reg(slot) {
                                Some(reg) => reg.from_rax(code),
                                None => store_local(code, slot),
                            }
                        }
                        ScalarPlace::Dynamic { .. } => {
                            lower_native_expr(ctx, value, code)?;
                            code.push(0x50); // push rax (value)
                            emit_dynamic_addr_into_rcx(ctx, &place, code)?; // rcx = &slot
                            code.push(0x58); // pop rax (value)
                            code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
                        }
                    }
                }
                other => {
                    let bin = match other {
                        AssignOp::Add => BinaryOp::Add,
                        AssignOp::Subtract => BinaryOp::Subtract,
                        AssignOp::Multiply => BinaryOp::Multiply,
                        AssignOp::Divide => BinaryOp::Divide,
                        AssignOp::Remainder => BinaryOp::Remainder,
                        AssignOp::Replace => unreachable!(),
                    };
                    match place {
                        ScalarPlace::Const { slot } => {
                            match ctx.promoted_reg(slot) {
                                Some(reg) => reg.to_rax(code), // rax = current
                                None => load_local(code, slot),
                            }
                            code.push(0x50); // push rax (left)
                            lower_native_expr(ctx, value, code)?; // rax = right
                            emit_i64_binop_from_stack(code, bin)?;
                            match ctx.promoted_reg(slot) {
                                Some(reg) => reg.from_rax(code),
                                None => store_local(code, slot),
                            }
                        }
                        ScalarPlace::Dynamic { .. } => {
                            // Compute &slot into rcx and keep it across the op.
                            emit_dynamic_addr_into_rcx(ctx, &place, code)?;
                            code.push(0x51); // push rcx (address)
                            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx] (left)
                            code.push(0x50); // push rax (left)
                            lower_native_expr(ctx, value, code)?; // rax = right
                            emit_i64_binop_from_stack(code, bin)?; // rax = left <op> right
                            code.push(0x59); // pop rcx (address)
                            code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
                        }
                    }
                }
            }
            Ok(())
        }
        BytecodeInstruction::Return(Some(expr)) => {
            if ctx.sret_slot.is_some() {
                lower_aggregate_return(ctx, expr, code)?;
            } else if matches!(ctx.return_ty, NativeType::F64 | NativeType::F32) {
                // A float return leaves its value in `xmm0` (the Win64 SSE return
                // register).
                lower_native_float_expr(ctx, expr, code)?;
            } else {
                lower_native_expr(ctx, expr, code)?;
            }
            emit_native_epilogue(code, ctx.frame_size, &ctx.saved_reg_slots);
            Ok(())
        }
        BytecodeInstruction::Return(None) => {
            Err("native subset functions must return an i64 value".to_string())
        }
        BytecodeInstruction::Expr(expr) => {
            // A tail expression is the function result; a non-tail call result is
            // discarded. Either way, evaluate it (leaving the value in rax).
            lower_native_expr(ctx, expr, code)?;
            Ok(())
        }
        BytecodeInstruction::Break(_) => {
            let loop_ctx = loops.last_mut().ok_or("`break` outside a loop")?;
            // jmp rel32 (target patched at loop end).
            code.push(0xE9);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            loop_ctx.break_sites.push(site);
            Ok(())
        }
        BytecodeInstruction::Continue(_) => {
            let loop_ctx = loops.last_mut().ok_or("`continue` outside a loop")?;
            match loop_ctx.continue_target {
                Some(target) => emit_jmp_to(code, target),
                None => {
                    // Forward jump to the (not-yet-emitted) step block.
                    code.push(0xE9);
                    let site = code.len();
                    code.extend_from_slice(&[0, 0, 0, 0]);
                    loop_ctx.continue_sites.push(site);
                }
            }
            Ok(())
        }
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => lower_native_if(ctx, branches, else_body, code, loops),
        BytecodeInstruction::While {
            condition, body, ..
        } => lower_native_while(ctx, condition, body, code, loops),
        BytecodeInstruction::Loop { body, .. } => lower_native_loop(ctx, body, code, loops),
        BytecodeInstruction::For {
            name,
            start,
            end,
            step,
            body,
            ..
        } => lower_native_for(ctx, name, start, end, step.as_ref(), body, code, loops),
        // Inline assembly: emit the raw x86-64 machine-code bytes verbatim into
        // the current function's `.text` at this point. The programmer is trusted
        // (this is `unsafe`); the bytes are not decoded, relocated, or validated
        // beyond the 0..=255 range check already done in semantics. A block that
        // leaves a value in `rax` (e.g. `mov rax, imm32`) returns it, since the
        // Win64 epilogue returns `rax`.
        BytecodeInstruction::Asm { bytes, .. } => {
            code.extend_from_slice(bytes);
            Ok(())
        }
        BytecodeInstruction::Throw { .. } | BytecodeInstruction::Try { .. } => {
            Err("throw/try is not in the native subset".to_string())
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => lower_native_match(ctx, scrutinee, arms, false, code, loops),
    }
}

// -- Stack aggregate lowering (init, place resolution, addressing) -----------

/// Materialize an aggregate value into the contiguous stack words beginning at
/// `base_slot`. The supported initializer shapes mirror how the IR lowerer
/// represents construction:
///   * an array literal `[e0, e1, ...]` -> each element materialized in turn;
///   * a struct constructor `Call { name: StructName, args }` -> each field in
///     declared order (the IR already reorders named fields);
///   * an enum constructor `Call { name: variant, args }`;
///   * a call to an aggregate-returning function -> the callee writes the result
///     through a hidden pointer; the returned pointer is copied word-by-word;
///   * an aggregate variable `x` -> a word-by-word copy of another local.
pub(crate) fn lower_aggregate_init(
    ctx: &mut NativeCtx,
    base_slot: i32,
    ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // `map_get(m, k) -> option<V>` is a builtin producing an aggregate: materialize
    // the `some(v)`/`none` option directly into `base_slot`. It is matched before
    // the compiled-function case because `map_get` is a builtin, not a `signatures`
    // entry.
    if let BytecodeExprKind::Call { name, args } = &value.kind
        && name == MAP_GET_BUILTIN
        && args.len() == 2
        && supported_map_kv(&args[0].ty).is_some()
    {
        return lower_map_get_into(ctx, base_slot, &value.ty, &args[0], &args[1], code);
    }
    // `checked_<op>(a, b) -> option<T>` is a builtin producing an aggregate:
    // materialize the `some(result)`/`none` option directly into `base_slot`.
    if let BytecodeExprKind::Call { name, args } = &value.kind
        && let Some((ovf_op, OverflowMode::Checked)) = overflow_builtin(name)
        && args.len() == 2
        && let Some(kind) = fixed_int_kind(args[0].ty.name.as_str())
    {
        return lower_native_checked_into(
            ctx, base_slot, &value.ty, ovf_op, kind, &args[0], &args[1], code,
        );
    }
    // `parse_i64(s) -> result<i64, string>` is a builtin producing an aggregate:
    // materialize the `ok(n)`/`err(message)` result directly into `base_slot`.
    if let BytecodeExprKind::Call { name, args } = &value.kind
        && name == "parse_i64"
        && args.len() == 1
        && is_string_type(&args[0].ty)
    {
        return lower_parse_i64_into(ctx, base_slot, &args[0], code);
    }
    // A `get(list<struct>, i)` initializing a stack `Struct` local/scratch: the
    // element is a HEAP struct; `lower_list_get` deep-copies it and leaves the fresh
    // heap pointer in `rax`. Bridge it into the stack-flattened `Struct` layout by
    // flat-copying each field word `[heap + 8*k]` -> `[base + 8*k]` (fields are one
    // word each at the one-level bound). This is the heap->stack seam that lets field
    // access (`p.x`) and the by-pointer call ABI consume a mutable-heap element.
    if let (NativeType::Struct { fields, .. }, BytecodeExprKind::Call { name, args }) =
        (ty, &value.kind)
        && name == LIST_GET_BUILTIN
        && args.len() == 2
        && matches!(
            native_collection_slot(
                &args[0]
                    .ty
                    .list_element()
                    .unwrap_or_else(|| args[0].ty.clone()),
                ctx.structs,
                ctx.enums,
                0,
            ),
            Some(NativeType::HeapStruct { .. })
        )
    {
        lower_list_get(ctx, &args[0], &args[1], code)?; // rax = fresh heap-struct ptr
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (heap ptr)
        for word in 0..fields.len() as i32 {
            // rax = [rcx + 8*word] ; [rbp - (base + 8*word)] = rax.
            code.extend_from_slice(&[0x48, 0x8B, 0x81]); // mov rax, [rcx + disp32]
            code.extend_from_slice(&(word * 8).to_le_bytes());
            store_local(code, base_slot + word * 8);
        }
        return Ok(());
    }
    // A call to a compiled function that returns this aggregate: the callee writes
    // its result through a hidden pointer we supply. We could pass `base_slot`'s
    // address as that pointer directly, but the address must be computed relative
    // to `rbp`; do so and let the call fill it, avoiding a second copy.
    if let BytecodeExprKind::Call { name, .. } = &value.kind
        && ctx.signatures.contains_key(name.as_str())
    {
        // Materialize the call, directing its aggregate result into `base_slot`.
        return lower_aggregate_returning_call(ctx, base_slot, ty, value, code);
    }
    match (&value.kind, ty) {
        (BytecodeExprKind::Array(elements), NativeType::Array { elem, len }) => {
            if elements.len() != *len {
                return Err("array literal length does not match layout".to_string());
            }
            let stride = elem.words() as i32;
            for (index, element) in elements.iter().enumerate() {
                let word = base_slot + index as i32 * stride * 8;
                lower_value_into(ctx, word, elem, element, code)?;
            }
            Ok(())
        }
        (
            BytecodeExprKind::Call { name, args },
            NativeType::Struct {
                name: sname,
                fields,
            },
        ) => {
            if name != sname {
                return Err(format!(
                    "constructor `{name}` does not match struct layout `{sname}`"
                ));
            }
            if args.len() != fields.len() {
                return Err(format!("constructor `{name}` has wrong field count"));
            }
            let mut word = base_slot;
            for (arg, (_, field_ty)) in args.iter().zip(fields.iter()) {
                lower_value_into(ctx, word, field_ty, arg, code)?;
                word += field_ty.words() as i32 * 8;
            }
            Ok(())
        }
        (
            BytecodeExprKind::Call { name, args },
            NativeType::Enum {
                variants,
                payload_words,
                ..
            },
        ) => lower_enum_construction(ctx, base_slot, variants, *payload_words, name, args, code),
        (BytecodeExprKind::Variable(source), _) => {
            // Aggregate copy: duplicate the source local word-by-word.
            let src = ctx.local(source)?;
            if &src.ty != ty {
                return Err("aggregate copy between differing layouts".to_string());
            }
            let src_slot = src.slot;
            for word in 0..ty.words() as i32 {
                load_local(code, src_slot + word * 8);
                store_local(code, base_slot + word * 8);
            }
            Ok(())
        }
        _ => Err("initializer is not a native aggregate constructor".to_string()),
    }
}

/// Materialize an enum value into the words at `base_slot`: word 0 = the
/// variant's discriminant tag, words 1.. = its payload. `payload_words` is the
/// enum's shared payload region width; unused trailing payload words (for a
/// narrower variant) are left untouched — `match` only reads the words the
/// matched variant defines, so stale bytes are never observed. `name` is the
/// constructed variant; `args` its positional payload expressions.
pub(crate) fn lower_enum_construction(
    ctx: &mut NativeCtx,
    base_slot: i32,
    variants: &[NativeEnumVariant],
    _payload_words: usize,
    name: &str,
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let variant = variants
        .iter()
        .find(|v| v.name == name)
        .ok_or_else(|| format!("enum constructor `{name}` is not a variant of the target enum"))?;
    if args.len() != variant.payload.len() {
        return Err(format!(
            "enum constructor `{name}` expects {} payload field(s), got {}",
            variant.payload.len(),
            args.len()
        ));
    }
    // Tag word: mov the discriminant into rax and store it at word 0.
    emit_mov_rax_imm(code, variant.tag);
    store_local(code, base_slot);
    // Payload words follow at base_slot + 8, +16, ... in field order. A float
    // payload word is materialized through xmm0; a scalar through rax.
    let mut word = base_slot + 8;
    for (arg, field_ty) in args.iter().zip(variant.payload.iter()) {
        match field_ty {
            // An integer-cell scalar OR a `string` payload is a single flat word:
            // `lower_native_expr` leaves the value (or the immutable string pointer)
            // in `rax`, stored into the payload word. A string is shared, never
            // deep-copied, so this is its exact value-semantic copy.
            NativeType::I64 | NativeType::String => {
                lower_native_expr(ctx, arg, code)?;
                store_local(code, word);
            }
            NativeType::F64 | NativeType::F32 => {
                let width = lower_native_float_expr(ctx, arg, code)?;
                store_float_local(code, word, width);
            }
            // A one-level MUTABLE-aggregate payload (`HeapStruct`/nested `List`/
            // `Map`): build/deep-copy an INDEPENDENT value pointer (so the enum owns
            // its own snapshot) and store it as the payload word.
            NativeType::HeapStruct { .. } | NativeType::List { .. } | NativeType::Map { .. } => {
                lower_heap_slot_value(ctx, field_ty, arg, code)?;
                store_local(code, word);
            }
            _ => {
                return Err(
                    "enum payload must be a native scalar, `string`, or one-level \
                     mutable aggregate"
                        .to_string(),
                );
            }
        }
        word += field_ty.words() as i32 * 8;
    }
    Ok(())
}

/// Materialize `value` (of layout `ty`) into the stack word(s) at `word_slot`.
/// Scalars go through the register path; nested aggregates recurse.
pub(crate) fn lower_value_into(
    ctx: &mut NativeCtx,
    word_slot: i32,
    ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match ty {
        NativeType::I64 => {
            lower_native_expr(ctx, value, code)?;
            store_local(code, word_slot);
            Ok(())
        }
        // A float array/aggregate element: evaluate into xmm0 and store the whole
        // 8-byte (f64) or 4-byte (f32) word — mirrors the enum-payload float path.
        NativeType::F64 | NativeType::F32 => {
            let width = lower_native_float_expr(ctx, value, code)?;
            store_float_local(code, word_slot, width);
            Ok(())
        }
        _ => lower_aggregate_init(ctx, word_slot, ty, value, code),
    }
}

/// One hop of an aggregate access path: a struct field name or an array index
/// expression. Shared between statement-side (`Assign` path) and read-side
/// (`Field`/`Index` expression) resolution.
pub(crate) enum PathStep<'a> {
    Field(&'a str),
    Index(&'a BytecodeExpr),
}

/// Walk a root local plus a list of field/index steps down to a single scalar
/// word, accumulating a constant word offset and, if a runtime index is
/// encountered, deferring to a `Dynamic` place. A constant integer-literal index
/// folds into the constant offset (so `xs[2]` stays static); any other index
/// expression makes the place dynamic. The final layout must be `i64`.
pub(crate) fn resolve_place_steps(
    ctx: &NativeCtx,
    root: &str,
    steps: &[PathStep],
) -> Result<ScalarPlace, String> {
    // The strict i64-only resolver: every existing scalar/SIMD caller relies on
    // this rejecting a float element, so float arrays never reach the integer
    // load/store or the packed-integer SIMD detectors.
    let (place, ty) = resolve_place_steps_typed(ctx, root, steps)?;
    if ty != NativeType::I64 {
        return Err("native access must resolve to an i64 scalar".to_string());
    }
    Ok(place)
}

/// Like [`resolve_place_steps`] but also accepts an `f64`/`f32` final element and
/// returns the resolved element type, so the float read/store paths can pick
/// `movsd`/`movss`. Kept separate from the strict i64 resolver so the SIMD
/// detectors (which call the strict one) never fire on a float array.
pub(crate) fn resolve_place_steps_typed(
    ctx: &NativeCtx,
    root: &str,
    steps: &[PathStep],
) -> Result<(ScalarPlace, NativeType), String> {
    let local = ctx.local(root)?;
    let base_slot = local.slot;
    let mut ty = local.ty.clone();
    let mut const_words: i64 = 0;
    let mut dynamic: Option<(i64, i64, BytecodeExpr)> = None;

    for step in steps {
        match (step, &ty) {
            (PathStep::Field(field), NativeType::Struct { fields, .. }) => {
                let mut offset = 0i64;
                let mut found = None;
                for (fname, fty) in fields {
                    if fname == *field {
                        found = Some(fty.clone());
                        break;
                    }
                    offset += fty.words() as i64;
                }
                let fty = found.ok_or_else(|| format!("unknown field `{field}`"))?;
                const_words += offset;
                ty = fty;
            }
            (PathStep::Index(index), NativeType::Array { elem, len }) => {
                let stride = elem.words() as i64;
                if let BytecodeExprKind::Integer(literal) = index.kind {
                    // A constant index is bounds-checked at compile time: an
                    // out-of-range literal is rejected so the function skips
                    // gracefully rather than emitting an out-of-bounds access.
                    if literal < 0 || literal >= *len as i64 {
                        return Err(format!(
                            "array index `{literal}` is out of bounds for length {len}"
                        ));
                    }
                    const_words += literal * stride;
                } else if dynamic.is_none() {
                    dynamic = Some((stride, *len as i64, (*index).clone()));
                } else {
                    return Err(
                        "at most one runtime array index is supported per access".to_string()
                    );
                }
                ty = (**elem).clone();
            }
            (PathStep::Field(_), _) => {
                return Err("field access on a non-struct native value".to_string());
            }
            (PathStep::Index(_), _) => {
                return Err("index access on a non-array native value".to_string());
            }
        }
    }

    if !matches!(ty, NativeType::I64 | NativeType::F64 | NativeType::F32) {
        return Err("native access must resolve to an i64 or f64 scalar".to_string());
    }

    let place = match dynamic {
        None => ScalarPlace::Const {
            slot: base_slot + const_words as i32 * 8,
        },
        Some((elem_words, index_len, index)) => ScalarPlace::Dynamic {
            base_slot,
            const_words,
            elem_words,
            index_len,
            index,
        },
    };
    Ok((place, ty))
}

/// Read-place decomposition (like [`resolve_read_place`]) that also permits a
/// float element and returns its type — for the float `Index`/`Field` read path.
pub(crate) fn resolve_read_place_typed(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
) -> Result<(ScalarPlace, NativeType), String> {
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = expr;
    let root = loop {
        match &cursor.kind {
            BytecodeExprKind::Variable(name) => break name.as_str(),
            BytecodeExprKind::Field { target, field } => {
                steps.push(PathStep::Field(field.as_str()));
                cursor = target;
            }
            BytecodeExprKind::Index { target, index } => {
                steps.push(PathStep::Index(index));
                cursor = target;
            }
            _ => return Err("native access must be rooted at a local variable".to_string()),
        }
    };
    steps.reverse();
    resolve_place_steps_typed(ctx, root, &steps)
}

/// Resolve an assignment target `(name, path)` to a scalar place.
pub(crate) fn resolve_scalar_place(
    ctx: &NativeCtx,
    name: &str,
    path: &[BytecodePlace],
) -> Result<ScalarPlace, String> {
    let steps: Vec<PathStep> = path
        .iter()
        .map(|place| match place {
            BytecodePlace::Field(field) => PathStep::Field(field.as_str()),
            BytecodePlace::Index(index) => PathStep::Index(index),
        })
        .collect();
    resolve_place_steps(ctx, name, &steps)
}

/// Like [`resolve_scalar_place`] but permits a float element and returns its type
/// — for the float array-element store path (`a[i] = <f64>`).
pub(crate) fn resolve_scalar_place_typed(
    ctx: &NativeCtx,
    name: &str,
    path: &[BytecodePlace],
) -> Result<(ScalarPlace, NativeType), String> {
    let steps: Vec<PathStep> = path
        .iter()
        .map(|place| match place {
            BytecodePlace::Field(field) => PathStep::Field(field.as_str()),
            BytecodePlace::Index(index) => PathStep::Index(index),
        })
        .collect();
    resolve_place_steps_typed(ctx, name, &steps)
}

/// Decompose a nested `Field`/`Index` read expression into a root variable and
/// an ordered list of steps, then resolve it to a scalar place. Returns `None`
/// (as an `Err`) if the expression is not an aggregate-rooted lvalue.
pub(crate) fn resolve_read_place(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
) -> Result<ScalarPlace, String> {
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = expr;
    let root = loop {
        match &cursor.kind {
            BytecodeExprKind::Variable(name) => break name.as_str(),
            BytecodeExprKind::Field { target, field } => {
                steps.push(PathStep::Field(field.as_str()));
                cursor = target;
            }
            BytecodeExprKind::Index { target, index } => {
                steps.push(PathStep::Index(index));
                cursor = target;
            }
            _ => return Err("native access must be rooted at a local variable".to_string()),
        }
    };
    steps.reverse();
    resolve_place_steps(ctx, root, &steps)
}

/// Load the i64 scalar at a resolved place into `rax`.
pub(crate) fn emit_load_place(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match place {
        ScalarPlace::Const { slot } => {
            load_local(code, *slot);
            Ok(())
        }
        ScalarPlace::Dynamic { .. } => {
            emit_dynamic_addr_into_rcx(ctx, place, code)?; // rcx = &word
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            Ok(())
        }
    }
}

/// Compute the effective address of a dynamic scalar word into `rcx`:
/// `rcx = rbp - (base_slot + 8*const_words) - 8*elem_words*index`.
/// Leaves the stack balanced.
pub(crate) fn emit_dynamic_addr_into_rcx(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        index,
    } = place
    else {
        return Err("expected a dynamic place".to_string());
    };
    // rax = index
    lower_native_expr(ctx, index, code)?;
    // Bounds check: trap on out-of-range, mirroring the interpreters' L0413.
    // One UNSIGNED compare catches both `index < 0` (a huge unsigned value) and
    // `index >= len`, so a negative or over-large index faults deterministically
    // (`ud2`) instead of reading adjacent stack memory.
    emit_bounds_check_rax(code, *index_len);
    // rax = index * elem_words   (imul rax, rax, imm32)
    emit_imul_rax_imm(code, *elem_words);
    // rax = rax * 8  -> byte stride  (shl rax, 3)
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]);
    // rcx = rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    // rcx = rcx - rax  (subtract the dynamic byte offset)
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    // rcx = rcx - (base_slot + 8*const_words)  (the static displacement)
    let static_disp = *base_slot + (*const_words as i32) * 8;
    emit_sub_rcx_imm(code, static_disp);
    Ok(())
}

/// `imul rax, rax, imm32`.
pub(crate) fn emit_imul_rax_imm(code: &mut Vec<u8>, imm: i64) {
    code.extend_from_slice(&[0x48, 0x69, 0xC0]);
    code.extend_from_slice(&(imm as i32).to_le_bytes());
}

/// Emit an array-index bounds check on the index already in `rax`: trap with
/// `ud2` unless `0 <= rax < len`. A single UNSIGNED comparison (`cmp`+`jb`) covers
/// both ends — a negative index is a huge unsigned value, so it is `>= len` too.
/// Matches the interpreters' `L0413` (fail, don't read out of bounds); `ud2` is
/// the same deterministic trap the string-slice helper already uses. `len` is a
/// static array length that always fits `imm32`.
pub(crate) fn emit_bounds_check_rax(code: &mut Vec<u8>, len: i64) {
    code.extend_from_slice(&[0x48, 0x3D]); // cmp rax, imm32
    code.extend_from_slice(&(len as i32).to_le_bytes());
    code.extend_from_slice(&[0x72, 0x02]); // jb +2  (in bounds -> skip the trap)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2    (out of bounds -> fault)
}

/// Emit a hoisted bounds guard for an auto-vectorized `for` loop over an array of
/// `len` elements, given the counter's start slot and inclusive-end slot. The
/// vectorized loop bodies address the array inline (bypassing the per-access
/// [`emit_bounds_check_rax`]), so this one-time guard at loop entry keeps them
/// memory-safe: if the loop is NON-EMPTY (`start <= end`) it traps (`ud2`) unless
/// `start >= 0` and `end < len`. The emptiness guard means an empty range (e.g.
/// `for i from 0 to n-1` with `n == 0`, i.e. `end == -1`) never false-traps.
pub(crate) fn emit_loop_bounds_guard(code: &mut Vec<u8>, i_slot: i32, end_slot: i32, len: i64) {
    load_local(code, i_slot); // rax = start
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg skip (start > end -> empty, no access)
    let skip_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Non-empty: start >= 0 (rax still holds start) ...
    code.extend_from_slice(&[0x48, 0x83, 0xF8, 0x00]); // cmp rax, 0
    code.extend_from_slice(&[0x0F, 0x8C]); // jl trap
    let trap_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // ... and end < len.
    load_local(code, end_slot); // rax = end
    code.extend_from_slice(&[0x48, 0x3D]); // cmp rax, imm32 (len)
    code.extend_from_slice(&(len as i32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x83]); // jae trap (end >= len)
    let trap_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xE9); // jmp skip (in bounds)
    let skip_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // trap:
    patch_rel32(code, trap_a);
    patch_rel32(code, trap_b);
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    // skip:
    patch_rel32(code, skip_a);
    patch_rel32(code, skip_b);
}

// -- SSE2 integer-SIMD encoders (auto-vectorization) -------------------------
//
// x86-64 always provides SSE2, so these need no feature check. They operate on
// `xmm0` (the packed accumulator) and `xmm1` (a loaded pair), which are free in
// the i64-scalar functions that carry vectorizable array loops.

/// `movdqu xmm1, [rcx]` — load 16 unaligned bytes (two `i64`s) into `xmm1`.
pub(crate) fn emit_movdqu_xmm1_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x09]);
}

/// `movdqu xmm0, [rcx]` — load 16 unaligned bytes (two `i64`s) into `xmm0`.
pub(crate) fn emit_movdqu_xmm0_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x01]);
}

/// `movdqu [rcx], xmm0` — store the two packed `i64` lanes of `xmm0` to `[rcx]`.
pub(crate) fn emit_movdqu_rcx_from_xmm0(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x7F, 0x01]);
}

/// Horizontally fold the two lanes of `xmm0` into `rax` with `op`: `movq rax,
/// xmm0` (low lane), `psrldq xmm0, 8` (bring the high lane low), `movq rcx,
/// xmm0`, then `rax = rax <op> rcx`. Leaves the packed reduction's scalar total
/// in `rax`.
pub(crate) fn emit_hfold_xmm0_into_rax(op: ReduceOp, code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // movq rax, xmm0 (low lane)
    code.extend_from_slice(&[0x66, 0x0F, 0x73, 0xD8, 0x08]); // psrldq xmm0, 8
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC1]); // movq rcx, xmm0 (high lane)
    op.emit_rax_rcx(code); // rax = rax <op> rcx
}

// -- SSE4.2 min/max vectorization with runtime CPUID dispatch -----------------
//
// 64-bit packed integer min/max needs a 64-bit packed compare (`pcmpgtq`), which
// is SSE4.2 — NOT in the SSE2 baseline every x86-64 CPU guarantees. So a min/max
// reduction emits BOTH a packed SSE4.2 path and a scalar fallback, and a one-time
// `cpuid` at loop entry picks between them at runtime: the produced binary uses
// the vector path on SSE4.2 hardware and the scalar path on an older CPU, staying
// correct everywhere. (`cpuid` runs once per loop entry, never per iteration, so
// its cost is amortized to nothing over the array.)

/// Emit a one-time CPUID SSE4.2 probe and a `jz` taken when SSE4.2 is ABSENT.
/// Returns the rel32 patch site of that jump for the caller to point at its scalar
/// fallback. `cpuid` clobbers eax/ebx/ecx/edx; rbx is callee-saved and may hold a
/// promoted local, so it is preserved around the probe. The SSE4.2 feature bit is
/// CPUID leaf 1, ECX bit 20; the ZF from `test` survives the `pop` (pop leaves
/// flags untouched).
pub(crate) fn emit_cpuid_sse42_probe(code: &mut Vec<u8>) -> usize {
    code.push(0x53); // push rbx (preserve a possibly-promoted local across cpuid)
    emit_mov_rax_imm(code, 1); // eax = 1 (feature leaf)
    code.extend_from_slice(&[0x0F, 0xA2]); // cpuid
    code.extend_from_slice(&[0x89, 0xC8]); // mov eax, ecx (feature bits -> scratch eax)
    code.push(0x5B); // pop rbx (restores the local; leaves ZF untouched)
    // test eax, 1<<20 (SSE4.2) ; jz fallback.
    code.extend_from_slice(&[0xA9]); // test eax, imm32
    code.extend_from_slice(&(1u32 << 20).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x84]); // jz rel32 (patched to the scalar fallback)
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    site
}

/// A vectorizable integer min/max reduction (`acc = max(acc, a[i])` /
/// `min(acc, a[i])`). Both are associative and commutative, so the two-lane packed
/// fold matches the scalar fold exactly.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum MinMaxOp {
    Min,
    Max,
}

impl MinMaxOp {
    /// The reduction identity, broadcast into both lanes of `xmm0` as the packed
    /// seed: `i64::MIN` for max, `i64::MAX` for min (so any real element wins).
    fn emit_packed_seed(self, code: &mut Vec<u8>) {
        let ident = match self {
            MinMaxOp::Max => i64::MIN,
            MinMaxOp::Min => i64::MAX,
        };
        emit_mov_rax_imm(code, ident);
        code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // movq xmm0, rax
        code.extend_from_slice(&[0x66, 0x0F, 0x6C, 0xC0]); // punpcklqdq xmm0,xmm0 (broadcast)
    }

    /// `xmm0 = minmax(xmm0, xmm1)` per lane, via the SSE4.2 `pcmpgtq` mask-blend.
    /// mask = (xmm0 > xmm1). Max keeps xmm0 where mask, xmm1 elsewhere; min is the
    /// mirror. Uses xmm2/xmm3 as scratch (free — these functions have no float
    /// locals).
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xD0]); // movdqa xmm2, xmm0
        code.extend_from_slice(&[0x66, 0x0F, 0x38, 0x37, 0xD1]); // pcmpgtq xmm2, xmm1 (mask = xmm0>xmm1)
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xDA]); // movdqa xmm3, xmm2 (copy mask)
        match self {
            MinMaxOp::Max => {
                code.extend_from_slice(&[0x66, 0x0F, 0xDB, 0xD8]); // pand  xmm3, xmm0 (mask & xmm0)
                code.extend_from_slice(&[0x66, 0x0F, 0xDF, 0xD1]); // pandn xmm2, xmm1 (~mask & xmm1)
            }
            MinMaxOp::Min => {
                code.extend_from_slice(&[0x66, 0x0F, 0xDB, 0xD9]); // pand  xmm3, xmm1 (mask & xmm1)
                code.extend_from_slice(&[0x66, 0x0F, 0xDF, 0xD0]); // pandn xmm2, xmm0 (~mask & xmm0)
            }
        }
        code.extend_from_slice(&[0x66, 0x0F, 0xEB, 0xDA]); // por xmm3, xmm2
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xC3]); // movdqa xmm0, xmm3 (result)
    }

    /// `rax = minmax(rax, rcx)` via `cmp`+`cmov` (branchless, exact for signed i64).
    fn emit_scalar_rax_rcx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
        match self {
            // max: if rax < rcx, take rcx  -> cmovl rax, rcx
            MinMaxOp::Max => code.extend_from_slice(&[0x48, 0x0F, 0x4C, 0xC1]),
            // min: if rax > rcx, take rcx  -> cmovg rax, rcx
            MinMaxOp::Min => code.extend_from_slice(&[0x48, 0x0F, 0x4F, 0xC1]),
        }
    }

    /// `acc = minmax(acc, rax)`, honoring register promotion of `acc`. Preserves
    /// the loaded element (in rax) by moving it to rcx first.
    fn emit_reduce_into_acc(self, ctx: &NativeCtx, acc_slot: i32, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (element)
        match ctx.promoted_reg(acc_slot) {
            Some(reg) => reg.to_rax(code),
            None => load_local(code, acc_slot),
        }
        self.emit_scalar_rax_rcx(code); // rax = minmax(acc, element)
        match ctx.promoted_reg(acc_slot) {
            Some(reg) => reg.from_rax(code),
            None => store_local(code, acc_slot),
        }
    }
}

/// The associative-and-commutative reductions that vectorize into an SSE2 packed
/// loop. Each is exact on `i64` — `+` is associative mod 2^64, and bitwise
/// `& | ^` are associative and commutative bit-for-bit — so the packed result is
/// identical to the scalar fold regardless of pairing order. (Multiplication is
/// also associative mod 2^64, but SSE2 has no 64-bit packed multiply, so it is
/// not offered here.)
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ReduceOp {
    Add,
    And,
    Or,
    Xor,
}

impl ReduceOp {
    /// Seed the packed accumulator (`xmm0`) with this operator's identity: all
    /// ones for `AND` (`pcmpeqd`), zero for `+`/`OR`/`XOR` (`pxor`).
    fn emit_packed_identity(self, code: &mut Vec<u8>) {
        match self {
            ReduceOp::And => code.extend_from_slice(&[0x66, 0x0F, 0x76, 0xC0]), // pcmpeqd xmm0,xmm0
            _ => code.extend_from_slice(&[0x66, 0x0F, 0xEF, 0xC0]),             // pxor xmm0,xmm0
        }
    }

    /// Combine the loaded pair (`xmm1`) into the packed accumulator: `xmm0 <op>= xmm1`.
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x66, 0x0F, 0xD4, 0xC1], // paddq
            ReduceOp::And => &[0x66, 0x0F, 0xDB, 0xC1], // pand
            ReduceOp::Or => &[0x66, 0x0F, 0xEB, 0xC1],  // por
            ReduceOp::Xor => &[0x66, 0x0F, 0xEF, 0xC1], // pxor
        });
    }

    /// `rax = rax <op> rcx`.
    fn emit_rax_rcx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x48, 0x01, 0xC8], // add rax, rcx
            ReduceOp::And => &[0x48, 0x21, 0xC8], // and rax, rcx
            ReduceOp::Or => &[0x48, 0x09, 0xC8],  // or  rax, rcx
            ReduceOp::Xor => &[0x48, 0x31, 0xC8], // xor rax, rcx
        });
    }

    /// `rax = rax <op> rdx`.
    fn emit_rax_rdx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x48, 0x01, 0xD0], // add rax, rdx
            ReduceOp::And => &[0x48, 0x21, 0xD0], // and rax, rdx
            ReduceOp::Or => &[0x48, 0x09, 0xD0],  // or  rax, rdx
            ReduceOp::Xor => &[0x48, 0x31, 0xD0], // xor rax, rdx
        });
    }
}

/// The element-wise map operators that vectorize into an SSE2 packed loop. `+`/`-`
/// are exact mod 2^64; `& | ^` are exact bit-for-bit. All are per-lane, so the
/// packed store is identical to the scalar loop (including under destination
/// aliasing). Multiplication is excluded (no 64-bit packed multiply in SSE2).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum MapOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
}

impl MapOp {
    /// Combine the two loaded pairs: `xmm0 <op>= xmm1` (with `xmm0` = lhs, `xmm1` = rhs).
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            MapOp::Add => &[0x66, 0x0F, 0xD4, 0xC1], // paddq
            MapOp::Sub => &[0x66, 0x0F, 0xFB, 0xC1], // psubq
            MapOp::And => &[0x66, 0x0F, 0xDB, 0xC1], // pand
            MapOp::Or => &[0x66, 0x0F, 0xEB, 0xC1],  // por
            MapOp::Xor => &[0x66, 0x0F, 0xEF, 0xC1], // pxor
        });
    }

    /// Scalar-tail combine `lhs <op> rhs` given `rcx` = lhs, `rax` = rhs, leaving
    /// the result in `rax`. The commutative ops fold in place; `-` (non-commutative)
    /// computes `rcx - rax` then moves it into `rax`.
    fn emit_scalar_tail(self, code: &mut Vec<u8>) {
        match self {
            MapOp::Add => code.extend_from_slice(&[0x48, 0x01, 0xC8]), // add rax, rcx
            MapOp::And => code.extend_from_slice(&[0x48, 0x21, 0xC8]), // and rax, rcx
            MapOp::Or => code.extend_from_slice(&[0x48, 0x09, 0xC8]),  // or  rax, rcx
            MapOp::Xor => code.extend_from_slice(&[0x48, 0x31, 0xC8]), // xor rax, rcx
            MapOp::Sub => {
                code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax (lhs - rhs)
                code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
            }
        }
    }
}

/// The element-wise map operators over `array<f64>`: `+ - *`. Each lane is an
/// independent IEEE-754 double op, so the packed store is bit-for-bit identical to
/// the scalar loop (element-wise maps do NOT reorder, so unlike an f64 *reduction*
/// they stay parity-exact and need no fast-math opt-in).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FloatMapOp {
    Add,
    Sub,
    Mul,
}

impl FloatMapOp {
    /// Packed: `xmm0 <op>= xmm1` (two f64 lanes). addpd/subpd/mulpd.
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            FloatMapOp::Add => &[0x66, 0x0F, 0x58, 0xC1], // addpd
            FloatMapOp::Sub => &[0x66, 0x0F, 0x5C, 0xC1], // subpd
            FloatMapOp::Mul => &[0x66, 0x0F, 0x59, 0xC1], // mulpd
        });
    }
    /// Scalar tail: `xmm0 <op>= xmm1` (single f64). addsd/subsd/mulsd.
    fn emit_scalar(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            FloatMapOp::Add => &[0xF2, 0x0F, 0x58, 0xC1], // addsd
            FloatMapOp::Sub => &[0xF2, 0x0F, 0x5C, 0xC1], // subsd
            FloatMapOp::Mul => &[0xF2, 0x0F, 0x59, 0xC1], // mulsd
        });
    }
}

/// An element-wise map's element type + operator: integer (`paddq`…) or float
/// (`addpd`…). Selected by the operand type at detection; the emitter branches on
/// it for the packed op and the scalar tail.
#[derive(Clone, Copy)]
pub(crate) enum MapKind {
    Int(MapOp),
    Float(FloatMapOp),
}

/// `movsd xmm1, [rcx]` — load a single f64 into xmm1 (scalar-tail rhs).
pub(crate) fn emit_movsd_xmm1_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x09]);
}

/// `sub rcx, imm32` (imm may be any i32; encodes the 32-bit immediate form).
pub(crate) fn emit_sub_rcx_imm(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x81, 0xE9]);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// Lower a `match` over an enum value with scalar payloads.
///
/// Layout mirrors [`NativeType::Enum`]: the scrutinee occupies a tag word
/// followed by its payload words. This function (1) materializes the scrutinee's
/// value into a stack region — either an existing enum local (matched in place)
/// or a scratch region holding a freshly-constructed / copied enum — (2) loads
/// the tag word, and (3) dispatches: each variant arm compares the tag against
/// the variant's discriminant, binds the variant's payload words into arm-scoped
/// locals, lowers the arm body, then jumps to the shared match end. A wildcard
/// arm binds nothing and is unconditional.
///
/// When `is_value` is true, each arm leaves its result value in `rax`; the caller
/// emits the return epilogue after the shared end. When false the match is a
/// statement and any produced value in `rax` is discarded.
///
/// The tag numbering is exactly the interpreter/IR variant order (declared order
/// for a user enum; `some`(0)/`none`(1), `ok`(0)/`err`(1) for the built-ins), so
/// the arm a native `match` selects is identical to the interpreters'.
pub(crate) fn lower_native_match(
    ctx: &mut NativeCtx,
    scrutinee: &BytecodeExpr,
    arms: &[BytecodeMatchArm],
    is_value: bool,
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    // Resolve the scrutinee's enum layout from its static type.
    let layout = resolve_native_type(&scrutinee.ty, ctx.structs, ctx.enums)?;
    let NativeType::Enum {
        variants,
        payload_words,
        ..
    } = layout
    else {
        return Err("match scrutinee is not a native enum".to_string());
    };

    // Materialize the scrutinee into a stack region, yielding its base slot.
    // A plain enum local is matched in place; any other scrutinee is spilled to a
    // scratch region. The scratch cursor is saved and restored so sequential
    // matches reuse the same words.
    let saved_scratch = ctx.scratch_next;
    let base_slot = match &scrutinee.kind {
        BytecodeExprKind::Variable(name) if ctx.locals.contains_key(name) => {
            // Match an existing enum local in place (no copy needed).
            let local = ctx.local(name)?;
            if !matches!(local.ty, NativeType::Enum { .. }) {
                return Err("match scrutinee local is not an enum".to_string());
            }
            local.slot
        }
        BytecodeExprKind::Call { name, args } if variants.iter().any(|v| v.name == *name) => {
            // A freshly-constructed enum: materialize it directly into scratch.
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_enum_construction(ctx, base, &variants, payload_words, name, args, code)?;
            base
        }
        BytecodeExprKind::Call { name, args }
            if name == MAP_GET_BUILTIN
                && args.len() == 2
                && supported_map_kv(&args[0].ty).is_some() =>
        {
            // `match map_get(m, k)`: materialize the builtin's `option<V>` result
            // (tag + payload words) directly into scratch, then dispatch on it. The
            // `map_get` lowering scans the map and writes `some(v)`/`none` into the
            // scratch region, exactly like a constructed enum scrutinee.
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_map_get_into(ctx, base, &scrutinee.ty, &args[0], &args[1], code)?;
            base
        }
        BytecodeExprKind::Call { name, args }
            if matches!(overflow_builtin(name), Some((_, OverflowMode::Checked)))
                && args.len() == 2
                && fixed_int_kind(args[0].ty.name.as_str()).is_some() =>
        {
            // `match checked_<op>(a, b)`: materialize the builtin's `option<T>`
            // result (tag + payload words) directly into scratch, then dispatch on
            // it, exactly like a `map_get` option scrutinee.
            let (ovf_op, _) = overflow_builtin(name).expect("guarded overflow builtin");
            let kind = fixed_int_kind(args[0].ty.name.as_str()).expect("guarded fixed-width kind");
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_native_checked_into(
                ctx,
                base,
                &scrutinee.ty,
                ovf_op,
                kind,
                &args[0],
                &args[1],
                code,
            )?;
            base
        }
        BytecodeExprKind::Call { name, args }
            if name == "parse_i64" && args.len() == 1 && is_string_type(&args[0].ty) =>
        {
            // `match parse_i64(s)`: materialize the builtin's `result<i64, string>`
            // (tag + payload words) directly into scratch, then dispatch on it, just
            // like a `map_get` option scrutinee.
            let words = 1 + payload_words;
            let base = ctx.alloc_scratch(words);
            lower_parse_i64_into(ctx, base, &args[0], code)?;
            base
        }
        BytecodeExprKind::Call { name, .. }
            if ctx
                .signatures
                .get(name.as_str())
                .is_some_and(NativeSignature::returns_aggregate) =>
        {
            // Matching the result of a call that *returns* an enum: materialize the
            // by-pointer aggregate return into scratch, then dispatch on it. The
            // aggregate-return ABI writes the tag + payload words directly into the
            // scratch destination.
            //
            // The scrutinee occupies scratch while the match runs; if the call also
            // needed scratch for by-pointer *aggregate arguments*, the shared region
            // (sized to the max, not the sum, of scrutinee vs. args) could overlap.
            // A call with only scalar arguments needs no arg scratch, so restrict to
            // that case and skip otherwise rather than risk a miscompile.
            let sig = ctx
                .signatures
                .get(name.as_str())
                .expect("guarded aggregate-returning signature");
            if sig.params.iter().any(NativeType::is_aggregate) {
                return Err(
                    "match on an enum-returning call whose arguments are aggregates is \
                     deferred on the native backend"
                        .to_string(),
                );
            }
            let enum_ty = NativeType::Enum {
                name: String::new(),
                variants: variants.clone(),
                payload_words,
            };
            let base = ctx.alloc_scratch(enum_ty.words());
            lower_aggregate_returning_call(ctx, base, &enum_ty, scrutinee, code)?;
            base
        }
        _ => {
            // Any other temporary enum scrutinee (e.g. an enum read out of an
            // aggregate field) is outside the supported set; such a function skips
            // gracefully to the interpreters rather than miscompiling.
            return Err(
                "match scrutinee must be an enum local, a freshly-constructed enum, \
                 or an enum-returning call"
                    .to_string(),
            );
        }
    };

    let mut end_jumps: Vec<usize> = Vec::new();
    let mut saw_wildcard = false;

    for arm in arms {
        match &arm.pattern {
            BytecodeMatchPattern::Wildcard => {
                // Unconditional: bind nothing, lower the body, jump to end.
                saw_wildcard = true;
                lower_match_arm_body(ctx, &arm.body, is_value, code, loops)?;
                code.push(0xE9); // jmp end
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                end_jumps.push(site);
                // A wildcard is terminal (exhaustiveness), so stop emitting arms.
                break;
            }
            BytecodeMatchPattern::Variant { name, bindings } => {
                let variant = variants
                    .iter()
                    .find(|v| &v.name == name)
                    .ok_or_else(|| format!("match arm names unknown variant `{name}`"))?;
                // Reload the tag word each arm — arm bodies clobber rax — then
                // cmp rax, tag ; jne next_arm.
                load_local(code, base_slot);
                emit_cmp_rax_imm(code, variant.tag);
                code.extend_from_slice(&[0x0F, 0x85]); // jne rel32 (patched)
                let jne_site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);

                // Bind the matched variant's payload words into the arm locals.
                // Payload word k lives at base_slot + 8*(1 + prefix_words).
                let mut payload_word = base_slot + 8;
                for (binding, field_ty) in bindings.iter().zip(variant.payload.iter()) {
                    let dst = ctx.local_slot(binding)?;
                    match field_ty {
                        // An integer-cell scalar OR a `string` payload binds as a
                        // single flat word: load the payload word into `rax` (the
                        // value, or the immutable string pointer) and store it into
                        // the arm-scoped local. The bound string shares its pointer.
                        NativeType::I64 | NativeType::String => {
                            load_local(code, payload_word);
                            store_local(code, dst);
                        }
                        NativeType::F64 | NativeType::F32 => {
                            let width = match field_ty {
                                NativeType::F64 => FloatWidth::F64,
                                NativeType::F32 => FloatWidth::F32,
                                _ => unreachable!("guarded above"),
                            };
                            load_float_local(code, payload_word, width);
                            store_float_local(code, dst, width);
                        }
                        // A `HeapStruct` payload is bound as a STACK `Struct` local:
                        // load the payload's heap pointer, then flat-copy each field
                        // word `[ptr + 8*k]` -> `[dst + 8*k]`. The bound value is a
                        // fresh stack snapshot, so mutating it never touches the
                        // source heap struct (value semantics, matching the
                        // interpreters' cloned `match` binding).
                        NativeType::HeapStruct { fields, .. } => {
                            load_local(code, payload_word); // rax = heap-struct ptr
                            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
                            for word in 0..fields.len() as i32 {
                                code.extend_from_slice(&[0x48, 0x8B, 0x81]); // mov rax, [rcx+disp32]
                                code.extend_from_slice(&(word * 8).to_le_bytes());
                                store_local(code, dst + word * 8);
                            }
                        }
                        // A nested `List`/`Map` payload binds its (deep-copied)
                        // pointer word directly.
                        NativeType::List { .. } | NativeType::Map { .. } => {
                            load_local(code, payload_word);
                            emit_heap_slot_deep_copy(ctx, field_ty, code);
                            store_local(code, dst);
                        }
                        _ => {
                            return Err("enum payload binding is not a native scalar, `string`, \
                                 or one-level mutable aggregate"
                                .to_string());
                        }
                    }
                    payload_word += field_ty.words() as i32 * 8;
                }

                lower_match_arm_body(ctx, &arm.body, is_value, code, loops)?;
                code.push(0xE9); // jmp end
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                end_jumps.push(site);

                // The next arm starts here (the jne target).
                patch_rel32(code, jne_site);
            }
        }
    }

    // If no wildcard covered the fallthrough, exhaustiveness guarantees one of the
    // variant arms matched, so this point is unreachable. Emit `ud2` to trap on
    // the impossible case rather than run off into the next function.
    if !saw_wildcard {
        code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    }

    let end = code.len();
    for site in end_jumps {
        patch_rel32_to(code, site, end);
    }

    ctx.scratch_next = saved_scratch;
    Ok(())
}

/// Lower one match arm body. When `is_value` is true the arm's tail expression is
/// the match's result: its value is left in `rax` (an arm ending in an explicit
/// `return` emits its own epilogue instead). When false the body is a statement
/// block whose result is discarded.
pub(crate) fn lower_match_arm_body(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    is_value: bool,
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    if is_value && matches!(body.last(), Some(BytecodeInstruction::Expr(e)) if !e.ty.is_void()) {
        let (head, tail) = body.split_at(body.len() - 1);
        lower_native_stmts(ctx, head, code, loops)?;
        if let BytecodeInstruction::Expr(expr) = &tail[0] {
            lower_native_expr(ctx, expr, code)?;
        }
        Ok(())
    } else {
        lower_native_stmts(ctx, body, code, loops)
    }
}

/// `cmp rax, imm`. Uses the sign-extended imm32 form (`48 3D imm32`) when the
/// value fits in an i32 (every discriminant tag does), else materializes the
/// immediate into `rcx` and compares register-to-register.
pub(crate) fn emit_cmp_rax_imm(code: &mut Vec<u8>, imm: i64) {
    if let Ok(imm32) = i32::try_from(imm) {
        // cmp rax, imm32  (48 3D id) — sign-extended.
        code.push(0x48);
        code.push(0x3D);
        code.extend_from_slice(&imm32.to_le_bytes());
    } else {
        // mov rcx, imm64 ; cmp rax, rcx.
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&imm.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
    }
}

/// If `cond` is a plain `i64`-vs-`i64` comparison, emit it fused with its branch:
/// lower both operands, `cmp rcx, rax`, then a single conditional jump taken when
/// the condition is FALSE (so control falls through into the guarded body only
/// when it holds). Returns the rel32 patch site of that jump for the caller to
/// point at the skip target, or `None` when `cond` is not a fusable i64
/// comparison — in which case the caller lowers the condition to a 0/1 in rax and
/// uses the generic `test rax,rax; jz` path.
///
/// This reuses the exact operand lowering and `cmp rcx, rax` that the boolean
/// comparison in `emit_i64_binop_from_stack` performs; it only replaces the
/// trailing `setcc; movzx rax,al; test rax,rax; jz` (four instructions that
/// materialize a 0/1 and re-test it) with one flag-based conditional jump —
/// exactly what a C compiler emits for `if (a < b)`. Fixed-width ints, floats,
/// strings, and non-comparison conditions fall back to the generic path.
pub(crate) fn try_emit_fused_i64_condition_branch(
    ctx: &mut NativeCtx,
    cond: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<Option<usize>, String> {
    let BytecodeExprKind::Binary { left, op, right } = &cond.kind else {
        return Ok(None);
    };
    // Only plain signed `i64` operands: the jumps below are the signed forms and
    // mirror the signed `setl`/`setle`/… the generic comparison path uses.
    if left.ty.name != "i64" || right.ty.name != "i64" {
        return Ok(None);
    }
    // Second byte of the `0F 8x` conditional jump taken when the comparison is
    // FALSE (the inverse of the operator), so the guarded body runs only when the
    // condition holds.
    let jump_when_false: u8 = match op {
        BinaryOp::Less => 0x8D,         // jge
        BinaryOp::LessEqual => 0x8F,    // jg
        BinaryOp::Greater => 0x8E,      // jle
        BinaryOp::GreaterEqual => 0x8C, // jl
        BinaryOp::Equal => 0x85,        // jne
        BinaryOp::NotEqual => 0x84,     // je
        _ => return Ok(None),
    };
    // Constant right operand (the common `n < 2` / `i < len` idiom): lower the
    // left operand into rax and compare against the immediate directly, skipping
    // the operand-stack shuffle (`emit_cmp_rax_imm` uses the imm32 form, or
    // materializes a full i64 into rcx). `cmp rax, imm` computes left - right,
    // so the same inverted jump applies.
    if let BytecodeExprKind::Integer(rhs) = &right.kind {
        // When the left operand is a promoted-register local and the immediate
        // fits imm32, compare the register directly (`cmp <reg>, imm`) instead of
        // `mov rax, <reg>; cmp rax, imm` — the common `i < len` loop-guard idiom.
        match (promoted_var_reg(ctx, left), i32::try_from(*rhs).ok()) {
            (Some(reg), Some(imm32)) => reg.cmp_imm(code, imm32),
            _ => {
                lower_native_expr(ctx, left, code)?;
                emit_cmp_rax_imm(code, *rhs); // cmp rax(left), right
            }
        }
        code.extend_from_slice(&[0x0F, jump_when_false]); // j<!cc> rel32 (patched by caller)
        let site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        return Ok(Some(site));
    }

    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (left)
    lower_native_expr(ctx, right, code)?; // right → rax
    code.push(0x59); // pop rcx (left)
    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
    code.extend_from_slice(&[0x0F, jump_when_false]); // j<!cc> rel32 (patched by caller)
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    Ok(Some(site))
}

/// Lower an `if`/`elif`/`else` chain. Each branch: test the condition (fused into
/// a `cmp`+conditional jump for an `i64` comparison, else `eval into rax` +
/// `test rax,rax`); `j.. next`; body; `jmp end`. The final else falls through.
pub(crate) fn lower_native_if(
    ctx: &mut NativeCtx,
    branches: &[BytecodeIfBranch],
    else_body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    let mut end_jumps: Vec<usize> = Vec::new();

    for branch in branches {
        // Fused `cmp`+conditional-jump for an i64 comparison; else the generic
        // "evaluate to 0/1 in rax, `test rax,rax`, `jz`" path. Both yield a rel32
        // site that jumps to the next branch when the condition is false.
        let jz_site = match try_emit_fused_i64_condition_branch(ctx, &branch.condition, code)? {
            Some(site) => site,
            None => {
                lower_native_expr(ctx, &branch.condition, code)?;
                code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                code.extend_from_slice(&[0x0F, 0x84]); // jz next_branch (patched below)
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                site
            }
        };

        lower_native_stmts(ctx, &branch.body, code, loops)?;

        // jmp end (rel32, patched at the very end).
        code.push(0xE9);
        let end_site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        end_jumps.push(end_site);

        // Patch the jz to land here (start of the next branch / else).
        patch_rel32(code, jz_site);
    }

    // Else body (may be empty).
    lower_native_stmts(ctx, else_body, code, loops)?;

    // Patch every branch's trailing `jmp end` to land here.
    let end = code.len();
    for site in end_jumps {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}

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
    /// Per-block: `acc += block_a*i + block_b` (`block_a = a*K`, `block_b =
    /// a*K(K-1)/2 + K*b`).
    block_a: i32,
    block_b: i32,
    /// Per-remainder-iteration: `acc += a*i + b`.
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
    // The per-block and per-iteration coefficients must all fit an i32 immediate;
    // otherwise fall through to the multi-accumulator reduction.
    const K: i64 = 4;
    let a = i32::try_from(a64).ok()?;
    let b = i32::try_from(b64).ok()?;
    let block_a = i32::try_from(a64.checked_mul(K)?).ok()?;
    let block_b = i32::try_from(
        a64.checked_mul(K * (K - 1) / 2)?
            .checked_add(b64.checked_mul(K)?)?,
    )
    .ok()?;
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
        block_a,
        block_b,
        a,
        b,
    })
}

/// `imul rax, <counter>, imm32` — `rax = counter * imm32` (low 64 bits).
fn emit_imul_rax_counter_imm(code: &mut Vec<u8>, counter: PReg, imm: i32) {
    // imul r64, r/m64, imm32 = REX.W 69 /r id ; ModRM = 11 rax(000) <counter>.
    code.extend_from_slice(&[0x48, 0x69, 0xC0 | counter.code3()]);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `add rax, imm32` (sign-extended).
fn emit_add_rax_imm(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x05]);
    code.extend_from_slice(&imm.to_le_bytes());
}

pub(crate) fn emit_affine_reduction(
    ctx: &mut NativeCtx,
    plan: &AffineReductionLoop,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    const K: i64 = 4;
    let AffineReductionLoop {
        acc,
        counter,
        block_a,
        block_b,
        a,
        b,
        ..
    } = *plan;
    const RCX: u8 = 1; // bound
    const RDX: u8 = 2; // limit = bound - (K-1)

    // Bounds: rcx = bound, rdx = bound-(K-1). The affine block/remainder use only
    // rax + the promoted counter (imul rax, counter, imm; add rax, imm), so rcx
    // and rdx survive the loop untouched.
    let skip_main_site = match &plan.bound {
        SumBound::Const(bound) => {
            emit_mov_scratch_imm(code, RCX, *bound as i32);
            emit_mov_scratch_imm(code, RDX, (bound - (K - 1)) as i32);
            None
        }
        SumBound::Runtime(bound_expr) => {
            lower_native_expr(ctx, bound_expr, code)?; // rax = n
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            code.extend_from_slice(&[0x48, 0x83, 0xF9, K as u8]); // cmp rcx, K
            code.extend_from_slice(&[0x0F, 0x8C]); // jl skip_main
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            code.extend_from_slice(&[0x48, 0x89, 0xCA]); // mov rdx, rcx
            code.extend_from_slice(&[0x48, 0x83, 0xEA, (K - 1) as u8]); // sub rdx, K-1
            Some(site)
        }
    };

    // Blocked main loop: acc += block_a*i + block_b, i += K.
    emit_align_loop_top(code);
    let main_top = code.len();
    emit_cmp_counter_scratch(code, counter, RDX);
    code.extend_from_slice(&[0x0F, 0x8D]); // jge main_end
    let main_exit = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_imul_rax_counter_imm(code, counter, block_a);
    emit_add_rax_imm(code, block_b);
    acc.add_rax(code);
    counter.add_imm(code, K as i32);
    emit_jmp_to(code, main_top);
    let main_end = code.len();
    patch_rel32_to(code, main_exit, main_end);

    if let Some(site) = skip_main_site {
        let here = code.len();
        patch_rel32_to(code, site, here);
    }

    // Scalar remainder: acc += a*i + b, i += 1.
    let rem_top = code.len();
    emit_cmp_counter_scratch(code, counter, RCX);
    code.extend_from_slice(&[0x0F, 0x8D]); // jge rem_end
    let rem_exit = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_imul_rax_counter_imm(code, counter, a);
    emit_add_rax_imm(code, b);
    acc.add_rax(code);
    counter.add_imm(code, 1);
    emit_jmp_to(code, rem_top);
    let rem_end = code.len();
    patch_rel32_to(code, rem_exit, rem_end);
    Ok(())
}

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
    // General multi-accumulator reduction: `acc = acc + EXPR` where `EXPR` is a
    // pure polynomial in the counter (sum-of-squares, weighted sums, …). Four
    // independent accumulators break the serial `acc` dependency chain that made
    // the naive scalar loop ~2.5× slower than C.
    if let Some(plan) = detect_general_reduction(ctx, condition, body) {
        emit_general_reduction(ctx, &plan, code)?;
        return Ok(());
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
    });
    lower_native_stmts(ctx, body, code, loops)?;
    // Reclaim per-iteration owned string temporaries on the fallthrough back-edge
    // (RC drop insertion); `continue` (jumps to `top`) and `break` skip it and leak
    // on those paths, which is safe.
    emit_loop_body_string_drops(ctx, body, code)?;
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
    let top = code.len();
    loops.push(NativeLoop {
        continue_target: Some(top),
        continue_sites: Vec::new(),
        break_sites: Vec::new(),
    });
    lower_native_stmts(ctx, body, code, loops)?;
    emit_loop_body_string_drops(ctx, body, code)?;
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
    for (idx, stmt) in body.iter().enumerate() {
        let BytecodeInstruction::Let { name, value, .. } = stmt else {
            continue;
        };
        let Ok(local) = ctx.local(name) else {
            continue;
        };
        let slot = local.slot;
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
        // mov rcx, [rbp - slot] ; call <drop_symbol>
        code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
        code.extend_from_slice(&(-slot).to_le_bytes());
        emit_call_symbol(ctx, drop_symbol, code);
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

/// Lower a range `for i = start..=end step s` to an `i64` counter loop mirroring
/// the interpreter's inclusive range: ascending stops when `i > end`, descending
/// when `i < end`. `continue` jumps to the step, `break` exits.
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
    });
    lower_native_stmts(ctx, body, code, loops)?;
    // Reclaim uniquely-owned per-iteration string temporaries on the fallthrough
    // back-edge (RC drop insertion). Placed BEFORE the step label so a `continue`
    // (which jumps to the step label) skips it — leaking on that path, which is
    // safe — while the common no-`continue` body frees every iteration.
    emit_loop_body_string_drops(ctx, body, code)?;
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

/// A recognized `for i from S to E: acc <op>= a[i]` reduction over an `array<i64>`,
/// ready to vectorize. Element `k` of the array sits at
/// `[rbp - array_base_static - 8*k]`, matching the scalar index addressing.
pub(crate) struct Reduction {
    acc_slot: i32,
    array_base_static: i32,
    array_len: i64,
    op: ReduceOp,
}

/// Recognize a `for counter from S to E: acc <op>= array[counter]` reduction,
/// where `acc` is an `i64` local, `array` is an `array<i64>`, and `<op>` is one of
/// the vectorizable reductions: `+` (spelled `acc += array[i]`) or bitwise
/// `& | ^` (spelled `acc = acc <op> array[i]`, either operand order — they are
/// commutative). Returns `None` for anything else so the caller falls back to the
/// scalar loop. Bounds `S`/`E` are lowered by the vectorizer itself.
pub(crate) fn detect_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<Reduction> {
    // Default ascending step of 1 only.
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    // Exactly one body statement, assigning a plain local (no field/index path).
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: assign_op,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() || acc == counter {
        return None;
    }
    // Determine the reduction operator and the `array[counter]` element read.
    // `+` uses the compound `acc += array[i]` (value is the bare element read);
    // bitwise ops use `acc = acc <op> array[i]` (value is a binary with `acc` as
    // one operand and the element read as the other, in either order).
    let (op, element) = match assign_op {
        AssignOp::Add => (ReduceOp::Add, value),
        AssignOp::Replace => {
            let BytecodeExprKind::Binary { left, op, right } = &value.kind else {
                return None;
            };
            let reduce_op = match op {
                BinaryOp::BitAnd => ReduceOp::And,
                BinaryOp::BitOr => ReduceOp::Or,
                BinaryOp::BitXor => ReduceOp::Xor,
                _ => return None,
            };
            // One operand must be exactly the accumulator variable; the other is
            // the element read. Bitwise ops are commutative, so accept both orders.
            let is_acc =
                |e: &BytecodeExpr| matches!(&e.kind, BytecodeExprKind::Variable(v) if v == acc);
            let element = if is_acc(left) {
                right
            } else if is_acc(right) {
                left
            } else {
                return None;
            };
            (reduce_op, element.as_ref())
        }
        _ => return None,
    };
    // The element must be `array[counter]`: resolve it as a dynamic i64 element
    // read (reusing the scalar addressing) whose index is exactly the counter.
    let BytecodeExprKind::Index { index, .. } = &element.kind else {
        return None;
    };
    let BytecodeExprKind::Variable(idx) = &index.kind else {
        return None;
    };
    if idx != counter {
        return None;
    }
    let place = resolve_read_place(ctx, element).ok()?;
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = place
    else {
        return None;
    };
    if elem_words != 1 {
        return None; // only a contiguous i64 array is 16-byte packable
    }
    // The accumulator must be a plain `i64` local, distinct from the array root.
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    Some(Reduction {
        acc_slot: acc_local.slot,
        array_base_static: base_slot + const_words as i32 * 8,
        array_len: index_len,
        op,
    })
}

/// A recognized `for i from S to E: acc = max(acc, a[i])` / `min(acc, a[i])`
/// reduction over a contiguous `array<i64>`. Vectorized via SSE4.2 with a runtime
/// CPUID gate and scalar fallback (see [`lower_native_minmax_reduction`]).
pub(crate) struct MinMaxReduction {
    acc_slot: i32,
    array_base_static: i32,
    array_len: i64,
    op: MinMaxOp,
}

/// Recognize `for counter from S to E: acc = max(acc, array[counter])` (or `min`),
/// where `acc` is an `i64` local and `array` is a contiguous `array<i64>`. `max`/
/// `min` are commutative, so the accumulator may be either argument. Returns `None`
/// for anything else so the caller falls back to the scalar loop.
pub(crate) fn detect_minmax_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<MinMaxReduction> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: AssignOp::Replace,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() || acc == counter {
        return None;
    }
    // value must be `max(_, _)` / `min(_, _)` with exactly two args.
    let BytecodeExprKind::Call { name, args } = &value.kind else {
        return None;
    };
    let op = match name.as_str() {
        "max" => MinMaxOp::Max,
        "min" => MinMaxOp::Min,
        _ => return None,
    };
    let [a0, a1] = args.as_slice() else {
        return None;
    };
    // One argument is exactly the accumulator; the other is `array[counter]`.
    let is_acc = |e: &BytecodeExpr| matches!(&e.kind, BytecodeExprKind::Variable(v) if v == acc);
    let element = if is_acc(a0) {
        a1
    } else if is_acc(a1) {
        a0
    } else {
        return None;
    };
    let BytecodeExprKind::Index { index, .. } = &element.kind else {
        return None;
    };
    let BytecodeExprKind::Variable(idx) = &index.kind else {
        return None;
    };
    if idx != counter {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = resolve_read_place(ctx, element).ok()?
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    Some(MinMaxReduction {
        acc_slot: acc_local.slot,
        array_base_static: base_slot + const_words as i32 * 8,
        array_len: index_len,
        op,
    })
}

/// A recognized `for i from S to E: c[i] = a[i] <op> b[i]` element-wise map over
/// contiguous `array<i64>`s (`op` is `+ - & | ^`). Element `k` of each array sits
/// at `[rbp - base - 8*k]`, matching the scalar index addressing.
pub(crate) struct ElementwiseMap {
    dest_base: i32,
    lhs_base: i32,
    rhs_base: i32,
    /// The smallest of the three arrays' lengths — the loop must stay within all
    /// of dest/lhs/rhs, so the hoisted bounds guard checks against the minimum.
    min_len: i64,
    kind: MapKind,
}

/// True when `expr` is exactly the loop counter `counter`.
pub(crate) fn index_is_counter(expr: &BytecodeExpr, counter: &str) -> bool {
    matches!(&expr.kind, BytecodeExprKind::Variable(v) if v == counter)
}

/// If `expr` is `array[counter]` over a contiguous `i64` array, return the array's
/// static element-0 base (`base_slot + 8*const_words`) and its element count.
pub(crate) fn indexed_i64_base(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
    counter: &str,
) -> Option<(i32, i64)> {
    let BytecodeExprKind::Index { index, .. } = &expr.kind else {
        return None;
    };
    if !index_is_counter(index, counter) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = resolve_read_place(ctx, expr).ok()?
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    Some((base_slot + const_words as i32 * 8, index_len))
}

/// Like [`indexed_i64_base`] but for a contiguous `array<f64>` element read.
pub(crate) fn indexed_f64_base(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
    counter: &str,
) -> Option<(i32, i64)> {
    let BytecodeExprKind::Index { index, .. } = &expr.kind else {
        return None;
    };
    if !index_is_counter(index, counter) {
        return None;
    }
    let (place, elem_ty) = resolve_read_place_typed(ctx, expr).ok()?;
    if !matches!(elem_ty, NativeType::F64) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        ..
    } = place
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    Some((base_slot + const_words as i32 * 8, index_len))
}

/// Recognize `for counter from S to E: dest[counter] = lhs[counter] (+|-)
/// rhs[counter]` over contiguous `array<i64>`s (default step 1). Returns `None`
/// for anything else so the caller falls back to the scalar loop.
pub(crate) fn detect_elementwise_map(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<ElementwiseMap> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: dest,
            path,
            op: AssignOp::Replace,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    // The destination is `dest[counter]`.
    let [BytecodePlace::Index(dest_index)] = path.as_slice() else {
        return None;
    };
    if !index_is_counter(dest_index, counter) {
        return None;
    }
    // The value is `lhs[counter] <op> rhs[counter]` for a vectorizable `op`. Try the
    // i64 forms first (`+ - & | ^`); if the operands are `array<f64>` instead, try
    // the float forms (`+ - *` via addpd/subpd/mulpd — bit-exact, per-lane).
    let BytecodeExprKind::Binary { left, op, right } = &value.kind else {
        return None;
    };
    let (lhs_base, lhs_len, rhs_base, rhs_len, kind, dest_float) = if let Some(map_op) = match op {
        BinaryOp::Add => Some(MapOp::Add),
        BinaryOp::Subtract => Some(MapOp::Sub),
        BinaryOp::BitAnd => Some(MapOp::And),
        BinaryOp::BitOr => Some(MapOp::Or),
        BinaryOp::BitXor => Some(MapOp::Xor),
        _ => None,
    }
    .filter(|_| indexed_i64_base(ctx, left, counter).is_some())
    {
        let (lb, ll) = indexed_i64_base(ctx, left, counter)?;
        let (rb, rl) = indexed_i64_base(ctx, right, counter)?;
        (lb, ll, rb, rl, MapKind::Int(map_op), false)
    } else {
        let fop = match op {
            BinaryOp::Add => FloatMapOp::Add,
            BinaryOp::Subtract => FloatMapOp::Sub,
            BinaryOp::Multiply => FloatMapOp::Mul,
            _ => return None,
        };
        let (lb, ll) = indexed_f64_base(ctx, left, counter)?;
        let (rb, rl) = indexed_f64_base(ctx, right, counter)?;
        (lb, ll, rb, rl, MapKind::Float(fop), true)
    };
    let (dest_place, dest_ty) = resolve_scalar_place_typed(ctx, dest, path).ok()?;
    // The destination element type must match the operands (i64 or f64).
    if dest_float != matches!(dest_ty, NativeType::F64) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len: dest_len,
        ..
    } = dest_place
    else {
        return None;
    };
    if elem_words != 1 {
        return None;
    }
    Some(ElementwiseMap {
        dest_base: base_slot + const_words as i32 * 8,
        lhs_base,
        rhs_base,
        min_len: dest_len.min(lhs_len).min(rhs_len),
        kind,
    })
}

/// Vectorize an element-wise map `dest[i] = lhs[i] (+|-) rhs[i]` into an SSE2
/// packed loop (two `i64` lanes per iteration) with a scalar tail for the odd
/// element. Lane order is preserved because all three arrays share the same
/// reverse `[rbp - base - 8*k]` addressing, so this is bit-for-bit identical to
/// the scalar loop (and correct under `dest` aliasing `lhs`/`rhs`).
pub(crate) fn lower_native_vectorized_map(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    map: &ElementwiseMap,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;

    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard against the smallest of dest/lhs/rhs (all three are
    // indexed inline below without a per-access check).
    emit_loop_bounds_guard(code, i_slot, end_slot, map.min_len);

    // `rcx = &array[i+1]` given `rdx = 8*(i+1)`: rcx = rbp - rdx - base.
    let block_addr = |code: &mut Vec<u8>, base: i32| {
        code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
        code.extend_from_slice(&[0x48, 0x29, 0xD1]); // sub rcx, rdx
        emit_sub_rcx_imm(code, base);
    };

    // --- main SIMD loop: while i + 1 <= end, map the pair (i, i+1) ---
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (block offset)
    block_addr(code, map.lhs_base);
    emit_movdqu_xmm0_from_rcx(code);
    block_addr(code, map.rhs_base);
    emit_movdqu_xmm1_from_rcx(code);
    match map.kind {
        MapKind::Int(op) => op.emit_packed(code),
        MapKind::Float(op) => op.emit_packed(code),
    }
    block_addr(code, map.dest_base);
    emit_movdqu_rcx_from_xmm0(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // --- scalar remainder: while i <= end, dest[i] = lhs[i] <op> rhs[i] ---
    patch_rel32(code, after_main_site);
    let rem_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdx = 8*i (element offset; the scalar addressing uses &array[i]).
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
    match map.kind {
        MapKind::Int(op) => {
            // rax = lhs[i]
            block_addr(code, map.lhs_base);
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            code.push(0x50); // push rax (lhs)
            // rax = rhs[i]
            block_addr(code, map.rhs_base);
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            code.push(0x59); // pop rcx (rcx = lhs)
            op.emit_scalar_tail(code); // rax = lhs <op> rhs
            // dest[i] = rax
            code.push(0x50); // push rax (result)
            block_addr(code, map.dest_base);
            code.push(0x58); // pop rax (result)
            code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
        }
        MapKind::Float(op) => {
            // xmm0 = lhs[i] ; xmm1 = rhs[i] ; xmm0 <op>= xmm1 ; dest[i] = xmm0.
            block_addr(code, map.lhs_base);
            load_float_from_rcx(code, FloatWidth::F64); // movsd xmm0, [rcx]
            block_addr(code, map.rhs_base);
            emit_movsd_xmm1_from_rcx(code); // movsd xmm1, [rcx]
            op.emit_scalar(code); // addsd/subsd/mulsd xmm0, xmm1
            block_addr(code, map.dest_base);
            store_float_from_rcx(code, FloatWidth::F64); // movsd [rcx], xmm0
        }
    }
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done_site);
    Ok(())
}

/// `acc = acc <op> rax`, honoring register promotion of the accumulator.
/// Preserves the operand (the loaded element or packed total) in `rdx` while
/// loading/combining/storing `acc`.
pub(crate) fn emit_reduce_into_acc(
    ctx: &NativeCtx,
    acc_slot: i32,
    op: ReduceOp,
    code: &mut Vec<u8>,
) {
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (save operand)
    match ctx.promoted_reg(acc_slot) {
        Some(reg) => reg.to_rax(code), // rax = acc
        None => load_local(code, acc_slot),
    }
    op.emit_rax_rdx(code); // rax = acc <op> rdx
    match ctx.promoted_reg(acc_slot) {
        Some(reg) => reg.from_rax(code), // acc = rax
        None => store_local(code, acc_slot),
    }
}

/// Emit the vectorized reduction `acc <op>= a[S..=E]`. Combines the array two
/// `i64`s at a time with the packed op (`paddq`/`pand`/`por`/`pxor`), horizontally
/// folds the packed accumulator into `acc`, then a scalar tail loop handles a
/// final odd element. The counter and bound live on the stack for this loop (the
/// counter is dead after it). Every offered op is associative (and, for bitwise,
/// commutative) and exact on `i64`, so the total matches the scalar fold
/// bit-for-bit regardless of the pairing order.
pub(crate) fn lower_native_vectorized_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    reduction: &Reduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    let base = reduction.array_base_static;
    let op = reduction.op;

    // i = start ; end_local = end
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard: the inline-addressed loop below bypasses the per-access
    // check, so trap here if the (non-empty) index range escapes the array.
    emit_loop_bounds_guard(code, i_slot, end_slot, reduction.array_len);
    op.emit_packed_identity(code); // packed accumulator = identity

    // --- main SIMD loop: while i + 1 <= end, combine the pair (a[i], a[i+1]) ---
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1  -> rax = i+1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main (i+1 > end)
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // addr of the 16-byte block = rbp - base - 8*(i+1); rax already holds i+1.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base); // rcx = &a[i+1] (block start; covers a[i+1],a[i])
    emit_movdqu_xmm1_from_rcx(code);
    op.emit_packed(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // after_main: fold the two packed lanes into acc.
    patch_rel32(code, after_main_site);
    emit_hfold_xmm0_into_rax(op, code); // rax = lane0 <op> lane1
    emit_reduce_into_acc(ctx, reduction.acc_slot, op, code);

    // --- scalar remainder: while i <= end, combine a[i] ---
    let rem_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done (i > end)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // load a[i]: addr = rbp - base - 8*i
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base);
    code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
    emit_reduce_into_acc(ctx, reduction.acc_slot, op, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done_site);
    Ok(())
}

/// Emit a min/max reduction `acc = max(acc, a[S..=E])` (or `min`) with runtime
/// CPUID dispatch: an SSE4.2 packed path (`pcmpgtq` mask-blend, two `i64` lanes per
/// iteration + scalar tail) when the CPU has SSE4.2, else a plain scalar fold. Both
/// paths fold the array into `acc`, so the result is identical (min/max is
/// associative and commutative). The `cpuid` probe runs once at loop entry.
pub(crate) fn lower_native_minmax_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    red: &MinMaxReduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    let base = red.array_base_static;
    let op = red.op;

    // i = start ; end_local = end  (set before the probe so the scalar fallback,
    // which the probe jumps to BEFORE any SIMD code runs, starts from `start`).
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard (both the SSE4.2 and scalar paths below index inline).
    emit_loop_bounds_guard(code, i_slot, end_slot, red.array_len);

    // Runtime CPUID gate: jump to the scalar fallback when SSE4.2 is absent.
    let fallback_site = emit_cpuid_sse42_probe(code);

    // --- SSE4.2 packed path ---
    op.emit_packed_seed(code); // xmm0 = identity broadcast to both lanes
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1  (i+1)
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base); // rcx = &a[i+1] (block start)
    emit_movdqu_xmm1_from_rcx(code);
    op.emit_packed(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // Fold the two packed lanes, then into acc.
    patch_rel32(code, after_main_site);
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // movq rax, xmm0 (lane0)
    code.extend_from_slice(&[0x66, 0x0F, 0x73, 0xD8, 0x08]); // psrldq xmm0, 8
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC1]); // movq rcx, xmm0 (lane1)
    op.emit_scalar_rax_rcx(code); // rax = minmax(lane0, lane1)
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);

    // Scalar tail for the odd final element (SSE4.2 path).
    let tail_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg simd_done
    let simd_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_load_array_elem(code, i_slot, base); // rax = a[i]
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, tail_top);
    patch_rel32(code, simd_done_site);
    // Skip the scalar fallback.
    code.push(0xE9);
    let done_jmp_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // --- scalar fallback (no SSE4.2): fold every element into acc ---
    patch_rel32(code, fallback_site);
    let scalar_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let scalar_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_load_array_elem(code, i_slot, base); // rax = a[i]
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, scalar_top);

    patch_rel32(code, scalar_done_site);
    patch_rel32(code, done_jmp_site); // both fallthroughs land here
    Ok(())
}

/// `rax = a[i]` for a contiguous i64 array whose element 0 sits at `rbp - base`:
/// addr = rbp - base - 8*i.
pub(crate) fn emit_load_array_elem(code: &mut Vec<u8>, i_slot: i32, base: i32) {
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
    emit_sub_rcx_imm(code, base);
    code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
}

/// `movsd xmm1, [rbp - slot]` — load an f64 local into xmm1.
pub(crate) fn emit_movsd_xmm1_from_local(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x8D]); // movsd xmm1, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// A recognized f64 reduction: `for i: acc += a[i]` (sum) or `acc += a[i]*b[i]`
/// (dot). Only vectorized under `--fast-math` (the 2-lane packed fold reorders the
/// additions). `rhs_base` is `Some` for a dot product, `None` for a plain sum.
pub(crate) struct F64Reduction {
    acc_slot: i32,
    lhs_base: i32,
    rhs_base: Option<i32>,
    array_len: i64,
}

/// Recognize `for counter from S to E: acc += a[counter]` or
/// `acc += a[counter] * b[counter]` where `acc` is an `f64` local and the arrays are
/// `array<f64>`. Returns `None` for anything else (scalar fallback).
pub(crate) fn detect_f64_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<F64Reduction> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: AssignOp::Add,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() {
        return None;
    }
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::F64) {
        return None;
    }
    match &value.kind {
        // sum: acc += a[i]
        BytecodeExprKind::Index { .. } => {
            let (lhs_base, len) = indexed_f64_base(ctx, value, counter)?;
            Some(F64Reduction {
                acc_slot: acc_local.slot,
                lhs_base,
                rhs_base: None,
                array_len: len,
            })
        }
        // dot: acc += a[i] * b[i]
        BytecodeExprKind::Binary {
            left,
            op: BinaryOp::Multiply,
            right,
        } => {
            let (lhs_base, ll) = indexed_f64_base(ctx, left, counter)?;
            let (rhs_base, rl) = indexed_f64_base(ctx, right, counter)?;
            Some(F64Reduction {
                acc_slot: acc_local.slot,
                lhs_base,
                rhs_base: Some(rhs_base),
                array_len: ll.min(rl),
            })
        }
        _ => None,
    }
}

/// Emit an f64 sum/dot reduction with a 2-lane packed accumulator: `pxor` seeds
/// `xmm0` to `0.0`; the main loop `movdqu`-loads a pair (and `mulpd`s the b-pair
/// for a dot), `addpd`s into `xmm0`; then the two lanes fold (`unpckhpd`+`addsd`,
/// SSE2) into the `acc` local, and a scalar tail (`addsd`/`mulsd`) handles the odd
/// element. `--fast-math` only (the packed pairing reorders the additions).
pub(crate) fn lower_native_f64_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    red: &F64Reduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    emit_loop_bounds_guard(code, i_slot, end_slot, red.array_len);
    code.extend_from_slice(&[0x66, 0x0F, 0xEF, 0xC0]); // pxor xmm0, xmm0 (packed acc = 0)

    // `rcx = &array[i+1]` given `rdx = 8*(i+1)`: rcx = rbp - rdx - base.
    let block_addr = |code: &mut Vec<u8>, base: i32| {
        code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
        code.extend_from_slice(&[0x48, 0x29, 0xD1]); // sub rcx, rdx
        emit_sub_rcx_imm(code, base);
    };

    // --- main SIMD loop: while i + 1 <= end, accumulate the pair ---
    let main_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (block offset)
    block_addr(code, red.lhs_base);
    emit_movdqu_xmm1_from_rcx(code); // xmm1 = a pair
    if let Some(rhs_base) = red.rhs_base {
        block_addr(code, rhs_base);
        code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x11]); // movdqu xmm2, [rcx] (b pair)
        code.extend_from_slice(&[0x66, 0x0F, 0x59, 0xCA]); // mulpd xmm1, xmm2
    }
    code.extend_from_slice(&[0x66, 0x0F, 0x58, 0xC1]); // addpd xmm0, xmm1
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // after_main: fold the two lanes, then add into acc.
    patch_rel32(code, after_main);
    code.extend_from_slice(&[0x66, 0x0F, 0x28, 0xC8]); // movapd xmm1, xmm0
    code.extend_from_slice(&[0x66, 0x0F, 0x15, 0xC9]); // unpckhpd xmm1, xmm1 (high lane -> low)
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1 (lane0 = lane0+lane1)
    emit_movsd_xmm1_from_local(code, red.acc_slot); // xmm1 = acc
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1 (acc + packed sum)
    store_float_local(code, red.acc_slot, FloatWidth::F64);

    // --- scalar tail: while i <= end, acc += a[i] (* b[i]) ---
    let rem_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let done = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
    block_addr(code, red.lhs_base);
    load_float_from_rcx(code, FloatWidth::F64); // xmm0 = a[i]
    if let Some(rhs_base) = red.rhs_base {
        block_addr(code, rhs_base);
        emit_movsd_xmm1_from_rcx(code); // xmm1 = b[i]
        code.extend_from_slice(&[0xF2, 0x0F, 0x59, 0xC1]); // mulsd xmm0, xmm1
    }
    emit_movsd_xmm1_from_local(code, red.acc_slot); // xmm1 = acc
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1
    store_float_local(code, red.acc_slot, FloatWidth::F64);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done);
    Ok(())
}
