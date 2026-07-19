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
//! 1. **Every Lullaby SCALAR is a normalized 8-byte cell.** An `i32` local does
//!    not occupy 4 bytes of frame; it occupies a full sign-extended word. A
//!    width-correct 4-byte store through that word's address would leave the
//!    upper half stale and corrupt the cell invariant every other native path
//!    relies on. So `addr_of` of a narrow *scalar* is refused.
//!
//!    A narrow **array element** is different, and is lowered: an `array<i32>`
//!    stores PACKED 4-byte elements (see `NativeType::Narrow`), so its element's
//!    storage width and the `ptr<i32>` pointee width coincide exactly like `i64`'s
//!    do. Both cases spell the same type name, so the gate cannot be a name test —
//!    it is [the width-agreement law](`lower_addr_of`): *the addressed storage must
//!    be exactly as wide as the pointee claims*. The scalar's storage is 8 bytes
//!    against a 4-byte pointee and is refused; the packed element's is 4 against 4
//!    and is lowered.
//! 2. **A pointer must only be taken to storage that actually exists.** A
//!    register-promoted local lives in `rbx`/`rsi` and has no address (see
//!    "Register promotion" below); a closure capture lives in the env block, not
//!    a frame slot; and a fat-pointer array parameter's data pointer aliases the
//!    *caller's* storage, which the fat-array ABI may only read (a write through
//!    an address into it would break the read-only assumption that makes the
//!    no-copy descriptor value-semantically safe). Each of those is refused here
//!    and the function skips cleanly (`L0339`) with a precise reason.
//!
//! # Buffer walking works: the aggregate layout ASCENDS
//!
//! Native stack aggregates lay their words out at **ascending** (C-compatible)
//! addresses: word `k` of a struct/array in a frame lives at `[rbp - (slot -
//! 8*k)]` and, through an aggregate pointer, at `[ptr + 8*k]` (see
//! `emit_mov_rax_disp_from_rcx`'s positive displacements). So a field/element at
//! `offset_of == +8` really sits 8 bytes **higher** in memory, and
//! `ptr_offset(addr_of(buf[0]), 1)` steps **forward** to `buf[1]` — agreeing with
//! C, with `size_of`/`offset_of`, and with the interpreters.
//!
//! That makes THE kernel idiom — `addr_of(buf[0])` + `ptr_offset` to walk a
//! buffer — compile and be correct. `addr_of` of an **array element** (constant
//! or runtime index) and of a **whole array** (decaying to `ptr<element>`) are
//! both lowered, and compose with `ptr_read`/`ptr_write`/`ptr_cast`.
//!
//! This holds for a **narrow-element** buffer too — `array<i32>`, `array<u8>`,
//! `array<i16>`, … — because those arrays are PACKED at the element's C width, so
//! `ptr_offset(p, 1)` strides `size_of(T)` and lands on element 1 exactly as C
//! would. That is what makes a driver's byte buffer walkable. It is also forced
//! rather than chosen: the interpreters' `addr_of` region stride is
//! `Value::layout_size` of the element (4 for `i32`, 1 for `byte`), so they already
//! DEFINE these walks — `tests/fixtures/valid/raw_ptr_addressing.lby` exits 18 on
//! all three with an i32 size law of 4. An 8-byte-cell native layout would answer
//! 22: the same defined program with two answers. Packing is what makes native
//! agree.
//!
//! `ptr_offset(addr_of(s.lo), 1)` likewise genuinely reaches `s.hi` now. Note the
//! *interpreters* refuse inter-field walking via their region model (each
//! `addr_of` place is a single-cell snapshot), so that particular shape is
//! **native-only** — a program the interpreters do not define, which native is
//! free to define. Where the interpreters DO define the program (a read/walk
//! within one array), native agrees with them exactly; the same split applies to
//! a `ptr_write` through an `addr_of` pointer, which the interpreters refuse with
//! `L0459` and native implements.
//!
//! The nested paths `addr_of(s.arr)` / `addr_of(s.arr[i])` resolve correctly
//! through the shared path resolver, but are not reachable today for a *separate*
//! reason: an `array<T>`-typed struct FIELD is not in the native struct layout at
//! all (`resolve_struct_fields` rejects it, since a bare `array<T>` type carries
//! no length), so such a struct skips before `addr_of` is ever consulted. That is
//! a pre-existing layout gap, not a raw-pointer one.
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
///
/// Also the pointee gate for an `alloc` heap box (`native_object_heapbox.rs`), for
/// the same width reason: the box's initializing store and the `ptr_read` load must
/// agree exactly.
pub(crate) fn is_addressable_word_type(name: &str) -> bool {
    matches!(name, "i64" | "u64" | "isize" | "usize") || is_raw_pointer_type_name(name)
}

