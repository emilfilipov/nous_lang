//! Native backend: **frame scratch and outgoing-argument sizing**. The three
//! whole-body analyses `NativeCtx::plan` runs to size the parts of a stack frame
//! that are not locals:
//!
//! - [`max_match_scratch_words`] — the shared spill region for a `match` whose
//!   enum scrutinee is not a plain local,
//! - [`max_call_arg_scratch_words`] — the materialization region for a call's
//!   by-pointer aggregate arguments, and
//! - [`max_outgoing_stack_words`] — the Win64 outgoing area for a call's 5th+
//!   arguments, above the 32-byte shadow space.
//!
//! Each walks the whole body (recursing through nested control flow) and returns
//! a worst-case word count, because a single region sized to the widest user is
//! reused by every user in turn. Split out of `native_object.rs`, which keeps
//! `NativeCtx` itself; sees the parent's items via `use super::*`.

use super::*;

/// The maximum scratch words a `match` in this body needs for a temporary
/// (non-plain-local) enum scrutinee. A match whose scrutinee is a plain local
/// dispatches in place and needs no scratch; any other scrutinee (a call
/// result, a freshly-constructed enum, an aggregate access) is spilled to a
/// scratch region sized to its enum layout. Recurses through nested bodies so
/// the single shared scratch region is sized to the widest such scrutinee.
pub(crate) fn max_match_scratch_words(
    body: &[BytecodeInstruction],
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<usize, String> {
    let mut max = 0usize;
    for instruction in body {
        let nested = match instruction {
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                let mut here = 0usize;
                if !matches!(scrutinee.kind, BytecodeExprKind::Variable(_)) {
                    let layout = resolve_native_type(&scrutinee.ty, structs, enums)?;
                    here = layout.words();
                }
                for arm in arms {
                    here = here.max(max_match_scratch_words(&arm.body, structs, enums)?);
                }
                here
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                let mut here = max_match_scratch_words(else_body, structs, enums)?;
                for branch in branches {
                    here = here.max(max_match_scratch_words(&branch.body, structs, enums)?);
                }
                here
            }
            BytecodeInstruction::While { body, .. }
            | BytecodeInstruction::Loop { body, .. }
            | BytecodeInstruction::For { body, .. } => {
                max_match_scratch_words(body, structs, enums)?
            }
            _ => 0,
        };
        max = max.max(nested);
    }
    Ok(max)
}

/// The maximum scratch words any single call in this body needs for its
/// by-pointer aggregate arguments. Each aggregate argument of a call is
/// materialized into scratch before its address is passed, so a call's scratch
/// need is the sum of its aggregate arguments' words; the shared scratch region
/// is sized to the widest single call across the function (and nested calls in
/// argument position, handled recursively). Non-aggregate arguments need none.
pub(crate) fn max_call_arg_scratch_words(
    body: &[BytecodeInstruction],
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    signatures: &HashMap<String, NativeSignature>,
    array_lengths: &ArrayLengths,
) -> Result<usize, String> {
    fn expr_scratch(expr: &BytecodeExpr, signatures: &HashMap<String, NativeSignature>) -> usize {
        let mut here = 0usize;
        if let BytecodeExprKind::Call { name, args } = &expr.kind {
            if let Some(sig) = signatures.get(name) {
                let mut sum = 0usize;
                for param_ty in &sig.params {
                    // Aggregates (struct/array/enum) and fat-pointer array
                    // descriptors are materialized into scratch before their
                    // address is passed by pointer.
                    if param_ty.is_aggregate() || matches!(param_ty, NativeType::FatArray { .. }) {
                        sum += param_ty.words();
                    }
                }
                here = sum;
            }
            // A nested call in argument position materializes independently.
            for arg in args {
                here = here.max(expr_scratch(arg, signatures));
            }
        } else {
            for child in expr_children(expr) {
                here = here.max(expr_scratch(child, signatures));
            }
        }
        here
    }
    // `structs`/`enums`/`array_lengths` are accepted for symmetry with the other
    // scratch sizers and to keep the call site uniform; layout comes from the
    // precomputed signatures.
    let _ = (structs, enums, array_lengths);
    let mut max = 0usize;
    for instruction in body {
        let here = match instruction {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Return(Some(value))
            | BytecodeInstruction::Expr(value) => expr_scratch(value, signatures),
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                let mut h = max_call_arg_scratch_words(
                    else_body,
                    structs,
                    enums,
                    signatures,
                    array_lengths,
                )?;
                for branch in branches {
                    h = h.max(expr_scratch(&branch.condition, signatures)).max(
                        max_call_arg_scratch_words(
                            &branch.body,
                            structs,
                            enums,
                            signatures,
                            array_lengths,
                        )?,
                    );
                }
                h
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => expr_scratch(condition, signatures).max(max_call_arg_scratch_words(
                body,
                structs,
                enums,
                signatures,
                array_lengths,
            )?),
            BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::For { body, .. } => {
                max_call_arg_scratch_words(body, structs, enums, signatures, array_lengths)?
            }
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                let mut h = expr_scratch(scrutinee, signatures);
                for arm in arms {
                    h = h.max(max_call_arg_scratch_words(
                        &arm.body,
                        structs,
                        enums,
                        signatures,
                        array_lengths,
                    )?);
                }
                h
            }
            _ => 0,
        };
        max = max.max(here);
    }
    Ok(max)
}

