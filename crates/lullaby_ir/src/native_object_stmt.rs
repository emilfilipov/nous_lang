//! Native statement and aggregate lowering core: the per-function lowering driver
//! (prologue/epilogue and arena setup), statement dispatch, and aggregate/enum
//! initialization. Register promotion, place resolution, `match`/`if` lowering,
//! loop lowering with drops, and the SSE reduction/vectorization machinery live in
//! sibling submodules declared below. Split out of native_object.rs; recurses into
//! expression/op lowering via `use super::*`.

use super::*;

#[path = "native_object_regalloc.rs"]
mod regalloc;
pub(crate) use regalloc::*;

#[path = "native_object_place.rs"]
mod place;
pub(crate) use place::*;

#[path = "native_object_match.rs"]
mod match_lowering;
pub(crate) use match_lowering::*;

#[path = "native_object_loops.rs"]
mod loops;
pub(crate) use loops::*;

#[path = "native_object_simd.rs"]
mod simd;
pub(crate) use simd::*;

#[path = "native_object_reduce.rs"]
mod reduce;
pub(crate) use reduce::*;

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
    is_arena: bool,
    closure_layouts: &HashMap<usize, ClosureLayout>,
    hof_index: &HashMap<String, Vec<HofParam>>,
) -> Result<LoweredNativeFunction, String> {
    // Give any binding that shadows an enclosing same-named binding its own slot by
    // alpha-renaming it apart, before the flat-map frame planner keys locals by
    // name. A function without cross-scope shadowing is returned unchanged, so its
    // codegen is byte-identical. See `native_object_rename.rs`.
    let renamed = alpha_rename_shadowing_bindings(function, closure_layouts)?;
    let function = &renamed;
    let mut ctx = NativeCtx::plan(
        function,
        callable,
        extern_sigs,
        structs,
        enums,
        strings,
        signatures,
        array_lengths,
        is_arena,
        closure_layouts,
        hof_index,
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
            // A `void` parameter carries no value, so there is no incoming register
            // or stack word to spill. The parameter resolver never yields `Void`
            // (only the return-only resolver does), so this is unreachable in
            // practice — but it is refused rather than skipped over silently, so
            // that if a frontend change ever admitted a void parameter the function
            // would skip cleanly (`L0339`) instead of consuming a register slot that
            // the caller never filled and shifting every later argument.
            // Unreachable: `Narrow` is an array-element-only layout and the
            // parameter resolver never yields one (a narrow scalar parameter is its
            // normalized `I64` cell). Refused rather than spilled, so a sub-word
            // parameter could never be stored with the wrong width.
            NativeType::Narrow { .. } => {
                return Err(format!(
                    "parameter `{}` resolves to a packed narrow layout, which is an \
                     array-element-only representation and is not in the native subset",
                    param.name
                ));
            }
            NativeType::Void => {
                return Err(format!(
                    "parameter `{}` is `void`; a void parameter is not in the native subset",
                    param.name
                ));
            }
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
            NativeType::Struct { .. }
            | NativeType::Array { .. }
            | NativeType::Enum { .. }
            | NativeType::FatArray { .. } => {
                // The argument holds a pointer to the caller's copy (in a register
                // for `reg < 4`, on the stack otherwise). Copy the aggregate words
                // into the parameter's frame slots (value semantics: the callee owns
                // an independent snapshot and never mutates the caller's copy). rax =
                // source pointer (addresses word 0, the aggregate's LOWEST stack
                // address). Words ASCEND in memory, so word k is at `[rax + 8*k]`,
                // matching the caller's `[rbp - (base - 8*k)]` layout — the same
                // convention C uses. A fat-pointer array descriptor copies exactly
                // its two words (data pointer at word 0, runtime length at word 1);
                // the pointer is the caller's storage, shared read-only.
                if on_stack {
                    emit_mov_rax_from_rbp_pos(&mut code, stack_disp);
                } else {
                    code.extend_from_slice(ARG_TO_RAX[reg]);
                }
                for word in 0..local.ty.words() as i32 {
                    // mov rcx, [rax + 8*word]
                    emit_mov_rcx_from_rax_disp(&mut code, word * 8);
                    // mov [rbp - (slot - 8*word)], rcx
                    emit_mov_slot_from_rcx(&mut code, local.slot - word * 8);
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

    // Arena-first memory (stage 1) prologue: after the parameters are seated (so
    // the argument registers are free), save the current bump pointer
    // (`__lullaby_heap_next`) into the arena mark slot and set the arena-mode flag.
    // The body then bump-allocates all its heap through the arena; every return edge
    // rewinds the bump pointer and clears the flag (see `emit_arena_reset`). This is
    // the only change to the function relative to its RC codegen, and it is
    // value-neutral: it reclaims dead memory at return without altering any result.
    if ctx.is_arena {
        emit_arena_prologue(&mut ctx, &mut code);
    }

    let mut loops: Vec<NativeLoop> = Vec::new();

    // A function whose last statement is a value-producing tail expression (e.g.
    // a body of just `a + b`) returns that value. Lower the leading statements
    // normally, then lower the tail expression and emit the return epilogue so
    // the result in rax is returned rather than being clobbered by the
    // fallthrough safety epilogue below.
    //
    // A VOID function has no value position at all, so its tail is never routed
    // through `lower_return_value`: it falls through to the ordinary statement
    // lowering below, which evaluates the tail for its effects and lets the
    // fallthrough epilogue return without writing a result. (The `!expr.ty.is_void()`
    // check alone is not this guard — a void function may still end in a
    // non-void expression statement, whose value is discarded, not returned.)
    let instructions = &function.instructions;
    let returns_void = function.return_type.is_void();
    let tail_is_value_expr = !returns_void
        && matches!(
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
    // each arm's tail is routed to the function's return convention and converges
    // after the match, where the epilogue returns it. (An arm that itself ends in an
    // explicit `return` emits its own epilogue and never reaches the shared end.)
    // (A VOID function's tail `match` is NOT a value position: it stays false here
    // and lowers as an ordinary statement `match` — `is_value: false` — below.)
    let tail_is_value_match =
        !returns_void && matches!(instructions.last(), Some(BytecodeInstruction::Match { .. }));
    // A function whose last statement is an `if`/`elif`/`else` producing the
    // function's value (e.g. a body ending in `if c\n a\n else\n b`): each branch's
    // tail is routed to the function's return convention (the hidden aggregate
    // pointer, `xmm0`, or `rax`) and converges after the chain, where the epilogue
    // returns it. Without this the tail `if` lowers as a plain statement whose
    // branch tails are evaluated and DISCARDED — for a scalar the fallthrough
    // `xor rax,rax` below overwrites the result (returning 0), and for an aggregate
    // or float return nothing ever writes the hidden result pointer / `xmm0`, so the
    // caller reads its own uninitialized scratch (a silently wrong value, or a wild
    // pointer dereference for a heap payload).
    // (A VOID function's tail `if` is likewise NOT a value position: it stays false
    // here and lowers as an ordinary statement `if` — `is_value: false` — below,
    // which is why a void body ending in a non-exhaustive `if` is fine.)
    let tail_is_value_if =
        !returns_void && matches!(instructions.last(), Some(BytecodeInstruction::If { .. }));
    // Default-deny: a value-position tail `if`/`match` is only lowerable when every
    // branch/arm provably yields the value (an exhaustive chain whose paths all end
    // in a tail expression, a `return`, or a nested yielding `if`/`match`). A tail
    // that can fall through without routing the value is refused here — skipping to
    // the interpreters — rather than emitted as a function that returns whatever was
    // already in the caller's buffer.
    if (tail_is_value_if || tail_is_value_match) && !block_yields_value(instructions) {
        return Err(
            "a value-producing tail `if`/`match` whose branches do not all yield a value \
             is deferred on the native backend"
                .to_string(),
        );
    }
    if tail_is_asm {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::Asm { bytes, .. } = &tail[0] {
            code.extend_from_slice(bytes);
        }
        // A tail `asm` carries no closure survivor expression; a promoting factory
        // never has one (`returns_promotable_closure` excludes a tail `asm`), so a
        // plain rewind (`None`) is correct here.
        emit_arena_reset(&mut ctx, &mut code, None)?;
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_expr {
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        let BytecodeInstruction::Expr(expr) = &tail[0] else {
            unreachable!("tail_is_value_expr matched a non-Expr tail");
        };
        // Route the tail to the function's return convention: an aggregate through the
        // hidden result pointer, a float in `xmm0`, any other scalar in `rax`.
        lower_return_value(&mut ctx, expr, &mut code)?;
        // Pass the tail expression so a promoting closure factory (whose tail is its
        // returned closure literal / literal-bound local) sizes and relocates the
        // survivor; a non-factory arena function ignores it and plain-rewinds.
        emit_arena_reset(&mut ctx, &mut code, Some(expr))?;
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
        // A value tail `match` routes its per-arm value through one post-convergence
        // reset with no single survivor expression; a promoting factory never has this
        // tail (`returns_promotable_closure` excludes it), so `None` (plain rewind).
        emit_arena_reset(&mut ctx, &mut code, None)?;
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_if {
        // The tail `if` lowers in value position: each branch routes the function's
        // value to the return convention and jumps to the convergence point right
        // before this epilogue, which returns it. Emitting the epilogue here makes
        // the fallthrough `xor rax,rax` below unreachable (dead safety code).
        let (head, tail) = instructions.split_at(instructions.len() - 1);
        lower_native_stmts(&mut ctx, head, &mut code, &mut loops)?;
        if let BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } = &tail[0]
        {
            lower_native_if(&mut ctx, branches, else_body, true, &mut code, &mut loops)?;
        }
        // A value tail `if` likewise routes each branch's value through one
        // post-convergence reset; excluded from promoting factories, so `None`.
        emit_arena_reset(&mut ctx, &mut code, None)?;
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else {
        lower_native_stmts(&mut ctx, instructions, &mut code, &mut loops)?;
    }

    // Fallthrough epilogue. For a VALUE-returning function this is a safety net:
    // such a function is expected to return on every path, but a `xor eax,eax` +
    // epilogue means a missing tail return cannot run off the end of the section.
    //
    // For a VOID function this is the NORMAL return path — a void body simply runs
    // its statements and falls out the bottom. The `xor rax,rax` is not a result
    // (`rax` is undefined on return from a void function and the caller must not
    // read it); it is retained unconditionally because zeroing a caller-saved
    // register is harmless under Win64 and keeping one shared exit avoids
    // branching the emitter on a distinction that has no correctness content.
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

/// Arena-first memory (stage 1) prologue: save the current bump pointer
/// (`__lullaby_heap_next`) into the arena mark slot and set the arena-mode flag to
/// `1`. Emitted once, after parameter seating (so the argument registers are free
/// and `rax` may be used as scratch). Only called when `ctx.is_arena`.
fn emit_arena_prologue(ctx: &mut NativeCtx, code: &mut Vec<u8>) {
    let mark = ctx.arena_mark_slot;

    // Stage-4b: a PROMOTING factory relocates its survivor DOWN to the saved mark, so
    // the mark must be a real, writable heap address. `__lullaby_heap_next` starts at
    // `0` (unseeded) and is only lazily seeded to `__lullaby_heap_base` by the first
    // allocation, so a factory whose closure is the program's first allocation would
    // save mark `0` and the promoting reset would `mov [0], …` — a null write. Seed
    // `heap_next` here (idempotent: a no-op once seeded), so the mark is always a valid
    // address BELOW the survivor. Only a promoting factory emits this, so every other
    // arena function's prologue stays byte-identical. The plain (non-promoting) rewind
    // tolerates a `0` mark (it just restores the unseeded cursor), so it needs no seed.
    if ctx.promotes_closure_return {
        // mov rax, [rip + heap_next]
        code.extend_from_slice(&[0x48, 0x8B, 0x05]);
        let load_site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        ctx.relocations.push(CodeRelocation {
            offset: load_site as u32,
            symbol: HEAP_NEXT_SYMBOL.to_string(),
        });
        // test rax, rax ; jnz +7 (skip the lea when already seeded)
        code.extend_from_slice(&[0x48, 0x85, 0xC0]);
        code.extend_from_slice(&[0x75, 0x07]);
        // lea rax, [rip + heap_base]  (rax = heap region base)
        code.extend_from_slice(&[0x48, 0x8D, 0x05]);
        let base_site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        ctx.relocations.push(CodeRelocation {
            offset: base_site as u32,
            symbol: HEAP_BASE_SYMBOL.to_string(),
        });
        // mov [rip + heap_next], rax  (store back — idempotent when already non-zero)
        code.extend_from_slice(&[0x48, 0x89, 0x05]);
        let store_site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        ctx.relocations.push(CodeRelocation {
            offset: store_site as u32,
            symbol: HEAP_NEXT_SYMBOL.to_string(),
        });
    }

    // mov rax, [rip + heap_next]
    code.extend_from_slice(&[0x48, 0x8B, 0x05]);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
    // mov [rbp - mark], rax  (save the entry bump pointer)
    code.extend_from_slice(&[0x48, 0x89, 0x85]);
    code.extend_from_slice(&(-mark).to_le_bytes());

    // Cross-call arena nesting (I2): SAVE the prior arena-mode flag before setting it,
    // so the return reset can RESTORE it. When this arena function was itself called by
    // another arena function the prior value is `1` (still in arena mode); at the top
    // level it is `0`. `rax` is free here (parameters are already seated).
    let saved_mode = ctx.arena_saved_mode_slot;
    // mov rax, [rip + alloc_mode]  (prior mode)
    code.extend_from_slice(&[0x48, 0x8B, 0x05]);
    let prior_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: prior_site as u32,
        symbol: ALLOC_MODE_SYMBOL.to_string(),
    });
    // mov [rbp - saved_mode], rax  (save the prior mode for the reset to restore)
    code.extend_from_slice(&[0x48, 0x89, 0x85]);
    code.extend_from_slice(&(-saved_mode).to_le_bytes());

    // mov eax, 1 ; mov [rip + alloc_mode], rax  (enter arena mode)
    code.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0x89, 0x05]);
    let mode_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: mode_site as u32,
        symbol: ALLOC_MODE_SYMBOL.to_string(),
    });
}

/// Arena-first memory return reset: reclaim the function's heap and restore the
/// prior arena-mode flag. Emitted at EVERY return/exit edge, immediately AFTER
/// `lower_return_value` and before the epilogue. A no-op unless `ctx.is_arena`.
///
/// Two variants, selected by whether this function PROMOTES a returned closure
/// (`ctx.promotes_closure_return`, arena stage-4b):
///
/// - **Plain rewind** (every non-factory arena function, `return_expr` irrelevant):
///   `heap_next = markF`, reclaiming in one bulk rewind every heap block the function
///   allocated. Uses `r10` as scratch, so the return value in `rax`/`xmm0` is
///   preserved.
///
/// - **Promoting rewind** (a promoting closure factory): on entry `rax` holds the
///   returned survivor's `[code_ptr][captures…]` block pointer. The block is FLAT
///   scalar-capture (no internal pointers — [`returns_promotable_closure`] guarantees
///   it), so a straight word copy relocates it. Copy the `size/8` survivor words down
///   to `markF` (`markF ≤ rax`, so the ascending copy is memmove-safe), set
///   `heap_next = markF + size` (NOT `markF` — the survivor stays reserved above the
///   cursor, so the caller's next allocation starts past it and the caller's later
///   rewind reclaims it exactly once), and return `rax = markF`. This reclaims the
///   factory's per-call scratch while promoting the survivor into the caller's region
///   (`markF ≥ markC`). `r10`/`rdx` are scratch; `size` is a per-return-site
///   compile-time immediate from the returned closure's layout.
///
/// In BOTH variants the arena-mode-flag restore (I2's co-fix) is byte-identical and
/// stays LAST: restore the saved prior `__lullaby_alloc_mode` (`1` when an arena
/// caller invoked this arena function, `0` at the top level) rather than hard-zeroing,
/// so a nested arena call leaves the caller's arena mode intact.
///
/// `return_expr` is the return edge's value (a closure literal / literal-bound local
/// for a promoting factory), used to size the survivor. Default-deny: a promoting
/// factory reaching a reset site whose survivor cannot be sized returns `Err` (the
/// function demotes cleanly) rather than emitting a plain rewind that would free the
/// live survivor.
fn emit_arena_reset(
    ctx: &mut NativeCtx,
    code: &mut Vec<u8>,
    return_expr: Option<&BytecodeExpr>,
) -> Result<(), String> {
    if !ctx.is_arena {
        return Ok(());
    }
    let mark = ctx.arena_mark_slot;

    // Stage-4b: resolve the per-site survivor size for a promoting factory. Any
    // promoting-factory reset site that cannot resolve a flat closure survivor is a
    // demote (Err), never a plain rewind that would reclaim the live survivor.
    let survivor_size = if ctx.promotes_closure_return {
        let expr = return_expr.ok_or_else(|| {
            "a promoting closure factory reached a return edge with no survivor \
             expression to relocate; refusing to plain-rewind a live block"
                .to_string()
        })?;
        Some(ctx.promoted_survivor_bytes(expr).ok_or_else(|| {
            "a promoting closure factory's return edge does not resolve to a flat \
             scalar-capture closure survivor; refusing to plain-rewind a live block"
                .to_string()
        })?)
    } else {
        None
    };

    match survivor_size {
        None => {
            // Plain rewind: mov r10, [rbp - mark] ; mov [rip + heap_next], r10.
            code.extend_from_slice(&[0x4C, 0x8B, 0x95]);
            code.extend_from_slice(&(-mark).to_le_bytes());
            code.extend_from_slice(&[0x4C, 0x89, 0x15]);
            let next_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            ctx.relocations.push(CodeRelocation {
                offset: next_site as u32,
                symbol: HEAP_NEXT_SYMBOL.to_string(),
            });
        }
        Some(size) => {
            let word_count = size / 8;
            // 1. mov r10, [rbp - mark]  (r10 = markF, the relocation dest).
            code.extend_from_slice(&[0x4C, 0x8B, 0x95]);
            code.extend_from_slice(&(-mark).to_le_bytes());
            // 2. copy each survivor word from src (`rax`, preserved) down to dest
            //    (`r10`): mov rdx, [rax + 8k] ; mov [r10 + 8k], rdx. Ascending k, and
            //    dest ≤ src, so this is a memmove-safe forward copy.
            for k in 0..word_count {
                let disp = (k * 8).to_le_bytes();
                code.extend_from_slice(&[0x48, 0x8B, 0x90]); // mov rdx, [rax + disp32]
                code.extend_from_slice(&disp);
                code.extend_from_slice(&[0x49, 0x89, 0x92]); // mov [r10 + disp32], rdx
                code.extend_from_slice(&disp);
            }
            // 3. mov rax, r10  (return value = relocated survivor at markF).
            code.extend_from_slice(&[0x4C, 0x89, 0xD0]);
            // 4. lea r10, [r10 + size] ; mov [rip + heap_next], r10  (heap_next =
            //    markF + size; the survivor stays reserved above the cursor).
            code.extend_from_slice(&[0x4D, 0x8D, 0x92]);
            code.extend_from_slice(&size.to_le_bytes());
            code.extend_from_slice(&[0x4C, 0x89, 0x15]);
            let next_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            ctx.relocations.push(CodeRelocation {
                offset: next_site as u32,
                symbol: HEAP_NEXT_SYMBOL.to_string(),
            });
        }
    }

    // Cross-call arena nesting (I2): RESTORE the saved prior arena-mode flag rather
    // than hard-zeroing it — UNCHANGED across both reset variants, and still LAST.
    // `r10` is scratch (the return value in `rax`/`xmm0` is preserved).
    let saved_mode = ctx.arena_saved_mode_slot;
    // mov r10, [rbp - saved_mode] ; mov [rip + alloc_mode], r10  (restore prior mode)
    code.extend_from_slice(&[0x4C, 0x8B, 0x95]);
    code.extend_from_slice(&(-saved_mode).to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x15]);
    let mode_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: mode_site as u32,
        symbol: ALLOC_MODE_SYMBOL.to_string(),
    });
    Ok(())
}

