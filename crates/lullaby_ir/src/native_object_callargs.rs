//! Native backend: the **internal call-argument ABI** — staging a call's arguments
//! and distributing them to the Win64 argument registers and the outgoing stack area.
//!
//! Split out of `native_object_expr.rs` (which reached the ~1500-line cap); sees the
//! parent's items via `use super::*`. This is the single place that decides WHERE a
//! call's arguments go, so every call shape in the backend — an ordinary internal
//! call, an aggregate-returning call's hidden `sret` pointer, and a closure call's
//! hidden env pointer — agrees on the rule by construction rather than by three
//! parallel implementations that must be kept in sync.
//!
//! `emit_extern_call` (the C-ABI boundary) stays in `native_object_expr.rs`: it
//! marshals across a DIFFERENT convention and shares none of this staging.

use super::*;

// -- Internal call argument ABI (registers + stack spill) --------------------
//
// A compiled Lullaby callee receives its first four **effective** arguments in
// the Win64 registers (`rcx`/`rdx`/`r8`/`r9` for integer/pointer/aggregate-copy
// pointers; `xmm0..3` positionally for floats) and its 5th+ arguments on the
// stack, pushed above the callee's 32-byte shadow space. When the callee returns
// an aggregate, its hidden result pointer consumes register 0, shifting the
// visible arguments down by one effective position. `emit_native_call_args`
// stages every visible argument onto the machine stack, then distributes each to
// its register or outgoing stack slot before the `call`.

/// Load the staged word at machine-stack offset `disp` into effective integer
/// register `pos` (`rcx`/`rdx`/`r8`/`r9`).
const GPR_ARG_INDEX: [u8; 4] = [0, 1, 2, 3];

/// A **hidden first argument** — a pointer the ABI passes in effective register
/// position 0 (`rcx`), shifting every visible argument to position `i + 1`.
///
/// The two call shapes needing one are structurally identical to the ABI, so they
/// share one staging/distribution path; only how `rcx` is materialized differs. The
/// position-shifting consequence — including which XMM a float argument lands in —
/// is shared.
#[derive(Clone, Copy)]
pub(crate) enum HiddenArg {
    /// An aggregate-returning call's caller-allocated destination slot, passed **by
    /// address** (`lea rcx, [rbp - slot]`).
    Sret(i32),
    /// A closure call's env pointer (the `[code_ptr][captures…]` block base), already
    /// a pointer **value** in the closure local's frame slot (`mov rcx, [rbp - slot]`).
    ClosureEnv(i32),
}

/// Stage a call's arguments and place them into the Win64 argument registers and
/// (for a 5th+ argument) the outgoing stack area, then leave the machine stack as
/// the emitter found it so the `call` sees the reserved outgoing area intact.
///
/// `sret` is the caller-allocated destination slot when the callee returns an
/// aggregate (its address is passed as the hidden first argument, register 0),
/// otherwise `None`. See [`emit_native_call_args_with`] for the mechanics.
pub(crate) fn emit_native_call_args(
    ctx: &mut NativeCtx,
    callee: &str,
    args: &[BytecodeExpr],
    sret: Option<i32>,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // The callee's parameter layouts (when it is a compiled function) tell us
    // which arguments are aggregates or floats. An `extern`/builtin call has no
    // aggregate/float parameters on this path (guarded elsewhere), so treat a
    // missing signature as all-scalar-integer.
    let param_tys: Vec<Option<NativeType>> = match ctx.signatures.get(callee) {
        Some(sig) => sig.params.iter().map(|t| Some(t.clone())).collect(),
        None => args.iter().map(|_| None).collect(),
    };
    emit_native_call_args_with(ctx, &param_tys, args, sret.map(HiddenArg::Sret), code)
}

