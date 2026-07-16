//! Native backend: the freestanding-tier **raw-pointer** surface — `addr_of`,
//! `ptr_read` / `ptr_write`, `volatile_load` / `volatile_store`, `ptr_offset`,
//! `ptr_cast`, and `int_to_ptr` / `ptr_to_int`. Split out of
//! `native_object_expr.rs`; sees the parent's items via `use super::*`.
//!
//! This is the tier that gets raw pointers *right*: a `ptr<T>` is a real 64-bit
//! machine address in a GPR/frame word, `addr_of(x)` is a real `lea` of `x`'s
//! frame slot, and `ptr_write(addr_of(x), 5)` genuinely makes `x == 5`. The
//! interpreters cannot model that aliasing (their `addr_of` snapshots the place
//! by value) and therefore *refuse* such a store at run time with `L0459` — a
//! loud refusal, never a silent wrong answer. Native is the tier where the store
//! aliases; the interpreter refusal is a separate, parallel concern and is not
//! touched here.
//!
//! # Default-deny scope (why some shapes skip)
//!
//! Everything here is `unsafe`-gated at check time, so there are no bounds checks
//! and no provenance tracking — but the *lowering* still has to be sound, and two
//! properties of the existing native value model bound what can be lowered:
//!
//! 1. **Every Lullaby scalar is a normalized 8-byte cell.** An `i32` local does
//!    not occupy 4 bytes of frame; it occupies a full sign-extended word. A
//!    width-correct 4-byte store through that word's address would leave the
//!    upper half stale and corrupt the cell invariant every other native path
//!    relies on. So `addr_of` is lowered **only for an 8-byte scalar** (`i64` /
//!    `u64` / `isize` / `usize` / `ptr<T>`), where the C width and the cell width
//!    coincide.
//! 2. **Native aggregates are laid out at DESCENDING addresses.** Word `k` of a
//!    struct/array in a frame lives at `[rbp - (slot + 8*k)]` and, through an
//!    aggregate pointer, at `[ptr - 8*k]` (see `emit_mov_rax_disp_from_rcx`'s
//!    negative displacements). So `ptr_offset(addr_of(a[0]), 1)` would step
//!    *backwards* through the array, disagreeing with C, with `size_of`/
//!    `offset_of`, and with the interpreters' ascending snapshot model — on a
//!    program the interpreters define. Rather than emit that, `addr_of` of an
//!    **array element or a whole array** is refused here and the function skips
//!    cleanly (`L0339`) with a precise reason. A struct-**field** path (`s.f`,
//!    `s.a.b`) *is* lowered: its address is genuine and a read/write through it
//!    aliases exactly, and the interpreters snapshot such a field as a
//!    single-cell region (walking off it is undefined on both tiers, exactly as
//!    walking off a C `&local` is).
//!
//! A pointer obtained from `int_to_ptr` (MMIO, a linker-provided address, an FFI
//! pointer) addresses **real, C-laid-out memory**, so `ptr_read`/`ptr_write`/
//! `volatile_*` through it use the pointee's true C width (1/2/4/8) with
//! sign/zero extension back into the normalized cell. That is the kernel-facing
//! path and it is exact.
//!
//! # `volatile_load` / `volatile_store` are genuinely non-eliding
//!
//! Every builtin here is an `IrExprKind::Call`, and every pass in the native
//! pipeline treats a `Call` as opaque:
//!
//! * The native command runs **only** `OptimizationConfig::inlining()`, and the
//!   inliner (a) only inlines callees whose body is a pure *leaf* expression
//!   (`Inliner::is_pure_leaf` rejects any `Call`), and (b) only substitutes
//!   arguments that are a bare variable or literal (`is_simple_arg`), so a
//!   `volatile_load(p)` argument is never duplicated into two evaluations.
//! * The other passes are not in the native pipeline at all, and are safe anyway:
//!   CSE's `pure_expr_signature` returns `None` for a `Call`; LICM excludes
//!   `Call` from hoisting; copy propagation only aliases a `let x = <Variable>`
//!   and treats a `Call` as a barrier (`expr_requires_optimizer_barrier`); DCE
//!   only drops statements after an unconditional terminator.
//! * The native backend's own peepholes (immediate folding, `*`/`/` strength
//!   reduction, the SIMD reduction detectors in `native_object_reduce.rs`, and
//!   register promotion) match `Integer`/`Variable`/`Binary`/`Index` shapes only
//!   — never a `Call`.
//! * This module itself caches nothing: each `volatile_load`/`volatile_store`
//!   call site emits its own `mov` at its own program point, in order.
//!
//! So a volatile access is never folded, hoisted, CSE'd, duplicated, or removed.
//! `native_program_tests.rs` pins both the emitted-byte count and an
//! observable-elision shape; `suite15.rs` pins the end-to-end exit code.
//!
//! # Register promotion (the address-taken hazard)
//!
//! A promoted local lives in `rbx`/`rsi` and has **no address**, so `lea` of its
//! (unused) frame slot would produce an address nothing reads or writes —
//! `ptr_write(addr_of(x), 5)` would silently not update `x`. That is the single
//! biggest miscompile risk in this surface. [`body_takes_address`] scans a
//! function body for any `addr_of` and `plan_register_promotion` refuses to
//! promote anything in such a function, so an address-taken local always lives in
//! its frame slot. See `addr_of_defeats_register_promotion` in
//! `native_program_tests.rs`.

