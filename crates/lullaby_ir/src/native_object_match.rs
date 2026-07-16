//! Native backend: `match` and `if` statement lowering — enum-tag dispatch,
//! match-arm bodies, and fused `i64` condition branches. Split out of
//! native_object_stmt.rs; shared items via `use super::super::*`.
use super::super::*;

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
                // Payload words ASCEND above the tag, so payload word k lives at the
                // decreasing displacement `base_slot - 8*(1 + prefix_words)`.
                let mut payload_word = base_slot - 8;
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
                                // The stack struct ascends like the heap block, so
                                // `[ptr + 8*k]` -> `[rbp - (dst - 8*k)]`.
                                store_local(code, dst - word * 8);
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
                    payload_word -= field_ty.words() as i32 * 8;
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

/// Route a value-position expression to the function's return convention.
///
/// This is the single place that decides *where* a returned value must land, and
/// it must be used for EVERY value-position tail — the function's own tail
/// expression, an `if`/`elif`/`else` branch tail, and a `match` arm tail alike:
///
///   * an aggregate return (`sret_slot` set) copies the value's words through the
///     hidden result pointer (`lower_aggregate_return`);
///   * an `f64`/`f32` return leaves its value in `xmm0`;
///   * every other native scalar leaves its value in `rax`.
///
/// Using the bare integer `lower_native_expr` for a value-position tail is exactly
/// the miscompile this centralization exists to prevent: for an aggregate return it
/// loads the enum's tag word into `rax` and never writes the hidden pointer, so the
/// caller reads its own uninitialized scratch (a silently wrong tag AND payload, or
/// a wild pointer dereference for a heap payload); for a float return it loads the
/// f64's bits into `rax` and never writes `xmm0`.
pub(crate) fn lower_return_value(
    ctx: &mut NativeCtx,
    expr: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if ctx.sret_slot.is_some() {
        lower_aggregate_return(ctx, expr, code)
    } else if matches!(ctx.return_ty, NativeType::F64 | NativeType::F32) {
        lower_native_float_expr(ctx, expr, code).map(|_| ())
    } else {
        lower_native_expr(ctx, expr, code)
    }
}

/// Whether `body` leaves the enclosing function's value on every path it can take.
///
/// A block yields a value when it ends in a non-void tail expression, in an explicit
/// `return` (which emits its own epilogue), in an `asm` block (trusted to leave its
/// value in `rax`, exactly as the function-level tail-`asm` path treats it), or in a
/// nested `if`/`match` whose every arm/branch itself yields. An `if` chain only
/// qualifies when it is exhaustive (it has a non-empty `else`), because a missing
/// `else` lets control fall out of the chain with the value never routed.
///
/// This is the default-deny gate for the value-position tail lowering below: a tail
/// shape that does not provably yield on every path is refused (`L0339`) rather than
/// lowered into a function that returns whatever happened to be in the caller's
/// buffer.
///
/// **Reachability, honestly (measured, not assumed):** the refusal is nearly
/// unreachable today, and where it does fire it is redundant rather than
/// load-bearing. A `Let`/`Assign`/`While`/`For`/diverging-`Loop` tail in a
/// value-returning function never reaches lowering — semantics rejects it with
/// `L0301` ("no final return value"), as it does a non-exhaustive tail `if` (a
/// missing `else`); all four were probed directly. The one shape that DOES reach
/// this gate is a `throw`/`try` branch tail (it passes `lullaby check`), which the
/// gate refuses — but `lower_native_stmt` would refuse it a few lines later anyway
/// with "throw/try is not in the native subset", so the compile-vs-skip decision is
/// unchanged either way.
///
/// The gate is kept deliberately despite that. The miscompile it guards against —
/// emitting a function that never routes its value — is exactly what shipped while
/// this reasoning was left implicit in a comment, and a future instruction variant
/// or a loosened frontend rule would otherwise reintroduce it silently. It is a
/// safety net, and it is described here as one rather than dressed up as a live
/// guard.
pub(crate) fn block_yields_value(body: &[BytecodeInstruction]) -> bool {
    match body.last() {
        Some(BytecodeInstruction::Expr(e)) => !e.ty.is_void(),
        Some(BytecodeInstruction::Return(_)) => true,
        // An `asm` block is trusted to leave the value in `rax` — the same contract
        // the function-level tail-`asm` path relies on. `asm` is native-only (the
        // interpreters reject it with `L0425`), so refusing this shape would not
        // demote it to an interpreter: it would make the program unbuildable
        // ANYWHERE, breaking the freestanding/kernel tier.
        Some(BytecodeInstruction::Asm { .. }) => true,
        Some(BytecodeInstruction::If {
            branches,
            else_body,
            ..
        }) => {
            !else_body.is_empty()
                && branches.iter().all(|b| block_yields_value(&b.body))
                && block_yields_value(else_body)
        }
        Some(BytecodeInstruction::Match { arms, .. }) => {
            arms.iter().all(|a| block_yields_value(&a.body))
        }
        _ => false,
    }
}

