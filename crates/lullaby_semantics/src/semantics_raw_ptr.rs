//! Typing for the freestanding-tier raw-pointer *addressing* surface (stage 2):
//! `addr_of` / `ptr_offset` / `ptr_cast`. Kept out of `semantics_checker_calls.rs`
//! (and `lib.rs`, already over the size cap) as a cohesive `impl Checker` block for
//! the raw-pointer builtins. See `documents/freestanding_tier_design.md` §2.2.
//!
//! All three are `unsafe`-gated exactly like the delivered raw-pointer builtins
//! (`L0330` outside `unsafe`) and require a *sized* pointee so element-scaled
//! arithmetic (`ptr_offset`) is well-defined. They are available in both tiers
//! under `unsafe`, and the `no-runtime` gate allows them (they yield an allowed
//! `ptr<T>` and are not host-allocator builtins).

use super::*;

impl Checker<'_> {
    /// `addr_of(place) -> ptr<T>`: the address of an addressable place — a local
    /// (`Variable`), an array element (`Index`), or a struct field (`Field`) — whose
    /// type `T` has a defined C-natural layout. A whole-array place decays to a
    /// pointer to its element type (so `ptr_offset` walks it), matching C array
    /// decay and the interpreters' region model. Taking the address of a temporary
    /// (a literal, a call result, arithmetic) is rejected with `L0458`.
    pub(crate) fn check_addr_of(
        &mut self,
        args: &[Expr],
        call_span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        self.expect_arg_count("addr_of", args, 1, function)?;
        let place = &args[0];
        if !matches!(
            place.kind,
            ExprKind::Variable(_) | ExprKind::Index { .. } | ExprKind::Field { .. }
        ) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0458",
                "addr_of requires an addressable place (a local, an array element, or a \
                 struct field); the address of a temporary cannot be taken"
                    .to_string(),
                Some(function.name.clone()),
                place.span,
            ));
            return None;
        }
        let place_ty = self.check_expr(place, scope, function)?;
        if !self.type_has_layout(&place_ty) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0431",
                format!(
                    "addr_of requires a place whose type has a defined memory layout but got `{}`",
                    place_ty.name
                ),
                Some(function.name.clone()),
                place.span,
            ));
            return None;
        }
        self.require_unsafe("addr_of", call_span, function)?;
        // A whole-array place decays to a pointer to its element type.
        let pointee = place_ty.array_element().unwrap_or(place_ty);
        Some(TypeRef::new(format!("ptr<{}>", pointee.name)))
    }

    /// `ptr_offset(p: ptr<T>, n: isize) -> ptr<T>`: element-scaled pointer
    /// arithmetic (`p + n*size_of(T)`). The pointee `T` must be a *sized* type so
    /// the scale factor is defined; an unsized `T` is rejected with `L0431`. The
    /// count `n` is an `isize`/`i64` signed element count. The result keeps the
    /// input pointer type.
    pub(crate) fn check_ptr_offset(
        &mut self,
        args: &[Expr],
        call_span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        self.expect_arg_count("ptr_offset", args, 2, function)?;
        let ptr_ty = self.check_expr(&args[0], scope, function)?;
        let pointee = self.expect_raw_pointer("ptr_offset", &ptr_ty, args[0].span, function)?;
        if !self.type_has_layout(&pointee) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0431",
                format!(
                    "ptr_offset scales by size_of<T>, so its pointer's pointee `T` must be a \
                     sized type, but `{}` has no defined layout",
                    pointee.name
                ),
                Some(function.name.clone()),
                args[0].span,
            ));
            return None;
        }
        let count_ty = self.check_expr(&args[1], scope, function)?;
        if count_ty.name != "i64" && count_ty.name != "isize" {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0331",
                format!(
                    "ptr_offset expects an `isize`/`i64` element count but got `{}`",
                    count_ty.name
                ),
                Some(function.name.clone()),
                args[1].span,
            ));
            return None;
        }
        self.require_unsafe("ptr_offset", call_span, function)?;
        Some(ptr_ty)
    }

    /// `ptr_cast(p: ptr<T>) -> ptr<U>`: reinterpret a raw pointer's pointee type
    /// with no value conversion. The target `U` comes from the caller's expected
    /// annotation when it is a raw pointer (mirroring `int_to_ptr`), defaulting to
    /// `ptr<i64>` when there is no annotation. This is the minimal-consistent
    /// spelling: the delivered raw-pointer builtins take no turbofish, so the target
    /// element type is supplied by the `let bp ptr<byte> = ptr_cast(base)` context.
    ///
    /// # `ptr_cast` preserves the pointer MODEL
    ///
    /// Lullaby has two pointer models and they are **not convertible**: the legacy
    /// `ptr_T` heap box that only `alloc` produces (the interpreters model it as a
    /// heap-SLOT INDEX over a one-cell `Vec<Option<Value>>`, not an address), and the
    /// modern `ptr<T>` raw address from `addr_of`/`int_to_ptr`. `let`/parameter
    /// binding already enforces that (`L0303`/`L0313`).
    ///
    /// `ptr_cast` used to be a hole in that wall, because it derived its result
    /// **purely from the caller's annotation and never from the operand**. That
    /// laundered a pointer across models in *both* directions:
    ///
    /// * `let q ptr<i64> = ptr_cast(alloc(8))` rewrote a box into an address, after
    ///   which `ptr_offset(q, 1)` type-checked — natively striding 8 bytes off a
    ///   one-cell payload into the next heap block's `[size]` header, the word
    ///   `__lullaby_alloc`'s free-list scan reads. Real allocator corruption.
    /// * `let fake ptr_i64 = ptr_cast(addr_of(buf[0]))` rewrote an address into a
    ///   box, falsifying the invariant that a `ptr_T`-typed expression is always
    ///   `alloc`-derived — which `native_object_rawptr.rs`'s `is_legacy_box_pointer`
    ///   spelling test relies on.
    ///
    /// Deriving the model from the operand closed both — but only at the **outer**
    /// type name, which left the same two directions open one level down, where no
    /// `ptr_T` is ever an operand and so every gate was *bypassed* rather than
    /// defeated. `addr_of` over a box place yields `ptr<ptr_i64>`, whose outer spelling
    /// reads modern:
    ///
    /// * `let pa ptr<i64> = ptr_cast(addr_of(a))` retargeted a box-typed pointee to
    ///   `i64`, erasing the model: `ptr_read(pa)` was a heap address natively and `0`
    ///   (the slot index) on the interpreters, and `ptr_write` through it then
    ///   SEGFAULTED natively while the interpreters raised `L0409`. An arbitrary
    ///   read/write primitive reachable from code that passed `check`.
    /// * `let pb ptr<ptr_i64> = ptr_cast(addr_of(buf[0]))` fabricated a box out of
    ///   ordinary array storage — needing no `alloc` at all — which native then
    ///   dereferenced.
    ///
    /// So the model is taken from the operand and the pointee is retargetable only
    /// when **no box model appears at any depth** on either side
    /// ([`mentions_box_model`]):
    ///
    /// * An operand mentioning a box (`ptr_T`, `ptr<ptr_i64>`, `ptr<ptr<ptr_i64>>`, …)
    ///   yields itself — a box is one opaque, tier-defined cell, so there is nothing
    ///   to reinterpret at any depth; the cast is an identity, and a model-crossing
    ///   annotation over it collides at the `let` (`L0303`).
    /// * A box-free `ptr<T>` operand yields `ptr<U>` from an annotation that is itself
    ///   a box-free address type ([`is_annotatable_address_type`]), defaulting to
    ///   `ptr<i64>`. Neither `ptr_U` nor `ptr<ptr_U>` captures it.
    ///
    /// This refuses **reinterpretation** across the model boundary, not the types
    /// themselves: `ptr<ptr_i64>` stays usable, because `ptr_read` of it reproduces
    /// each tier's own faithful box rather than reinterpreting one
    /// (`run_addr_of_box.lby`, 7 on all four tiers).
    ///
    /// The native backend keeps its own `refuse_legacy_box_pointer` gate on the
    /// operand as defense in depth. That gate stays an **outer** test deliberately —
    /// widening it to nested pointees would refuse the coherent `ptr<ptr_i64>` reads
    /// above, which are not a mismatch.
    ///
    /// # What is, and is not, a whole-program property
    ///
    /// This check was originally documented as making the backend's spelling test
    /// "sound as a whole-program property". **That over-claimed, and it still would.**
    /// `ptr_cast` was not the only annotation-governed pointer producer: `int_to_ptr`
    /// and `arena_alloc` carried the identical
    /// `expected.filter(|ty| ty.is_raw_pointer())` pattern. `arena_alloc` was a real
    /// third door and is now closed ([`annotated_address_type`]). `int_to_ptr` is
    /// **deliberately left open**, so the strong property is false and cannot be
    /// recovered. What actually holds:
    ///
    /// > **Every builtin whose pointer model is derivable now derives it, at every
    /// > nesting depth.** `alloc` is the only producer of the legacy spelling;
    /// > `ptr_cast` takes its model from the operand and retargets only box-free
    /// > pointees; `ptr_offset` preserves its operand's type; `addr_of` derives
    /// > `ptr<T>` from the place; `arena_alloc` yields only a box-free `ptr<T>`.
    /// > `let`/parameter binding (`L0303`/`L0313`) keeps the two spellings from meeting
    /// > anywhere else.
    ///
    /// The depth clause is not decoration. Every one of these producers was once
    /// correct about the model it *named* and blind to the model *nested inside* what
    /// it named, which is a distinct bug with the same consequence — and it is the
    /// reason the two prior laundering fixes missed the `ptr_cast(addr_of(box))`
    /// exploit entirely: they made each gate correct about its own operand, and that
    /// route never handed a gate a `ptr_T` to be correct about.
    ///
    /// > **`int_to_ptr` is the sole remaining exception, and it is irreducible.** Its
    /// > operand is an `i64`, and **an integer carries no provenance** — so neither
    /// > model is derivable from it, by construction rather than by omission. On the
    /// > interpreters an integer genuinely may be either (a heap-slot handle below
    /// > `RAW_POINTER_BASE`, a byte address above it), and both round trips are
    /// > delivered and fixture-pinned: `run_ptr_cast.lby` reconstructs a real box from
    /// > `ptr_to_int(box)` as `ptr_i64`, and `freestanding_mmio_vga.lby` names
    /// > `0xB8000` as `ptr<i64>`. Its annotation is therefore an **`unsafe`
    /// > assertion**, not an inference — and a false one (`let fake ptr_i64 =
    /// > int_to_ptr(ptr_to_int(addr_of(buf[0])))`) still compiles.
    ///
    /// Three ways to close it were designed and attacked; all three failed:
    ///
    /// * **Track provenance into the `i64`.** Defeated by arithmetic, arrays, and
    ///   function boundaries — the integer is an ordinary value.
    /// * **Split the builtin** (`int_to_ptr` for addresses, `int_to_box` for handles).
    ///   Disproven empirically: `int_to_ptr(753664)` — a *pure constant* — already
    ///   yields a `ptr_i64` under a legacy annotation, so `int_to_box` would launder
    ///   identically. Renaming relocates the assertion; it does not remove it.
    /// * **Refuse the `addr_of`-derived shape.** `run_ptr_cast.lby` launders through a
    ///   temp var, which is indistinguishable from the legitimate round trip.
    ///
    /// Only removing `ptr_to_int(box)` from the language closes it, and that is an
    /// owner-level surface decision, not a checker fix.
    ///
    /// So the spelling test is **not** sound whole-program, and this doc makes no
    /// claim that anything downstream compensates for that. In particular, do **not**
    /// read the backend's `refuse_legacy_box_pointer` gate as containment: it is a
    /// prefix test on the *outer* type name, it does not see a box model nested in a
    /// pointee (`ptr<ptr_i64>`), and it is not widened to — the nesting question is
    /// answered here at the frontend instead, because natively a nested box is often
    /// perfectly coherent and refusing it would be wrong. The gate guards the cases it
    /// names and nothing more.
    ///
    /// The property proved by the nesting fix is narrow and worth stating exactly:
    ///
    /// > **No `ptr_cast` or `arena_alloc` result can cross the box/address model
    /// > boundary at any nesting depth**, because neither derives a model from an
    /// > annotation any more. It says nothing about `int_to_ptr`, which still can and
    /// > is meant to; nothing about whether a box is live or aliased (`L0350`'s job);
    /// > and nothing about builtins added later — a new annotation-governed pointer
    /// > producer must route its expected type through
    /// > [`is_annotatable_address_type`] or it reopens this exact hole.
    ///
    /// What is **not** implied either way: this is a spelling property, not provenance
    /// analysis. It says a `ptr_T` came from `alloc`; it says nothing about *which*
    /// box, whether it is live, or whether two `ptr_T`s alias. Lifetime is `L0350`'s
    /// job, and its limits are documented in `semantics_lifetime_alias.rs`.
    pub(crate) fn check_ptr_cast(
        &mut self,
        args: &[Expr],
        call_span: Span,
        expected: Option<&TypeRef>,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        self.expect_arg_count("ptr_cast", args, 1, function)?;
        let ptr_ty = self.check_expr(&args[0], scope, function)?;
        self.expect_raw_pointer("ptr_cast", &ptr_ty, args[0].span, function)?;
        self.require_unsafe("ptr_cast", call_span, function)?;
        // Anything mentioning the box model — the legacy `ptr_T` itself, or a box
        // nested in a pointee (`ptr<ptr_i64>`, which `addr_of` over a box place
        // yields) — casts to itself: a box is one opaque, tier-defined cell, so there
        // is nothing to retarget at any depth. The cast stays an identity and a
        // model-crossing annotation then collides at the `let` (`L0303`).
        if mentions_box_model(&ptr_ty) {
            return Some(ptr_ty);
        }
        // A box-free modern `ptr<T>` retargets its pointee from an annotation that is
        // itself a box-free address type, so an annotation can neither relabel the
        // address as a box nor bury one in the pointee and fabricate it on read.
        Some(
            expected
                .filter(|ty| is_annotatable_address_type(ty))
                .cloned()
                .unwrap_or_else(|| TypeRef::new("ptr<i64>")),
        )
    }
}