use super::*;

/// The name of the address-of builtin. A function body containing one is never
/// register-promoted (see the module docs).
pub(crate) const ADDR_OF_BUILTIN: &str = "addr_of";

/// How a pointee is loaded/stored through a raw pointer: its width in bytes and
/// whether a narrow load sign-extends (a signed integer) or zero-extends (an
/// unsigned integer, `byte`, or a pointer) back into the normalized 8-byte cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PointeeAccess {
    /// 1, 2, 4, or 8 bytes — the pointee's C-natural `size_of`.
    pub(crate) size: i64,
    /// Whether a narrow load sign-extends (signed) rather than zero-extends.
    pub(crate) signed: bool,
}

/// The pointee element type named by a `ptr<T>` spelling (or the legacy `ptr_T`
/// spelling that `alloc` produces), or `None` for a non-pointer type name.
pub(crate) fn raw_pointee_name(name: &str) -> Option<&str> {
    if let Some(rest) = name.strip_prefix("ptr<") {
        return rest.strip_suffix('>');
    }
    name.strip_prefix("ptr_")
}

/// Classify a Lullaby type as a raw-pointer **pointee** the native backend can
/// load and store through: an integer-cell scalar of a known C width, or a
/// pointer (8 bytes). Returns `None` for every other pointee — a float, `bool`,
/// `char`, `string`, a struct, an array, or a generic parameter — which makes the
/// enclosing function skip cleanly.
///
/// `bool` and `char` are deliberately excluded even though they are one-word
/// cells: a raw load of arbitrary memory could produce a byte outside `0..=1`
/// (bool) or a non-scalar-value code point (char), breaking an invariant the rest
/// of the backend and the interpreters rely on. `f64`/`f32` are excluded because
/// a float result must land in an XMM register, which this integer-`rax` path
/// cannot deliver; both are clean, precise skips rather than approximations.
pub(crate) fn pointee_access(name: &str) -> Option<PointeeAccess> {
    if is_raw_pointer_type_name(name) {
        // A pointer-to-pointer: a 64-bit address word, unsigned (no extension).
        return Some(PointeeAccess {
            size: 8,
            signed: false,
        });
    }
    let (size, signed) = match name {
        "i8" => (1, true),
        "i16" => (2, true),
        "i32" => (4, true),
        "i64" | "isize" => (8, true),
        "u8" | "byte" => (1, false),
        "u16" => (2, false),
        "u32" => (4, false),
        "u64" | "usize" => (8, false),
        _ => return None,
    };
    Some(PointeeAccess { size, signed })
}

/// Whether a Lullaby type is an **8-byte scalar** whose native normalized cell is
/// exactly its C representation — the only types whose frame word may have its
/// address taken (see the module docs, point 1). `f64` is excluded: taking its
/// address is sound in principle, but `ptr_read` cannot return a float into the
/// integer `rax` path, so admitting it would only produce pointers nothing here
/// can dereference.
fn is_addressable_word_type(name: &str) -> bool {
    matches!(name, "i64" | "u64" | "isize" | "usize") || is_raw_pointer_type_name(name)
}