/// The call-argument staging and distribution core, over an explicit parameter
/// layout and an optional [`HiddenArg`].
///
/// A scalar argument stages its value word; a float argument stages its raw float
/// word; an aggregate argument stages a *pointer* to a fresh caller-owned copy in
/// scratch (value semantics). After staging all `n` words on the stack (argument `i`
/// at `[rsp + 8*(n-1-i)]`), each argument's **effective position** — `i` plus one when
/// a hidden argument is present — decides where it goes: positions 0..3 load into the
/// GPR **or XMM of that same index** (Win64 pairs `rcx`/`xmm0`, `rdx`/`xmm1`,
/// `r8`/`xmm2`, `r9`/`xmm3` and consumes both slots together, so a float's register is
/// fixed by position, never by how many floats precede it), and each later position is
/// copied into the outgoing area at `[rsp + 8*n + 32 + 8*(pos-4)]` (becoming
/// `[rsp' + 32 + 8*(pos-4)]` once the staging words are discarded). The hidden argument
/// is materialized into `rcx` last, after every staged word is read, so nothing can
/// clobber it.
pub(crate) fn emit_native_call_args_with(
    ctx: &mut NativeCtx,
    param_tys: &[Option<NativeType>],
    args: &[BytecodeExpr],
    hidden: Option<HiddenArg>,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if param_tys.len() != args.len() {
        return Err(format!(
            "call passes {} argument(s) but the parameter layout has {}",
            args.len(),
            param_tys.len()
        ));
    }
    // Fast path: a single scalar integer/pointer argument with no hidden
    // argument. Staging exists only to keep an already-placed register from being
    // clobbered while a *later* argument is evaluated — with one argument there is
    // nothing to clobber, so evaluate it straight into the first argument register
    // (`rcx`) instead of the stack round-trip. A hidden argument disqualifies it:
    // `rcx` belongs to the hidden pointer, and the visible argument belongs in
    // `rdx`.
    let single_agg_or_float = matches!(
        param_tys.first(),
        Some(Some(t)) if t.is_aggregate()
            || matches!(t, NativeType::F64 | NativeType::F32 | NativeType::FatArray { .. })
    );
    if hidden.is_none() && args.len() == 1 && !single_agg_or_float {
        // `f(reg ± const)` (the recursive `fib(n - 1)` / `fib(n - 2)` idiom):
        // compute the argument with a single `lea rcx, [reg ± imm]`, exactly as C
        // does, instead of `mov rax, reg; add/sub rax, imm; mov rcx, rax`.
        if let Some((reg, disp)) = promoted_reg_plus_const(ctx, &args[0]) {
            emit_lea_rcx_reg_disp(code, reg, disp);
            return Ok(());
        }
        lower_native_expr(ctx, &args[0], code)?; // arg → rax
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
        return Ok(());
    }
    // Stage every argument onto the machine stack as one 8-byte word, left to
    // right, so evaluating a later argument cannot clobber an already-placed
    // register. Reset the scratch cursor so each call reuses the shared region.
    let saved_scratch = ctx.scratch_next;
    for (arg, param_ty) in args.iter().zip(param_tys.iter()) {
        match param_ty {
            Some(ty) if ty.is_aggregate() => {
                // Materialize the argument aggregate into scratch, then push its
                // address (the callee copies-in from this snapshot).
                let base = ctx.alloc_scratch(ty.words());
                lower_aggregate_init(ctx, base, ty, arg, code)?;
                emit_lea_rax_slot(code, base); // rax = &scratch copy
                code.push(0x50); // push rax
            }
            Some(NativeType::FatArray { .. }) => {
                // A fat-pointer array argument: build a two-word `(data_ptr, length)`
                // descriptor in scratch and push its address (the callee copies the
                // two descriptor words in, then reads the array through the shared
                // data pointer). The data pointer is the caller's array storage, so
                // no array body is copied — value-semantically safe because the
                // callee parameter is read-only.
                let base = ctx.alloc_scratch(2);
                emit_fat_array_descriptor(ctx, base, arg, code)?;
                emit_lea_rax_slot(code, base); // rax = &descriptor
                code.push(0x50); // push rax
            }
            Some(NativeType::F64) | Some(NativeType::F32) => {
                // A float argument evaluates into `xmm0`; spill it as one raw word.
                lower_native_float_expr(ctx, arg, code)?;
                code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8
                code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x04, 0x24]); // movsd [rsp], xmm0
            }
            _ => {
                // A scalar integer/pointer argument: evaluate into rax and push it.
                lower_native_expr(ctx, arg, code)?;
                code.push(0x50); // push rax
            }
        }
    }
    ctx.scratch_next = saved_scratch;

    let n = args.len();
    let hidden_words = usize::from(hidden.is_some());
    // Distribute each staged word to its effective position. Register positions
    // (< 4) load into the GPR/XMM chosen by position and class; stack positions
    // (>= 4) copy into the outgoing area above the shadow space.
    for (i, param_ty) in param_tys.iter().enumerate() {
        let staged_disp = 8 * (n - 1 - i) as i32; // arg i at [rsp + staged_disp]
        let pos = i + hidden_words;
        let is_float = matches!(param_ty, Some(NativeType::F64) | Some(NativeType::F32));
        if pos < 4 {
            if is_float {
                emit_load_xmm_from_rsp_disp(code, pos as u8, staged_disp);
            } else {
                emit_load_gpr_from_rsp_disp(code, GPR_ARG_INDEX[pos], staged_disp);
            }
        } else {
            // Copy the staged word into the outgoing stack slot. After the staging
            // words are discarded (`add rsp, 8*n`), the slot at
            // `[rsp + 8*n + 32 + 8*(pos-4)]` becomes `[rsp' + 32 + 8*(pos-4)]`,
            // exactly where the callee reads its `(pos-4)`-th stack parameter from
            // `[rbp + 16 + 8*(pos-4)]`.
            let out_disp = 8 * n as i32 + 32 + 8 * (pos as i32 - 4);
            // mov rax, [rsp + staged_disp] ; mov [rsp + out_disp], rax.
            code.extend_from_slice(&[0x48, 0x8B, 0x84, 0x24]); // mov rax, [rsp + disp32]
            code.extend_from_slice(&staged_disp.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]); // mov [rsp + disp32], rax
            code.extend_from_slice(&out_disp.to_le_bytes());
        }
    }
    // The hidden pointer occupies register 0 (`rcx`). Materialized last, once every
    // staged word has been read out, so no argument evaluation can clobber it.
    match hidden {
        // An aggregate return's destination is passed BY ADDRESS.
        Some(HiddenArg::Sret(dest_slot)) => emit_lea_rcx_slot(code, dest_slot),
        // A closure's env pointer is already a pointer VALUE in its frame slot.
        Some(HiddenArg::ClosureEnv(env_slot)) => emit_mov_rcx_from_slot(code, env_slot),
        None => {}
    }
    // Discard the staging words; the outgoing area and shadow space remain.
    if n > 0 {
        emit_add_rsp(code, 8 * n as i32);
    }
    Ok(())
}

