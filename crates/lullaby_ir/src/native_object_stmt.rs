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
        is_arena,
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
            NativeType::Struct { .. }
            | NativeType::Array { .. }
            | NativeType::Enum { .. }
            | NativeType::FatArray { .. } => {
                // The argument holds a pointer to the caller's copy (in a register
                // for `reg < 4`, on the stack otherwise). Copy the aggregate words
                // into the parameter's frame slots (value semantics: the callee owns
                // an independent snapshot and never mutates the caller's copy). rax =
                // source pointer (addresses word 0, the aggregate's highest stack
                // address). Words descend in memory, so word k is at `[rax - 8*k]`,
                // matching the caller's `[rbp - (base + 8*k)]` layout. A fat-pointer
                // array descriptor copies exactly its two words (data pointer at
                // word 0, runtime length at word 1); the pointer is the caller's
                // storage, shared read-only.
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
        emit_arena_reset(&mut ctx, &mut code);
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
        emit_arena_reset(&mut ctx, &mut code);
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
        emit_arena_reset(&mut ctx, &mut code);
        emit_native_epilogue(&mut code, ctx.frame_size, &ctx.saved_reg_slots);
    } else if tail_is_value_if {
        // The tail `if` lowers as a statement; each branch leaves the function's
        // value in rax and jumps to the convergence point right before this
        // epilogue, which returns it. Emitting the epilogue here makes the
        // fallthrough `xor rax,rax` below unreachable (dead safety code).
        lower_native_stmts(&mut ctx, instructions, &mut code, &mut loops)?;
        emit_arena_reset(&mut ctx, &mut code);
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

/// Arena-first memory (stage 1) prologue: save the current bump pointer
/// (`__lullaby_heap_next`) into the arena mark slot and set the arena-mode flag to
/// `1`. Emitted once, after parameter seating (so the argument registers are free
/// and `rax` may be used as scratch). Only called when `ctx.is_arena`.
fn emit_arena_prologue(ctx: &mut NativeCtx, code: &mut Vec<u8>) {
    let mark = ctx.arena_mark_slot;
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

/// Arena-first memory (stage 1) return reset: restore the bump pointer to the entry
/// mark (reclaiming, in one bulk rewind, every heap block the function allocated)
/// and clear the arena-mode flag. Emitted at EVERY return/exit edge, immediately
/// before the epilogue. Uses `r10`/`r10d` as scratch so the return value in `rax`
/// (or `xmm0`) is preserved. A no-op unless `ctx.is_arena`.
fn emit_arena_reset(ctx: &mut NativeCtx, code: &mut Vec<u8>) {
    if !ctx.is_arena {
        return;
    }
    let mark = ctx.arena_mark_slot;
    // mov r10, [rbp - mark] ; mov [rip + heap_next], r10  (rewind the bump pointer)
    code.extend_from_slice(&[0x4C, 0x8B, 0x95]);
    code.extend_from_slice(&(-mark).to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0x15]);
    let next_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: next_site as u32,
        symbol: HEAP_NEXT_SYMBOL.to_string(),
    });
    // xor r10d, r10d ; mov [rip + alloc_mode], r10  (leave arena mode)
    code.extend_from_slice(&[0x45, 0x31, 0xD2]);
    code.extend_from_slice(&[0x4C, 0x89, 0x15]);
    let mode_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: mode_site as u32,
        symbol: ALLOC_MODE_SYMBOL.to_string(),
    });
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
            if ctx.sret_slot.is_some() {
                lower_aggregate_return(ctx, expr, code)?;
            } else if matches!(ctx.return_ty, NativeType::F64 | NativeType::F32) {
                // A float return leaves its value in `xmm0` (the Win64 SSE return
                // register).
                lower_native_float_expr(ctx, expr, code)?;
            } else {
                lower_native_expr(ctx, expr, code)?;
            }
            emit_arena_reset(ctx, code);
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
