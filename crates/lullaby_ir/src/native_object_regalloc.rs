//! Native backend: scalar-local register promotion. Decides which purely-`i64`
//! scalar locals of a function can live in the callee-saved `rbx`/`rsi` registers
//! for the whole body, and provides the `PReg` register abstraction used across
//! the native lowering submodules. Split out of native_object_stmt.rs; resolves
//! shared items via `use super::super::*`.
use super::super::*;

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
    pub(crate) fn from_arg(self, code: &mut Vec<u8>, arg: usize) {
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
    pub(crate) fn spill_to_slot(self, code: &mut Vec<u8>, slot: i32) {
        // mov [rbp + disp32], <reg>
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x89, 0x9D],
            PReg::Rsi => &[0x48, 0x89, 0xB5],
        });
        code.extend_from_slice(&(-slot).to_le_bytes());
    }
    pub(crate) fn restore_from_slot(self, code: &mut Vec<u8>, slot: i32) {
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
    pub(crate) fn sub_imm(self, code: &mut Vec<u8>, imm: i32) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x81, 0xEB],
            PReg::Rsi => &[0x48, 0x81, 0xEE],
        });
        code.extend_from_slice(&imm.to_le_bytes());
    }
    pub(crate) fn add_rax(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            PReg::Rbx => &[0x48, 0x01, 0xC3],
            PReg::Rsi => &[0x48, 0x01, 0xC6],
        });
    }
    pub(crate) fn sub_rax(self, code: &mut Vec<u8>) {
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
    pub(crate) fn sub_reg(self, code: &mut Vec<u8>, src: PReg) {
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
    pub(crate) fn code3(self) -> u8 {
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