/// Whether `ty`'s **outer** spelling is the legacy `ptr_T` box that only `alloc`
/// produces, as opposed to the modern `ptr<T>` address spelling. Mirrors
/// `native_object_rawptr.rs`'s `is_legacy_box_pointer`.
///
/// This is deliberately an *outer* test, and it is the wrong predicate for deciding
/// whether a type is model-faithfully reinterpretable — use [`mentions_box_model`]
/// for that. See its docs for the exploit the outer-only reading admitted.
fn is_legacy_box_spelling(ty: &TypeRef) -> bool {
    ty.name.starts_with("ptr_")
}

/// The top-level, nesting-aware type arguments of any `ctor<...>` spelling, whatever
/// the constructor: `ptr<ptr_i64>` yields `[ptr_i64]`, `map<i64, ptr_i64>` yields
/// `[i64, ptr_i64]`, `i64` yields `[]`. Also decomposes a `fn(A, B) -> R` spelling
/// into `[A, B, R]`.
///
/// Constructor-agnostic on purpose: [`mentions_box_model`] must find a box model
/// wherever it is nested, and enumerating constructors would reopen the hole for the
/// next one added.
fn nested_type_arguments(ty: &TypeRef) -> Vec<TypeRef> {
    if let Some((mut params, ret)) = ty.function_signature() {
        params.push(ret);
        return params;
    }
    let Some(open) = ty.name.find('<') else {
        return Vec::new();
    };
    let ctor = &ty.name[..open];
    ty.generic_args(ctor).unwrap_or_default()
}