/// The maximum number of **outgoing stack-argument words** any single call in
/// this body needs. The first four Win64 register slots (`rcx`/`rdx`/`r8`/`r9`,
/// including a hidden aggregate-return pointer when the callee returns an
/// aggregate) are passed in registers; arguments 5, 6, … spill onto the stack
/// above the 32-byte shadow space. The caller must reserve `8 * this` bytes of
/// outgoing space (plus 32 bytes shadow) in its frame so those stack words have a
/// home at each `call`. An extern (C-ABI) call spills its 5th+ arguments into the
/// same outgoing area (see `emit_extern_call`), so it is counted here too, using
/// its raw argument count (an extern has no internal signature).
pub(crate) fn max_outgoing_stack_words(
    body: &[BytecodeInstruction],
    signatures: &HashMap<String, NativeSignature>,
) -> usize {
    fn call_stack_words(
        name: &str,
        args: usize,
        signatures: &HashMap<String, NativeSignature>,
    ) -> usize {
        // A compiled callee that returns an aggregate consumes one register slot
        // for its hidden result pointer, shifting the visible args down by one.
        let hidden = signatures
            .get(name)
            .map(|sig| usize::from(sig.returns_aggregate()))
            .unwrap_or(0);
        (args + hidden).saturating_sub(4)
    }
    fn expr_words(expr: &BytecodeExpr, signatures: &HashMap<String, NativeSignature>) -> usize {
        let mut here = 0usize;
        if let BytecodeExprKind::Call { name, args } = &expr.kind {
            // A compiled (internal) callee uses the stack-spill convention with a
            // possible hidden aggregate-return pointer. An extern (C-ABI) call also
            // spills its 5th+ arguments into the same outgoing area, so it must be
            // counted too; it has no native signature, so use its raw argument
            // count. (Native builtins never exceed four arguments, so this
            // over-reserves nothing in practice.)
            here = if signatures.contains_key(name.as_str()) {
                call_stack_words(name, args.len(), signatures)
            } else {
                args.len().saturating_sub(4)
            };
            for arg in args {
                here = here.max(expr_words(arg, signatures));
            }
        } else {
            for child in expr_children(expr) {
                here = here.max(expr_words(child, signatures));
            }
        }
        here
    }
    let mut max = 0usize;
    for instruction in body {
        let here = match instruction {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Return(Some(value))
            | BytecodeInstruction::Expr(value) => expr_words(value, signatures),
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                let mut h = max_outgoing_stack_words(else_body, signatures);
                for branch in branches {
                    h = h
                        .max(expr_words(&branch.condition, signatures))
                        .max(max_outgoing_stack_words(&branch.body, signatures));
                }
                h
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => expr_words(condition, signatures).max(max_outgoing_stack_words(body, signatures)),
            BytecodeInstruction::For {
                start,
                end,
                step,
                body,
                ..
            } => expr_words(start, signatures)
                .max(expr_words(end, signatures))
                .max(
                    step.as_ref()
                        .map(|s| expr_words(s, signatures))
                        .unwrap_or(0),
                )
                .max(max_outgoing_stack_words(body, signatures)),
            BytecodeInstruction::Loop { body, .. } => max_outgoing_stack_words(body, signatures),
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                let mut h = expr_words(scrutinee, signatures);
                for arm in arms {
                    h = h.max(max_outgoing_stack_words(&arm.body, signatures));
                }
                h
            }
            _ => 0,
        };
        max = max.max(here);
    }
    max
}