/// Whether `name` is the **legacy `ptr_T` spelling** that only `alloc` produces, as
/// opposed to the typed `ptr<T>` spelling. The two are distinct types to the checker
/// (`let p ptr<i64> = alloc(8)` is `L0303`) and are not interchangeable at a function
/// boundary (`L0313`), so a `ptr_T`-typed expression is always an `alloc`-derived
/// heap box.
///
/// **This is a spelling test, not a provenance analysis** — and that distinction is
/// load-bearing, because `int_to_ptr`'s annotation can still assert the `ptr_T`
/// spelling over a value that is not `alloc`-derived (its operand is an `i64`, which
/// carries no provenance; the assertion is what `unsafe` buys). So "`ptr_T` implies
/// `alloc`-derived" is a **heuristic here, not a theorem**.
///
/// It is also a test on the **outer** type name only: a box model nested in a pointee
/// (`ptr<ptr_i64>`, which `addr_of` over a box place produces) is invisible to it. See
/// [`refuse_legacy_box_pointer`]'s "What this gate does NOT do" and
/// `semantics_raw_ptr.rs`'s "What is, and is not, a whole-program property" for the
/// precise boundary.
fn is_legacy_box_pointer(name: &str) -> bool {
    name.starts_with("ptr_")
}

/// Refuse `builtin` when its pointer operand is an `alloc`-derived heap box, so the
/// enclosing function skips cleanly (`L0339`) instead of computing an answer the
/// interpreters define differently.
///
/// The interpreters represent an `alloc` box as `Value::Ptr(slot_index)` over a
/// `Vec<Option<Value>>`, not as an address, and the box is **one cell**:
///
/// * `ptr_to_int(alloc(7))` is `0` on the interpreters (the first slot index) —
///   natively it would be a real heap address. A defined program, two different
///   answers.
/// * `ptr_offset(p, 1)` over a box is REFUSED by the interpreters at run time
///   (`L0406`: "ptr_offset requires a pointer produced by addr_of"). Natively it
///   would stride 8 bytes past a one-cell payload straight into the NEXT block's
///   `[size]` header — the word `__lullaby_alloc`'s free-list first-fit scan reads to
///   decide reuse — so a write through it corrupts allocator metadata. Not garbage:
///   *active* garbage.
///
/// # Why `ptr_cast` is gated too
///
/// **Historically, `ptr_cast` laundered the spelling this gate keys on.**
/// `check_ptr_cast` (`semantics_raw_ptr.rs`) used to derive its result type from the
/// **caller's expected annotation**, defaulting to `ptr<i64>`, and *never* from the
/// operand. So `let q ptr<i64> = ptr_cast(p)` rewrote a `ptr_i64` box into a
/// `ptr<i64>` — laundering away the very spelling this gate keys on, after which
/// `ptr_offset(q, 1)` sailed through.
///
/// `check_ptr_cast` now takes the result's pointer **model** from the operand, so
/// that laundering is rejected at the frontend (`L0303`) and this gate is no longer
/// the only thing standing between it and corruption. The gate stays as **defense in
/// depth**, and it is still genuinely reachable — a *model-preserving* identity cast
/// (`let q = ptr_cast(p)`, which the frontend allows and which keeps the `ptr_i64`
/// model) delivers a real box to this call, and it must be refused. That reachable
/// shape is what `native_object_heapbox_tests.rs` and the `gen_alloc_cast_launder_program`
/// fuzzer now exercise; without it, both would have degraded into frontend-only tests
/// that stay green while this gate rots.
///
/// # Scope: this gate guards the cases it names, and nothing more
///
/// The frontend audit behind the original "complete, not whack-a-mole" claim was
/// wrong. `ptr_cast` was described as "the only builtin whose result type ignored its
/// operand"; in fact `int_to_ptr` and `arena_alloc` carried the identical
/// annotation-driven pattern. The verified enumeration of every pointer producer:
///
/// * `alloc` — the ONLY producer of the legacy `ptr_T` box spelling.
/// * `check_ptr_offset` returns `Some(ptr_ty)` — it *preserves* the operand's type.
/// * `check_addr_of` derives `ptr<T>` from the addressed **place**.
/// * `check_ptr_cast` takes its result *model* from the operand.
/// * `arena_alloc` is annotation-governed but filtered to the modern `ptr<T>`
///   spelling, so it cannot claim `alloc` provenance (fixed alongside this).
/// * **`int_to_ptr` is annotation-governed over BOTH spellings, deliberately.** Its
///   operand is an `i64`, which carries no provenance, and both round trips are real:
///   `int_to_ptr(ptr_to_int(box))` truthfully rebuilds a box (`run_ptr_cast.lby`),
///   `int_to_ptr(0xB8000)` truthfully names an address (`freestanding_mmio_vga.lby`).
///   Its annotation is an `unsafe` assertion and it **can be false**, so a `ptr_T` here
///   may not be `alloc`-derived. That route is irreducible (an `i64` carries no
///   provenance) and is documented in `semantics_raw_ptr.rs`, not compensated for here.
///
/// # What this gate does NOT do
///
/// It is a **prefix test on the OUTER type name** (`is_legacy_box_pointer`). So it does
/// not see a box model nested inside a pointee: `addr_of` over a `ptr_i64` place yields
/// `ptr<ptr_i64>`, whose outer spelling is *modern*, and this gate never fires on it.
/// The pointer-model mismatch is therefore reachable in shapes whose operand is never
/// spelled `ptr_T` at all, and this gate is **not** a containment for the mismatch in
/// general — only for the cases it names. Do not reason "the gate refuses it" about any
/// shape not literally covered above, and do not treat the frontend's spelling rules as
/// backed up by this gate.
///
/// It closes the **cross-function** route for free: a model-preserving helper
/// (`fn rebox p ptr_i64 -> ptr_i64` whose body is `ptr_cast(p)`) has the `ptr_T`
/// operand *at its own `ptr_cast` site*, so it refuses there, skips, and the
/// demotion fixpoint then skips every caller. (The model-*changing* helper this once
/// closed — `fn launder p ptr_i64 -> ptr<i64>` — no longer type-checks at all.)
///
/// A `ptr<T>` operand from `addr_of`/`int_to_ptr` is unaffected: it keeps its full
/// existing lowering, including the buffer-walking `addr_of(buf[0])` + `ptr_offset`
/// kernel idiom.
fn refuse_legacy_box_pointer(pointer: &BytecodeExpr, builtin: &str) -> Result<(), String> {
    if !is_legacy_box_pointer(&pointer.ty.name) {
        return Ok(());
    }
    Err(format!(
        "`{builtin}` over the `{}` produced by `alloc` is not lowered natively: the \
         interpreters model an `alloc` box as a heap-SLOT INDEX over one cell, not as an \
         address, so `ptr_to_int` of it is a slot number (not a machine address) and \
         `ptr_offset` over it is refused outright (`L0406`). Natively these would answer \
         differently, so the function runs on the interpreters instead. Use an \
         `addr_of`-derived `ptr<T>` for pointer arithmetic and address identity",
        pointer.ty.name
    ))
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
            // `ptr_to_int` of an `alloc` box exposes the pointer's numeric identity,
            // which the interpreters define as a heap-slot index rather than an
            // address. `ptr_cast` of one is refused alongside it: the frontend now
            // makes `ptr_cast` model-preserving, so a `ptr_i64` operand here is a
            // genuine box (an identity cast), and striding or numbering it would
            // answer differently from the interpreters. Both go through
            // `refuse_legacy_box_pointer`.
            //
            // `int_to_ptr` is NOT gated here: it takes an `i64`, so there is no pointer
            // operand to test. Its own model-laundering route — a `ptr_T` ANNOTATION
            // capturing the result — is deliberately OPEN at the frontend and cannot be
            // closed there (an `i64` carries no provenance, so neither model is
            // derivable; see `semantics_raw_ptr::check_ptr_cast`'s "What is, and is not,
            // a whole-program property"). Nothing here compensates for that, and this
            // gate must not be read as if it did.
            if matches!(name, "ptr_to_int" | "ptr_cast") {
                refuse_legacy_box_pointer(operand, name)?;
            }
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
            // An `alloc` box is ONE cell and the interpreters refuse to stride over
            // it at all (`L0406`); natively a stride would walk into the allocator's
            // RC header. Refuse rather than define it as garbage.
            refuse_legacy_box_pointer(pointer, name)?;
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

/// The element type named by an `array<T>` spelling, or `None` for a non-array
/// type name. Used by the whole-array decay `addr_of(a) -> ptr<T>`.
fn array_element_name(name: &str) -> Option<&str> {
    name.strip_prefix("array<")?.strip_suffix('>')
}

/// `addr_of(place) -> ptr<T>`: the REAL address of the addressed frame word,
/// `lea rax, [rbp - slot]` (or a computed address for a runtime array index).
///
/// Two place shapes are lowered:
///
/// * An **8-byte scalar** at an aggregate access path — a scalar local/parameter,
///   a struct-field path (`s.f`, `s.a.b`), an **array element** (`a[i]`,
///   `s.arr[i]`, constant or runtime index).
/// * A **whole array** decaying to a pointer to its element 0 — `addr_of(a)` /
///   `addr_of(s.arr)` where the place types as `array<T>` and the call types as
///   `ptr<T>`.
///
/// Because the aggregate layout ASCENDS (module docs), a pointer produced here is
/// C-walkable: `ptr_offset(addr_of(buf[0]), 1)` reaches `buf[1]`. Every other
/// shape skips cleanly — a narrow/float scalar (the normalized-cell hazard), a
/// register-promoted local, a closure capture, and a fat-pointer array parameter
/// (the read-only caller-storage hazard).
fn lower_addr_of(
    ctx: &mut NativeCtx,
    place: &BytecodeExpr,
    expr_ty: &TypeRef,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // The call must type as `ptr<P>` for some pointee `P` (the type checker infers
    // it); everything below agrees `P` against the place.
    let pointee = raw_pointee_name(&expr_ty.name)
        .ok_or_else(|| {
            format!(
                "`addr_of` must type as a `ptr<T>` on the native backend, found `{}`",
                expr_ty.name
            )
        })?
        .to_string();

    // Classify the place: an 8-byte scalar addressed directly, or an `array<T>`
    // decaying to a pointer to its element 0.
    let decays_from_array = match array_element_name(&place.ty.name) {
        Some(elem) => {
            // `addr_of(a)` where `a array<T>` types as `ptr<T>`: the pointee must be
            // the ELEMENT type, and that element must be an 8-byte cell so a
            // `ptr_read`/`ptr_write` through it is width-exact.
            if pointee != elem {
                return Err(format!(
                    "`addr_of` of an `{}` place typed as `{}` is not lowered natively (a \
                     whole-array decay must point at the element type `{elem}`)",
                    place.ty.name, expr_ty.name
                ));
            }
            // No width gate here: an array element may legitimately be an 8-byte
            // cell OR a packed narrow element, and the two are indistinguishable by
            // type NAME. The width-agreement law below decides it from the resolved
            // layout, which is the thing that is actually true about the storage.
            true
        }
        None => {
            // A directly-addressed non-array place is resolved and then held to the
            // width-agreement law below. A narrow SCALAR (`i32`/`byte`) local
            // resolves to a normalized 8-byte `I64` cell while its pointee says
            // 1/2/4, so the law refuses it — the same outcome the old name-based
            // gate produced, now derived from the storage rather than asserted. A
            // float/string/struct place fails the law's layout match.
            //
            // Defensive agreement between the place's type and the checker-inferred
            // pointee: `addr_of(x)` must type as `ptr<typeof x>`. A mismatch means
            // an assumption here no longer holds, so skip rather than emit an
            // address whose pointee width the reader would get wrong.
            if pointee != place.ty.name {
                return Err(format!(
                    "`addr_of` of a `{}` place typed as `{}` is not lowered natively (the \
                     pointee must be the place's own type)",
                    place.ty.name, expr_ty.name
                ));
            }
            false
        }
    };

    // Decompose the place into a root local plus field/index steps. An `Index` step
    // is now lowered: the ascending layout makes an element pointer C-walkable.
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = place;
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
            _ => {
                return Err(
                    "`addr_of` must address a local variable, a struct field, or an array \
                     element on the native backend (a temporary has no stable address)"
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
    // A fat-pointer array parameter's data pointer aliases the CALLER's storage,
    // which the no-copy fat-array ABI is only sound for because the parameter is
    // READ-ONLY. Handing out an address into it would let `ptr_write` mutate the
    // caller's array, breaking that value-semantic guarantee — so refuse, rather
    // than silently make a read-only parameter writable.
    if matches!(ctx.local(root)?.ty, NativeType::FatArray { .. }) {
        return Err(
            "`addr_of` into a fat-pointer (runtime-length) array parameter is not lowered \
             natively: the descriptor shares the caller's storage read-only, so an address \
             into it could be used to mutate the caller's array"
                .to_string(),
        );
    }

    // A whole-array decay is exactly the address of the array's ELEMENT 0, so
    // append a synthetic constant index-0 step and let the shared resolver walk it.
    // The resolver also validates that the place really is a native `Array` and
    // (via its literal bounds check) that it is non-empty.
    let zero = BytecodeExpr {
        kind: BytecodeExprKind::Integer(0),
        ty: TypeRef::new("i64"),
        span: place.span,
    };
    if decays_from_array {
        steps.push(PathStep::Index(&zero));
    }

    let (place_slot, ty) = resolve_place_steps_typed(ctx, root, &steps)?;

    // THE WIDTH-AGREEMENT LAW, and the whole soundness argument for narrow places:
    //
    //   the STORAGE the address names must be exactly as wide as the POINTEE type
    //   says it is.
    //
    // A `ptr<T>` promises its holder that `ptr_read`/`ptr_write` moves `size_of(T)`
    // bytes there and that `ptr_offset(p, 1)` steps `size_of(T)` bytes. Both are
    // true exactly when the storage width equals `size_of(T)` — so this one check
    // subsumes every ad-hoc gate that used to stand in for it, and decides the two
    // same-named cases correctly *because it asks the resolved layout rather than
    // the type name*:
    //
    // * `addr_of(x)` where `x` is an `i32` LOCAL resolves to `NativeType::I64` — a
    //   narrow scalar is still a normalized 8-byte cell — while the pointee `i32`
    //   says 4. 8 != 4, so it is REFUSED, exactly as before: a 4-byte store through
    //   that address would leave the cell's upper half stale.
    // * `addr_of(a[0])` where `a` is an `array<i32>` resolves to
    //   `NativeType::Narrow { bytes: 4 }` — the element is PACKED — and the pointee
    //   `i32` says 4. 4 == 4, so it is LOWERED, and the resulting pointer is
    //   C-walkable and agrees with the interpreters' `size_of(element)` region
    //   stride.
    //
    // The two spell the same type name (`i32`), so no name-based gate could tell
    // them apart; the resolved layout can, and it is the thing that is actually
    // true about the storage.
    let storage_bytes = match &ty {
        NativeType::I64 => 8,
        NativeType::Narrow { bytes, .. } => *bytes as i64,
        _ => {
            return Err(
                "`addr_of` must resolve to a single integer/pointer word or a packed narrow \
                 array element on the native backend"
                    .to_string(),
            );
        }
    };
    let access = pointee_access(&pointee).ok_or_else(|| {
        format!(
            "`addr_of` producing a `ptr<{pointee}>` is not lowered natively: the native \
             raw-pointer surface supports integer and pointer pointees (`i8`…`u64`, \
             `isize`/`usize`, `byte`, `ptr<U>`) only"
        )
    })?;
    if access.size != storage_bytes {
        return Err(format!(
            "`addr_of` of a `{}` place typed as `{}` is not lowered natively: the addressed \
             storage is {storage_bytes} bytes wide but the pointee `{pointee}` is \
             {} — a read or write through the address, or a `ptr_offset` stride, would \
             disagree with the storage. (A narrow SCALAR is stored as a normalized 8-byte \
             cell; only a narrow ARRAY ELEMENT is packed to its C width.)",
            place.ty.name, expr_ty.name, access.size
        ));
    }
    match place_slot {
        ScalarPlace::Const { slot } => {
            // A promoted local lives in `rbx`/`rsi` and has no address.
            // `body_takes_address` disables promotion for any function containing an
            // `addr_of`, so this must not fire; assert it here so a future change to
            // the promotion gate turns into a clean skip instead of a silent
            // miscompile (a `lea` of an unread frame slot).
            if ctx.promoted_reg(slot).is_some() {
                return Err(
                    "`addr_of` of a register-promoted local has no address (the promotion \
                     gate must exclude address-taken functions)"
                        .to_string(),
                );
            }
            emit_lea_rax_local(code, slot);
        }
        // A runtime array index (`addr_of(buf[i])`): compute the element's real
        // effective address. The shared resolver emits the same UNSIGNED bounds
        // check an ordinary `buf[i]` read does, so an out-of-range index traps
        // (`ud2`) exactly as it would on the interpreters (`L0413`) rather than
        // handing back a pointer to adjacent stack memory. An array root is never
        // register-promoted (promotion only picks `i64` scalars), so there is no
        // promotion hazard on this path.
        place_slot @ ScalarPlace::Dynamic { .. } => {
            emit_dynamic_addr_into_rcx(ctx, &place_slot, code)?; // rcx = &element
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
        }
        // Refused above (a fat-array root skips before resolving), but kept as a
        // hard gate rather than an `unreachable!`.
        ScalarPlace::FatIndex { .. } => {
            return Err(
                "`addr_of` into a fat-pointer array parameter is not lowered natively".to_string(),
            );
        }
    }
    Ok(())
}

/// `lea rax, [rbp - slot]` — the address of a frame word.
pub(crate) fn emit_lea_rax_local(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8D, 0x85]); // lea rax, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// Load the pointee at `[rcx]` into `rax`, extended to the normalized 8-byte cell.
pub(crate) fn emit_load_through_rcx(code: &mut Vec<u8>, access: PointeeAccess) {
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
pub(crate) fn emit_store_through_rcx(code: &mut Vec<u8>, access: PointeeAccess) {
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
        BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::RegionBlock { body, .. } => {
            body_takes_address(body)
        }
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