/// Whether `ty` mentions the legacy `alloc`-box model at **any** nesting depth —
/// `ptr_i64` itself, `ptr<ptr_i64>`, `ptr<ptr<ptr_i64>>`, `array<ptr_i64>`, …
///
/// # Why depth matters: the outer-name-prefix hole
///
/// [`is_legacy_box_spelling`] is a prefix test on the OUTER type name, and reading it
/// as *the* model test was a live memory-safety hole. `addr_of` over a `ptr_i64` place
/// yields `ptr<ptr_i64>`: the outer spelling reads *modern*, so an outer-only test
/// never fires — and yet the pointee is a box, whose representation the tiers do not
/// agree on (a heap-slot index on the interpreters, a machine address natively).
/// `ptr_cast` could then retarget that pointee to `ptr<i64>`, erasing the box model
/// without any `ptr_T` ever being an operand, so **every gate was bypassed rather than
/// defeated**:
///
/// ```text
/// let a ptr_i64 = alloc(7)
/// unsafe
///     let pa ptr<i64> = ptr_cast(addr_of(a))   # checked clean
///     ptr_read(pa)                             # native: a heap ADDRESS; interpreters: 0
/// ```
///
/// The converse direction was equally open, and needed no `alloc` at all — an
/// annotation could *fabricate* a box out of ordinary storage, which native then
/// dereferenced:
///
/// ```text
/// let buf array<i64> = [0, 0]
/// unsafe
///     let pb ptr<ptr_i64> = ptr_cast(addr_of(buf[0]))
///     let fake ptr_i64 = ptr_read(pb)          # a "box" whose slot index is 0
/// ```
///
/// So the rule is symmetric and depth-insensitive: a box model is opaque, and storage
/// may be reinterpreted neither **from** nor **into** one.
///
/// # What this does NOT say
///
/// It does not refuse `ptr<ptr_i64>` as a type. That type is *coherent*: `ptr_read` of
/// it yields each tier's own faithful box (fixture `run_addr_of_box.lby` answers 7 on
/// all four tiers), because reading a box-typed cell reproduces a box rather than
/// reinterpreting it. Only **reinterpretation** across the model boundary is refused.
/// Nor is this provenance analysis: it is a spelling property, and `int_to_ptr`'s
/// annotation can still assert a box spelling over a non-`alloc` value (see
/// [`annotated_address_type`]).
fn mentions_box_model(ty: &TypeRef) -> bool {
    is_legacy_box_spelling(ty) || nested_type_arguments(ty).iter().any(mentions_box_model)
}