/// The `PointeeAccess` of the pointer-typed expression `expr` (its `ptr<T>` type's
/// `T`), or an error naming the unsupported pointee so the function skips cleanly.
fn access_of_pointer_expr(expr: &BytecodeExpr, builtin: &str) -> Result<PointeeAccess, String> {
    let pointee = raw_pointee_name(&expr.ty.name).ok_or_else(|| {
        format!(
            "`{builtin}` expects a `ptr<T>` operand on the native backend, found `{}`",
            expr.ty.name
        )
    })?;
    pointee_access(pointee).ok_or_else(|| {
        format!(
            "`{builtin}` through a `ptr<{pointee}>` is not lowered natively: the native \
             raw-pointer surface supports integer and pointer pointees (`i8`…`u64`, \
             `isize`/`usize`, `byte`, `ptr<U>`) only"
        )
    })
}

/// Whether `name` is a raw-pointer builtin this module lowers. Used by the call
/// dispatcher so an unhandled raw-pointer name still reaches the ordinary
/// unknown-function error (and skips) rather than being silently treated as a
/// user call.
pub(crate) fn is_raw_pointer_builtin(name: &str) -> bool {
    matches!(
        name,
        "addr_of"
            | "ptr_read"
            | "ptr_write"
            | "volatile_load"
            | "volatile_store"
            | "ptr_offset"
            | "ptr_cast"
            | "int_to_ptr"
            | "ptr_to_int"
    )
}

/// Lower a raw-pointer builtin call, leaving its result (a pointer, an integer
/// cell, or — for the `void` stores — a dead value) in `rax`. Returns `None` when
/// `name` is not a raw-pointer builtin, so the caller falls through to its other
/// dispatch arms.
///
/// `expr_ty` is the *call's* own type (e.g. `ptr<i64>` for `addr_of`), which
/// carries the pointee the type checker inferred.
pub(crate) fn lower_raw_pointer_call(
    ctx: &mut NativeCtx,
    name: &str,
    args: &[BytecodeExpr],
    expr_ty: &TypeRef,
    code: &mut Vec<u8>,
) -> Option<Result<(), String>> {
    if !is_raw_pointer_builtin(name) {
        return None;
    }
    Some(lower_checked(ctx, name, args, expr_ty, code))
}