/// Build a fat-pointer array **descriptor** `[data_ptr, length]` in the two scratch
/// words at `base_slot` (word 0) and `base_slot - 8` (word 1 — 8 bytes higher in
/// the ASCENDING layout), for a fat-array call argument. In this increment the
/// argument must be a bare variable bound to a **stack array local** (the common
/// `let arr array<i64> = [..]; f(arr)` shape); anything else demotes the caller
/// gracefully. The data pointer is the address of the array's element 0 (its
/// LOWEST stack word), so the callee reads the caller's storage in place with no
/// array-body copy, striding forward exactly as C would.
pub(crate) fn emit_fat_array_descriptor(
    ctx: &mut NativeCtx,
    base_slot: i32,
    arg: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let BytecodeExprKind::Variable(name) = &arg.kind else {
        return Err("a fat-pointer array argument must be an array variable".to_string());
    };
    let local = ctx.local(name)?.clone();
    let NativeType::Array { len, .. } = &local.ty else {
        return Err("a fat-pointer array argument must reference a stack array local".to_string());
    };
    let len = *len as i64;
    // Descriptor word 0: data pointer = address of the array's element 0 (its
    // LOWEST stack word, `[rbp - arr_slot]`, since words ascend from word 0).
    emit_lea_rax_slot(code, local.slot); // rax = rbp - arr_slot
    store_local(code, base_slot); // descriptor word 0 = data_ptr
    // Descriptor word 1: runtime length (a compile-time constant for a stack array).
    emit_mov_rax_imm(code, len);
    store_local(code, base_slot - 8); // descriptor word 1 = length
    Ok(())
}

/// The marshalling class of one C-ABI scalar crossing the FFI boundary: an
/// integer/pointer value (Win64 GPR `rcx`/`rdx`/`r8`/`r9`, an optional
/// re-normalization on a narrow return) or a float value (`f64`/`f32` in the SSE
/// registers `xmm0..3`, returned in `xmm0`). Positional routing (§4.1) chooses
/// the register for argument N by N's *position and type*: a float at position N
/// consumes `xmm N`, an integer at position N consumes integer register N, and
/// each position consumes exactly one slot in exactly one sequence.
#[derive(Clone, Copy)]
pub(crate) enum FfiScalarClass {
    /// An integer/pointer scalar; `Some(kind)` needs a narrow-return normalization
    /// in `rax`, `None` already fills the 64-bit cell (`i64`/`u64`/`isize`/`usize`).
    Int(Option<IntKind>),
    /// An `f64`/`f32` float scalar routed through the SSE registers.
    Float(FloatWidth),
}