/// Whether `ty` is the modern `ptr<T>` raw-pointer spelling (an address), excluding
/// the legacy `ptr_T` box spelling that `TypeRef::is_raw_pointer` also admits.
///
/// An **outer** test: `ptr<ptr_i64>` satisfies it, because it genuinely is an address.
/// Pair it with [`mentions_box_model`] via [`is_annotatable_address_type`] when the
/// question is whether an annotation may *mint* the type.
pub(crate) fn is_modern_raw_pointer(ty: &TypeRef) -> bool {
    ty.generic_arg("ptr").is_some()
}

/// Whether an annotation-governed builtin whose own pointer model is **fixed to an
/// address** may take its result type from `ty`.
///
/// Two conditions, and both are load-bearing:
///
/// * [`is_modern_raw_pointer`] — the annotation cannot relabel the address as a box
///   (`let fake ptr_i64 = arena_alloc(pool, 1)`).
/// * `!`[`mentions_box_model`] — nor can it bury a box in the *pointee* and fabricate
///   one on read (`let pb ptr<ptr_i64> = arena_alloc(pool, 1)`, then
///   `ptr_read(pb)`, which natively dereferences whatever bits were in the cell).
///
/// The second was the hole the first alone left open. A rejected annotation does not
/// capture the result: the builtin yields its natural `ptr<i64>` and the `let` then
/// collides at the existing `L0303` wall — the same landing the model-preserving
/// `ptr_cast` fix uses.
pub(crate) fn is_annotatable_address_type(ty: &TypeRef) -> bool {
    is_modern_raw_pointer(ty) && !mentions_box_model(ty)
}