fn lower_checked(
    ctx: &mut NativeCtx,
    name: &str,
    args: &[BytecodeExpr],
    expr_ty: &TypeRef,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match name {
        // `int_to_ptr(n) -> ptr<T>` and `ptr_to_int(p) -> isize` are the identity
        // at machine level: a 64-bit address and a 64-bit integer are the same
        // register word. `ptr_cast(p) -> ptr<U>` reinterprets the pointee type
        // only — no value change, no address change. All three lower to their
        // operand's own codegen with nothing appended.
        "int_to_ptr" | "ptr_to_int" | "ptr_cast" => {
            let [operand] = args else {
                return Err(format!("`{name}` takes exactly one argument"));
            };
            lower_native_expr(ctx, operand, code)
        }
        "addr_of" => {
            let [place] = args else {
                return Err("`addr_of` takes exactly one argument".to_string());
            };
            lower_addr_of(ctx, place, expr_ty, code)
        }
        "ptr_read" | "volatile_load" => {
            let [pointer] = args else {
                return Err(format!("`{name}` takes exactly one argument"));
            };
            let access = access_of_pointer_expr(pointer, name)?;
            lower_native_expr(ctx, pointer, code)?; // address -> rax
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_load_through_rcx(code, access);
            Ok(())
        }
        "ptr_write" | "volatile_store" => {
            let [pointer, value] = args else {
                return Err(format!("`{name}` takes exactly two arguments"));
            };
            let access = access_of_pointer_expr(pointer, name)?;
            // Evaluate the address first and spill it, then the value — the same
            // two-operand idiom the binary-op lowering uses (`push` left,
            // evaluate right, `pop` it back), so a call inside either operand is
            // staged correctly.
            lower_native_expr(ctx, pointer, code)?; // address -> rax
            code.push(0x50); // push rax (address)
            lower_native_expr(ctx, value, code)?; // value -> rax
            code.push(0x59); // pop rcx  (address)
            emit_store_through_rcx(code, access);
            Ok(())
        }
        // `ptr_offset(p, n isize) -> ptr<T>` = `p + n * size_of(T)`, with `n`
        // SIGNED (a negative `n` walks back). The stride is the pointee's
        // C-natural size, so the observable size law
        // `ptr_to_int(ptr_offset(p, 1)) - ptr_to_int(p) == size_of(T)` holds
        // exactly, matching the interpreters.
        "ptr_offset" => {
            let [pointer, count] = args else {
                return Err("`ptr_offset` takes exactly two arguments".to_string());
            };
            let pointee = raw_pointee_name(&pointer.ty.name).ok_or_else(|| {
                format!(
                    "`ptr_offset` expects a `ptr<T>` operand on the native backend, found `{}`",
                    pointer.ty.name
                )
            })?;
            // The stride is `size_of(T)`. Only pointees whose C size this backend
            // knows exactly are lowered; a struct/array/float pointee skips
            // cleanly rather than guessing a stride. (An unsized pointee is
            // already rejected at check time with `L0431`.)
            let stride = pointee_access(pointee)
                .map(|access| access.size)
                .ok_or_else(|| {
                    format!(
                        "`ptr_offset` over a `ptr<{pointee}>` is not lowered natively: the \
                         native raw-pointer surface strides by integer and pointer pointee \
                         sizes (`i8`…`u64`, `isize`/`usize`, `byte`, `ptr<U>`) only"
                    )
                })?;
            lower_native_expr(ctx, pointer, code)?; // base -> rax
            code.push(0x50); // push rax (base)
            lower_native_expr(ctx, count, code)?; // n -> rax (signed)
            code.push(0x59); // pop rcx  (base)
            emit_scaled_add_rcx_rax(code, stride);
            Ok(())
        }
        // `is_raw_pointer_builtin` and this match list the same names, so no other
        // name reaches here.
        other => Err(format!("`{other}` is not a raw-pointer builtin")),
    }
}