/// Arena-first memory (stage 2): decide whether a loop gets a per-iteration
/// **sub-region**. Returns `Some(mark_slot)` when the enclosing function is an
/// arena region (`ctx.is_arena`), the loop touches the heap, AND its body confines
/// that heap to the iteration (nothing escapes — see `loop_body_confines_heap`), so
/// its allocations are dead at each iteration edge and a bump-pointer rewind
/// reclaims them soundly. `depth` is the loop's nesting depth (`loops.len()` before
/// the loop is pushed), selecting its dedicated mark word. `None` = no sub-region
/// (non-arena function, a scalar loop, or a loop whose heap escapes — the latter
/// two never occur inside an arena function, since eligibility forbids an unbounded
/// heap loop, but the check keeps this self-contained and default-deny).
pub(crate) fn arena_loop_reset_mark(
    ctx: &NativeCtx,
    touches_heap: bool,
    body: &[BytecodeInstruction],
    depth: usize,
) -> Option<i32> {
    if ctx.is_arena && touches_heap && loop_body_confines_heap(body, &ctx.heap_aggregates) {
        Some(ctx.arena_loop_mark_slot(depth))
    } else {
        None
    }
}

/// Arena stage-2: save the current bump pointer (`__lullaby_heap_next`) into a
/// loop's sub-region mark slot at loop entry. Uses `rax` as scratch (free before a
/// loop top). Paired with [`emit_arena_loop_rewind`] at each iteration edge.
pub(crate) fn emit_arena_loop_save(ctx: &mut NativeCtx, mark: i32, code: &mut Vec<u8>) {
    // mov rax, [rip + heap_next]
    code.extend_from_slice(&[0x48, 0x8B, 0x05]);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
    // mov [rbp - mark], rax
    code.extend_from_slice(&[0x48, 0x89, 0x85]);
    code.extend_from_slice(&(-mark).to_le_bytes());
}