/// The result type of a builtin that **mints a fresh machine address** whose model is
/// therefore known, and takes only its *pointee* from the caller's annotation. Today
/// that is `arena_alloc`.
///
/// # Why `arena_alloc` is annotation-governed, but only over the pointee
///
/// `check_ptr_cast` takes its result *model* from the operand, because its operand is
/// a pointer and so carries a model to preserve. `arena_alloc(region, count)` has no
/// such operand — a region name is a compile-time entity, not a value — so the
/// annotation must supply the pointee. But the **model** is fixed by what the builtin
/// *is*: an arena cell is a real address bumped out of a caller-owned `array<i64>`,
/// the host allocator is never involved, and **only `alloc` produces a box**. So the
/// result is always the modern `ptr<T>` spelling.
///
/// # The laundering this closes
///
/// `arena_alloc` used to filter the annotation through `TypeRef::is_raw_pointer`,
/// which admits the legacy `ptr_T` box spelling too, so the annotation could mint a
/// lie:
///
/// ```text
/// let fake ptr_i64 = arena_alloc(pool, 1)
/// ```
///
/// — a value spelled "I am an `alloc` box" over an arena cell, falsifying the
/// invariant `native_object_rawptr.rs`'s `is_legacy_box_pointer` spelling test rests
/// on. Filtering the annotation meant that no longer captures the result; the builtin
/// yields its natural `ptr<T>` and the `let` then collides at the existing `L0303`
/// wall, exactly as the `ptr_cast` fix does.
///
/// That first filter tested only the **outer** spelling, which left the same lie
/// available one level down and needing no `alloc` anywhere in the program:
///
/// ```text
/// let pb ptr<ptr_i64> = arena_alloc(pool, 1)
/// let fake ptr_i64 = ptr_read(pb)          # a "box" made of whatever was in the cell
/// ```
///
/// — verified to SEGFAULT natively while the interpreters raised `L0409`. The filter
/// is therefore [`is_annotatable_address_type`], which rejects a box model at **any**
/// depth.
///
/// Refusing the legacy spelling here costs nothing real: there was never a legitimate
/// meaning for it. A `ptr_T` from `arena_alloc`, nested or not, was *always* a false
/// claim.
///
/// # Why `int_to_ptr` is NOT routed through here
///
/// `int_to_ptr` looks like the same pattern and is deliberately left alone. Its
/// operand is an `i64`, and on the interpreters an integer may be *either* model — a
/// heap-slot handle below `RAW_POINTER_BASE`, or a byte address above it. Both round
/// trips are delivered and fixture-pinned:
///
/// ```text
/// let back ptr_i64  = int_to_ptr(ptr_to_int(box))  # run_ptr_cast.lby — TRUTHFUL
/// let base ptr<i64> = int_to_ptr(753664)           # freestanding_mmio_vga.lby
/// ```
///
/// So `int_to_ptr` has **no derivable model** — an `i64` carries no provenance, and
/// restricting it to `ptr<T>` breaks the first fixture, which rebuilds a genuine box.
/// Its annotation is an `unsafe` assertion, not a hole. See `check_ptr_cast`'s
/// "What is, and is not, a whole-program property" for the closure designs that were
/// attacked and why each failed.
pub(crate) fn annotated_address_type(expected: Option<&TypeRef>) -> TypeRef {
    expected
        .filter(|ty| is_annotatable_address_type(ty))
        .cloned()
        .unwrap_or_else(|| TypeRef::new("ptr<i64>"))
}