/// `addr_of(place) -> ptr<T>`: the REAL address of the addressed frame word,
/// `lea rax, [rbp - slot]`.
///
/// Lowered only for a place that resolves to a **single 8-byte scalar word at a
/// compile-time-constant frame slot**: a scalar local/parameter, or a struct-field
/// path (`s.f`, `s.a.b`). Every other shape skips cleanly — see the module docs
/// for why an array element, a whole-array decay, and a narrow/float scalar are
/// each refused.
fn lower_addr_of(
    ctx: &mut NativeCtx,
    place: &BytecodeExpr,
    expr_ty: &TypeRef,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // The addressed place must itself be an 8-byte scalar. This is what rejects a
    // whole-array decay (`addr_of(a)` where `a array<i64>` types as `ptr<i64>`, so
    // the call's own type would pass), a narrow `i32`/`byte` cell, and a
    // float/string/aggregate place.
    if !is_addressable_word_type(&place.ty.name) {
        return Err(format!(
            "`addr_of` of a `{}` place is not lowered natively: the native backend takes the \
             address of an 8-byte scalar (`i64`/`u64`/`isize`/`usize`/`ptr<T>`) only — a \
             narrower scalar is stored as a normalized 8-byte cell, so a width-correct store \
             through its address would corrupt the cell's upper bits",
            place.ty.name
        ));
    }
    // Defensive agreement between the place's type and the checker-inferred
    // pointee: `addr_of(x)` must type as `ptr<typeof x>`. A mismatch means an
    // assumption here no longer holds, so skip rather than emit an address whose
    // pointee width the reader would get wrong.
    match raw_pointee_name(&expr_ty.name) {
        Some(pointee) if pointee == place.ty.name => {}
        _ => {
            return Err(format!(
                "`addr_of` of a `{}` place typed as `{}` is not lowered natively (the pointee \
                 must be the place's own type)",
                place.ty.name, expr_ty.name
            ));
        }
    }

    // Decompose the place into a root local plus FIELD steps only. An `Index` step
    // is refused: native aggregates descend in address, so a pointer into an array
    // element is not C-walkable (module docs, point 2).
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = place;
    let root = loop {
        match &cursor.kind {
            BytecodeExprKind::Variable(name) => break name.as_str(),
            BytecodeExprKind::Field { target, field } => {
                steps.push(PathStep::Field(field.as_str()));
                cursor = target;
            }
            BytecodeExprKind::Index { .. } => {
                return Err(
                    "`addr_of` of an array element is not lowered natively: the native frame \
                     lays an aggregate's words out at DESCENDING addresses, so a pointer into \
                     an array would walk backwards under `ptr_offset` — disagreeing with C, \
                     with `size_of`/`offset_of`, and with the interpreters"
                        .to_string(),
                );
            }
            _ => {
                return Err(
                    "`addr_of` must address a local variable or a struct field on the native \
                     backend (a temporary has no stable address)"
                        .to_string(),
                );
            }
        }
    };
    steps.reverse();

    // A free variable captured by an enclosing closure resolves through the env
    // block, not a frame slot (see the `Variable` arm of `lower_native_expr`), so
    // `local_slot` would name the wrong storage. Refuse rather than address it.
    if let Some(env) = &ctx.closure_env
        && env.captures.contains_key(root)
    {
        return Err(
            "`addr_of` of a closure-captured variable is not lowered natively (the capture \
             lives in the closure's env block, not an addressable frame slot)"
                .to_string(),
        );
    }

    let (place_slot, ty) = resolve_place_steps_typed(ctx, root, &steps)?;
    if ty != NativeType::I64 {
        return Err(
            "`addr_of` must resolve to a single integer/pointer frame word on the native backend"
                .to_string(),
        );
    }
    let ScalarPlace::Const { slot } = place_slot else {
        // Unreachable via the field-only path above (a dynamic place needs an
        // `Index` step), but kept as a hard gate rather than an `unreachable!`:
        // an address is only emitted for a slot known at compile time.
        return Err(
            "`addr_of` requires a compile-time-constant frame slot on the native backend"
                .to_string(),
        );
    };
    // A promoted local lives in `rbx`/`rsi` and has no address. `body_takes_address`
    // disables promotion for any function containing an `addr_of`, so this must not
    // fire; assert it here so a future change to the promotion gate turns into a
    // clean skip instead of a silent miscompile (a `lea` of an unread frame slot).
    if ctx.promoted_reg(slot).is_some() {
        return Err(
            "`addr_of` of a register-promoted local has no address (the promotion gate must \
             exclude address-taken functions)"
                .to_string(),
        );
    }
    emit_lea_rax_local(code, slot);
    Ok(())
}

/// `lea rax, [rbp - slot]` — the address of a frame word.
pub(crate) fn emit_lea_rax_local(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8D, 0x85]); // lea rax, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// Load the pointee at `[rcx]` into `rax`, extended to the normalized 8-byte cell.
fn emit_load_through_rcx(code: &mut Vec<u8>, access: PointeeAccess) {
    match (access.size, access.signed) {
        (8, _) => code.extend_from_slice(&[0x48, 0x8B, 0x01]), // mov rax, [rcx]
        (4, true) => code.extend_from_slice(&[0x48, 0x63, 0x01]), // movsxd rax, dword [rcx]
        (4, false) => code.extend_from_slice(&[0x8B, 0x01]),   // mov eax, [rcx]  (zero-extends)
        (2, true) => code.extend_from_slice(&[0x48, 0x0F, 0xBF, 0x01]), // movsx rax, word [rcx]
        (2, false) => code.extend_from_slice(&[0x48, 0x0F, 0xB7, 0x01]), // movzx rax, word [rcx]
        (1, true) => code.extend_from_slice(&[0x48, 0x0F, 0xBE, 0x01]), // movsx rax, byte [rcx]
        (1, false) => code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0x01]), // movzx rax, byte [rcx]
        // `pointee_access` only ever yields 1/2/4/8, so no other width exists.
        (size, _) => unreachable!("unsupported pointee width {size}"),
    }
}