/// Arena stage-2: rewind the bump pointer to a loop's sub-region mark, reclaiming
/// (in one bulk rewind) every heap block the iteration allocated. Emitted at each
/// iteration edge — the fallthrough back-edge and the `break`/`continue` edges.
/// Uses `r10` as scratch so it never clobbers a value in `rax`/`xmm0`. The rewind
/// is idempotent (it restores the same saved value), so an iteration that reaches
/// it more than once cannot corrupt the heap; confinement guarantees it never
/// rewinds past a value that survives the iteration. Does NOT touch the arena-mode
/// flag — the function stays in arena mode across the whole loop.
pub(crate) fn emit_arena_loop_rewind(ctx: &mut NativeCtx, mark: i32, code: &mut Vec<u8>) {
    // mov r10, [rbp - mark]
    code.extend_from_slice(&[0x4C, 0x8B, 0x95]);
    code.extend_from_slice(&(-mark).to_le_bytes());
    // mov [rip + heap_next], r10
    code.extend_from_slice(&[0x4C, 0x89, 0x15]);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
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
                // A `void` local has no value to store. `resolve_native_type` never
                // yields `Void`, so a local can never carry it and this is
                // unreachable in practice; refusing it keeps the default-deny
                // posture (the function skips cleanly) rather than binding a name to
                // a zero-word slot.
                NativeType::Void => {
                    return Err(format!(
                        "local `{name}` is `void`; a void binding is not in the native subset"
                    ));
                }
                // Unreachable: `Narrow` is an array-element-only layout, and a
                // local's layout comes from `native_type_of_init`/`resolve_native_type`,
                // which map a narrow scalar to its normalized `I64` cell. Refused
                // rather than stored, so a narrow `let` could never write a sub-word
                // value into a slot the rest of the backend reads as a full cell.
                NativeType::Narrow { .. } => {
                    return Err(format!(
                        "local `{name}` resolves to a packed narrow layout, which is an \
                         array-element-only representation and is not in the native subset"
                    ));
                }
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
                // A fat-pointer array is a parameter-only descriptor; it can never
                // be the type of a `let` local (there is no expression that produces
                // one), so this arm is unreachable — reject defensively.
                NativeType::FatArray { .. } => {
                    return Err("a fat-pointer array cannot be bound to a local".to_string());
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
            // A PACKED narrow array element store (`a[i] = <i32>`). Only a plain
            // `Replace` is lowered: a compound `a[i] += …` would need a
            // read-modify-write at the element's width, and is deferred exactly
            // like the float element compound store above (the function skips
            // cleanly rather than storing at the wrong width).
            if let Some(access) = narrow_access(&elem_ty) {
                if !matches!(op, AssignOp::Replace) {
                    return Err(
                        "compound assignment on a packed narrow array element is not supported"
                            .to_string(),
                    );
                }
                lower_native_expr(ctx, value, code)?; // rax = value (a normalized cell)
                return emit_store_place_narrow(ctx, &typed_place, access, code);
            }
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
                    // A fat-pointer array parameter is read-only, so an element
                    // store never resolves to it; reject defensively.
                    ScalarPlace::FatIndex { .. } => {
                        return Err("cannot assign to a fat-pointer array element".to_string());
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
                        ScalarPlace::FatIndex { .. } => {
                            return Err("cannot assign to a fat-pointer array element".to_string());
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
                        ScalarPlace::FatIndex { .. } => {
                            return Err("cannot assign to a fat-pointer array element".to_string());
                        }
                    }
                }
            }
            Ok(())
        }
        BytecodeInstruction::Return(Some(expr)) => {
            // Route the returned value to the function's convention: an aggregate
            // through the hidden result pointer, a float in `xmm0`, any other scalar
            // in `rax`.
            lower_return_value(ctx, expr, code)?;
            // Pass the returned expression so a promoting closure factory sizes and
            // relocates its survivor from THIS return edge (per-site — different edges
            // may return different-arity closures); a non-factory arena function
            // ignores it and plain-rewinds.
            emit_arena_reset(ctx, code, Some(expr))?;
            emit_native_epilogue(code, ctx.frame_size, &ctx.saved_reg_slots);
            Ok(())
        }
        BytecodeInstruction::Return(None) => {
            // A bare `return` is legal in — and only in — a VOID function: there is
            // no value to route, so reset the arena and emit the epilogue directly.
            // `rax` is left undefined, which is exactly the void contract (the
            // caller must not read it).
            //
            // In a VALUE-returning function a bare `return` would mean returning
            // whatever happened to be in `rax`, so it is still refused and the
            // function skips cleanly (`L0339`). Semantics rejects that shape first
            // (`L0301`), making this a default-deny backstop rather than the live
            // gate — kept so a frontend change cannot silently turn it into a
            // miscompile.
            if !matches!(ctx.return_ty, NativeType::Void) {
                return Err(
                    "a bare `return` in a value-returning function is not in the native subset"
                        .to_string(),
                );
            }
            // A bare `return` is only legal in a VOID function, which never promotes a
            // closure return; a plain rewind (`None`) is correct.
            emit_arena_reset(ctx, code, None)?;
            emit_native_epilogue(code, ctx.frame_size, &ctx.saved_reg_slots);
            Ok(())
        }
        BytecodeInstruction::Expr(expr) => {
            // A tail expression is the function result; a non-tail call result is
            // discarded. Either way, evaluate it (leaving the value in rax).
            lower_native_expr(ctx, expr, code)?;
            Ok(())
        }
        BytecodeInstruction::Break(_) => {
            // RC stage 2: drop this loop's LIVE owned heap temporaries on the break
            // edge before jumping out. Exactly-once safety: a dynamic iteration takes
            // exactly one of {fallthrough back-edge, break, continue}; the fallthrough
            // drop is emitted at end-of-body, which this `jmp end` skips, so no owned
            // value is dropped on more than one edge. `live_drops` holds only locals
            // whose `let` textually precedes this `break`, so each dropped slot is a
            // live, uniquely-owned value.
            let (drops, arena_mark) = {
                let l = loops.last().ok_or("`break` outside a loop")?;
                (l.live_drops.clone(), l.arena_reset_mark)
            };
            emit_owned_local_drops(ctx, &drops, code);
            // Arena stage-2: rewind this loop's sub-region on the break edge too, so
            // the breaking iteration's confined scratch is reclaimed (and the heap
            // stays bounded for any code after the loop). Idempotent with the
            // fallthrough rewind — a `break` jumps past the back-edge, so at most one
            // fires per dynamic iteration, and even a double rewind restores the same
            // mark.
            if let Some(mark) = arena_mark {
                emit_arena_loop_rewind(ctx, mark, code);
            }
            let loop_ctx = loops.last_mut().expect("loop present (checked above)");
            // jmp rel32 (target patched at loop end).
            code.push(0xE9);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            loop_ctx.break_sites.push(site);
            Ok(())
        }
        BytecodeInstruction::Continue(_) => {
            // RC stage 2: drop this loop's LIVE owned heap temporaries on the continue
            // edge before jumping to the loop top / step block. The fallthrough drop is
            // emitted at end-of-body, which this `continue` jump skips, so no owned
            // value is dropped twice. This is the reclamation-critical early-exit case:
            // a `continue` recurs every iteration, so leaking here exhausts the heap,
            // whereas a `break` fires at most once.
            let (drops, arena_mark) = {
                let l = loops.last().ok_or("`continue` outside a loop")?;
                (l.live_drops.clone(), l.arena_reset_mark)
            };
            emit_owned_local_drops(ctx, &drops, code);
            // Arena stage-2: rewind this loop's sub-region on the continue edge —
            // THE reclamation-critical case, since a `continue` recurs every
            // iteration. The continue jump skips the fallthrough rewind, so this is
            // the iteration's single rewind on that path.
            if let Some(mark) = arena_mark {
                emit_arena_loop_rewind(ctx, mark, code);
            }
            let loop_ctx = loops.last_mut().expect("loop present (checked above)");
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
        } => lower_native_if(ctx, branches, else_body, false, code, loops),
        BytecodeInstruction::While {
            condition, body, ..
        } => lower_native_while(ctx, condition, body, code, loops),
        BytecodeInstruction::Loop { body, .. } => lower_native_loop(ctx, body, code, loops),
        // The explicit `region` block lowers **value-neutrally**: emit its body
        // statements in sequence in the current frame. The scope-renamer
        // (`alpha_rename_shadowing_bindings`) has already given any block-local
        // shadow its own slot, so no aliasing occurs and no reclamation is emitted
        // (that is the scoped follow-up). `break`/`continue`/`return` inside the body
        // target the enclosing loop/function exactly as if the block were absent.
        BytecodeInstruction::RegionBlock { body, .. } => lower_native_stmts(ctx, body, code, loops),
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
            // rax = [rcx + 8*word] ; [rbp - (base - 8*word)] = rax. The heap block
            // already ascends; the stack destination now ascends too, so the two
            // sides step in the same direction.
            code.extend_from_slice(&[0x48, 0x8B, 0x81]); // mov rax, [rcx + disp32]
            code.extend_from_slice(&(word * 8).to_le_bytes());
            store_local(code, base_slot - word * 8);
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
            // The element's BYTE stride: its packed C width for a narrow element,
            // `8 * words` otherwise (unchanged for every pre-existing array).
            let stride = elem.byte_size() as i32;
            for (index, element) in elements.iter().enumerate() {
                // Element `index` ascends: stride*index bytes above element 0.
                let at = base_slot - index as i32 * stride;
                lower_value_into(ctx, at, elem, element, code)?;
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
            // Fields ascend in declaration order (field k at `offset_of == +8*k`
            // sits 8*k bytes ABOVE field 0), so the displacement DECREASES.
            let mut word = base_slot;
            for (arg, (_, field_ty)) in args.iter().zip(fields.iter()) {
                lower_value_into(ctx, word, field_ty, arg, code)?;
                word -= field_ty.words() as i32 * 8;
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
                load_local(code, src_slot - word * 8);
                store_local(code, base_slot - word * 8);
            }
            Ok(())
        }
        (
            BytecodeExprKind::Field { .. } | BytecodeExprKind::Index { .. },
            NativeType::Array { .. },
        ) => {
            // Whole-field by-value copy of an inline fixed array into a fresh local:
            // `let c = f.field` (and the hidden `let __foreach_coll = f.field` binding
            // the `for x in f.field` desugar emits). Resolve the source's static word-0
            // base slot, then move the array's words one at a time — an INDEPENDENT
            // snapshot, exactly like the whole-local aggregate copy above. Because the
            // copy is element-wise into `c`'s own slots, a later mutation of `c` never
            // touches `f.field` (the soundness core). The resolver refuses a runtime-
            // indexed source and a heap-word element (`array<string, N>`), so those
            // skip cleanly (`L0339`) rather than aliasing.
            let (src_slot, src_ty) = resolve_inline_aggregate_source(ctx, value)?;
            if &src_ty != ty {
                return Err("aggregate copy between differing layouts".to_string());
            }
            for word in 0..ty.words() as i32 {
                load_local(code, src_slot - word * 8);
                store_local(code, base_slot - word * 8);
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
    // Payload words follow at ASCENDING addresses — 8, 16, ... bytes above the tag
    // — i.e. at the decreasing displacements base_slot - 8, -16, ... in field
    // order. A float payload word is materialized through xmm0; a scalar through
    // rax.
    let mut word = base_slot - 8;
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
        word -= field_ty.words() as i32 * 8;
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
        // A PACKED narrow array element: evaluate the value into its normalized
        // cell, then store only its low `bytes` bytes at the element's own address.
        // `word_slot` is a byte displacement here (the caller scales by the element's
        // byte stride), so this writes exactly the packed element and never touches
        // its neighbours.
        NativeType::Narrow { bytes, signed } => {
            lower_native_expr(ctx, value, code)?; // rax = value
            emit_lea_rcx_slot(code, word_slot); // rcx = &elem
            emit_store_through_rcx(
                code,
                PointeeAccess {
                    size: *bytes as i64,
                    signed: *signed,
                },
            );
            Ok(())
        }
        // A float array/aggregate element: evaluate into xmm0 and store the whole
        // 8-byte (f64) or 4-byte (f32) word — mirrors the enum-payload float path.
        NativeType::F64 | NativeType::F32 => {
            let width = lower_native_float_expr(ctx, value, code)?;
            store_float_local(code, word_slot, width);
            Ok(())
        }
        // A `string` struct field: evaluate the string expression (a literal, `+`
        // concat, `to_string`, a field read, …) — `lower_native_expr` leaves the
        // immutable record pointer in `rax` — and store the flat pointer word. Since
        // strings are immutable, sharing the pointer IS the field's value-semantic
        // copy, exactly like a `string` list element or enum payload.
        NativeType::String => {
            lower_native_expr(ctx, value, code)?;
            store_local(code, word_slot);
            Ok(())
        }
        _ => lower_aggregate_init(ctx, word_slot, ty, value, code),
    }
}
