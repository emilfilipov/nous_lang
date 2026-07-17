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
    /// `ptr_cast` used to be the one hole in that wall, because it derived its result
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
    /// The result model is therefore taken from the **operand**, and only the pointee
    /// within that model is retargetable:
    ///
    /// * A `ptr_T` operand yields exactly `ptr_T` — a box is one opaque cell, so
    ///   there is no pointee to reinterpret; the cast is an identity. A `ptr<U>` or
    ///   `ptr_U` annotation over it now correctly collides at the `let` (`L0303`).
    /// * A `ptr<T>` operand yields `ptr<U>` from a **modern** annotation only,
    ///   defaulting to `ptr<i64>`. A legacy `ptr_U` annotation no longer captures it.
    ///
    /// The native backend keeps its own `refuse_legacy_box_pointer` gate on the
    /// operand as defense in depth; this check is what makes that spelling test sound
    /// as a whole-program property rather than a patch applied from the backend.
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
        // A legacy `alloc` box casts to itself: one opaque cell, nothing to retarget.
        if is_legacy_box_spelling(&ptr_ty) {
            return Some(ptr_ty);
        }
        // A modern `ptr<T>` retargets its pointee from a modern annotation only, so a
        // legacy `ptr_U` annotation cannot relabel an address as a box.
        Some(
            expected
                .filter(|ty| is_modern_raw_pointer(ty))
                .cloned()
                .unwrap_or_else(|| TypeRef::new("ptr<i64>")),
        )
    }
}

/// Whether `ty` uses the legacy `ptr_T` spelling that only `alloc` produces, as
/// opposed to the modern `ptr<T>` address spelling. Mirrors
/// `native_object_rawptr.rs`'s `is_legacy_box_pointer`; keeping `ptr_cast` from
/// crossing the two spellings is what keeps that backend test sound.
fn is_legacy_box_spelling(ty: &TypeRef) -> bool {
    ty.name.starts_with("ptr_")
}

/// Whether `ty` is the modern `ptr<T>` raw-pointer spelling (an address), excluding
/// the legacy `ptr_T` box spelling that `TypeRef::is_raw_pointer` also admits.
fn is_modern_raw_pointer(ty: &TypeRef) -> bool {
    ty.generic_arg("ptr").is_some()
}