/// Store the low `access.size` bytes of `rax` to `[rcx]`.
fn emit_store_through_rcx(code: &mut Vec<u8>, access: PointeeAccess) {
    match access.size {
        8 => code.extend_from_slice(&[0x48, 0x89, 0x01]), // mov [rcx], rax
        4 => code.extend_from_slice(&[0x89, 0x01]),       // mov [rcx], eax
        2 => code.extend_from_slice(&[0x66, 0x89, 0x01]), // mov [rcx], ax
        1 => code.extend_from_slice(&[0x88, 0x01]),       // mov [rcx], al
        size => unreachable!("unsupported pointee width {size}"),
    }
}

/// `rax = rcx + rax * stride` — the scaled pointer advance behind `ptr_offset`.
/// A stride of 1/2/4/8 is one `lea` with an x86 SIB scale; any other stride would
/// need an `imul` first, but `pointee_access` only produces 1/2/4/8 so the `lea`
/// always applies.
fn emit_scaled_add_rcx_rax(code: &mut Vec<u8>, stride: i64) {
    let scale_bits: u8 = match stride {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        other => unreachable!("unsupported pointer stride {other}"),
    };
    // lea rax, [rcx + rax*scale] -> REX.W 8D /r, ModRM = 00 000 100 (dest rax,
    // r/m = SIB). SIB = [scale:2][index:3][base:3] with index = rax (000, which
    // contributes nothing to the byte) and base = rcx (001).
    let sib = (scale_bits << 6) | 0x01;
    code.extend_from_slice(&[0x48, 0x8D, 0x04, sib]);
}

// -- The address-taken gate for register promotion ----------------------------

/// Whether `instrs` contain any `addr_of` call. A function that takes the address
/// of a local must keep **every** local in its frame slot: a promoted local lives
/// in `rbx`/`rsi` and has no address, so `lea` of its (never-read) frame slot
/// would make `ptr_write(addr_of(x), 5)` silently not update `x`.
///
/// The gate is deliberately whole-function and coarse rather than per-local:
/// promotion only fires for purely-scalar functions, so refusing to promote a
/// function that takes any address costs nothing measurable and cannot be defeated
/// by an aliasing pattern this analysis failed to see through. See
/// `addr_of_defeats_register_promotion` in `native_program_tests.rs`.
pub(crate) fn body_takes_address(instrs: &[BytecodeInstruction]) -> bool {
    instrs.iter().any(instr_takes_address)
}

fn instr_takes_address(instr: &BytecodeInstruction) -> bool {
    match instr {
        BytecodeInstruction::Let { value, .. } | BytecodeInstruction::Assign { value, .. } => {
            expr_takes_address(value)
        }
        BytecodeInstruction::Return(Some(expr))
        | BytecodeInstruction::Expr(expr)
        | BytecodeInstruction::Throw { value: expr, .. } => expr_takes_address(expr),
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
                .any(|b| expr_takes_address(&b.condition) || body_takes_address(&b.body))
                || body_takes_address(else_body)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_takes_address(condition) || body_takes_address(body),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_takes_address(start)
                || expr_takes_address(end)
                || step.as_ref().is_some_and(expr_takes_address)
                || body_takes_address(body)
        }
        BytecodeInstruction::Loop { body, .. } => body_takes_address(body),
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => expr_takes_address(scrutinee) || arms.iter().any(|a| body_takes_address(&a.body)),
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => body_takes_address(body) || body_takes_address(catch_body),
    }
}

fn expr_takes_address(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        BytecodeExprKind::Call { name, args } => {
            name == ADDR_OF_BUILTIN || args.iter().any(expr_takes_address)
        }
        BytecodeExprKind::Unary { expr: inner, .. } | BytecodeExprKind::Await { expr: inner } => {
            expr_takes_address(inner)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_takes_address(left) || expr_takes_address(right)
        }
        BytecodeExprKind::Array(values) => values.iter().any(expr_takes_address),
        BytecodeExprKind::Index { target, index } => {
            expr_takes_address(target) || expr_takes_address(index)
        }
        BytecodeExprKind::Field { target, .. } => expr_takes_address(target),
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Variable(_)
        | BytecodeExprKind::Closure { .. } => false,
    }
}