/// Lower a block in VALUE position — the enclosing function's result comes from
/// this block's tail. Mirrors [`block_yields_value`]: a tail expression is routed
/// through [`lower_return_value`]; a tail `asm` block emits its bytes and is trusted
/// to leave the value in `rax`; a nested `if`/`match` recurses in value position so
/// its own branch/arm tails are routed the same way; anything else (notably a block
/// ending in an explicit `return`, which emits its own epilogue) lowers as ordinary
/// statements.
pub(crate) fn lower_value_block(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    match body.last() {
        Some(BytecodeInstruction::Expr(e)) if !e.ty.is_void() => {
            let (head, tail) = body.split_at(body.len() - 1);
            lower_native_stmts(ctx, head, code, loops)?;
            let BytecodeInstruction::Expr(expr) = &tail[0] else {
                unreachable!("tail matched as a non-void Expr above");
            };
            lower_return_value(ctx, expr, code)
        }
        // A tail `asm` block IS the branch's value: emit its bytes and let control
        // converge on the shared end, where the caller's epilogue returns `rax`.
        // This mirrors the function-level tail-`asm` path (which emits the bytes and
        // then the epilogue) — the programmer's contract is that the block leaves the
        // result in `rax`, so there is no expression to route.
        Some(BytecodeInstruction::Asm { .. }) => {
            let (head, tail) = body.split_at(body.len() - 1);
            lower_native_stmts(ctx, head, code, loops)?;
            let BytecodeInstruction::Asm { bytes, .. } = &tail[0] else {
                unreachable!("tail matched as an Asm above");
            };
            code.extend_from_slice(bytes);
            Ok(())
        }
        Some(BytecodeInstruction::If { .. }) => {
            let (head, tail) = body.split_at(body.len() - 1);
            lower_native_stmts(ctx, head, code, loops)?;
            let BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } = &tail[0]
            else {
                unreachable!("tail matched as an If above");
            };
            lower_native_if(ctx, branches, else_body, true, code, loops)
        }
        Some(BytecodeInstruction::Match { .. }) => {
            let (head, tail) = body.split_at(body.len() - 1);
            lower_native_stmts(ctx, head, code, loops)?;
            let BytecodeInstruction::Match {
                scrutinee, arms, ..
            } = &tail[0]
            else {
                unreachable!("tail matched as a Match above");
            };
            lower_native_match(ctx, scrutinee, arms, true, code, loops)
        }
        _ => lower_native_stmts(ctx, body, code, loops),
    }
}

/// Lower one match arm body. When `is_value` is true the arm's tail is the match's
/// result and is routed to the function's return convention (see
/// [`lower_value_block`]); an arm ending in an explicit `return` emits its own
/// epilogue instead. When false the body is a statement block whose result is
/// discarded.
pub(crate) fn lower_match_arm_body(
    ctx: &mut NativeCtx,
    body: &[BytecodeInstruction],
    is_value: bool,
    code: &mut Vec<u8>,
    loops: &mut Vec<NativeLoop>,
) -> Result<(), String> {
    if is_value {
        lower_value_block(ctx, body, code, loops)
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
///
/// When `is_value` is true this chain produces the enclosing function's result:
/// every branch body — and the `else` — is lowered in value position, so a branch's
/// tail expression is routed to the function's return convention (the hidden
/// aggregate pointer, `xmm0`, or `rax`) instead of being evaluated and discarded.
/// All branches converge on the shared end, where the caller emits the epilogue.
pub(crate) fn lower_native_if(
    ctx: &mut NativeCtx,
    branches: &[BytecodeIfBranch],
    else_body: &[BytecodeInstruction],
    is_value: bool,
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

        if is_value {
            lower_value_block(ctx, &branch.body, code, loops)?;
        } else {
            lower_native_stmts(ctx, &branch.body, code, loops)?;
        }

        // jmp end (rel32, patched at the very end).
        code.push(0xE9);
        let end_site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        end_jumps.push(end_site);

        // Patch the jz to land here (start of the next branch / else).
        patch_rel32(code, jz_site);
    }

    // Else body (may be empty). In value position it is the chain's final path and
    // carries the function's value like any branch.
    if is_value {
        lower_value_block(ctx, else_body, code, loops)?;
    } else {
        lower_native_stmts(ctx, else_body, code, loops)?;
    }

    // Patch every branch's trailing `jmp end` to land here.
    let end = code.len();
    for site in end_jumps {
        patch_rel32_to(code, site, end);
    }
    Ok(())
}
