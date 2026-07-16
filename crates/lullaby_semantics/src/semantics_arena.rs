//! Typing for the freestanding-tier **static-buffer arena** surface (§5 of
//! `documents/freestanding_tier_design.md`): the `region NAME in BUFFER`
//! declaration and the `arena_alloc(NAME, count)` bump builtin. Kept out of
//! `semantics_checker_calls.rs` and `lib.rs` (both already over the size cap) as a
//! cohesive `impl Checker` block.
//!
//! # Why this exists
//!
//! A `no-runtime` module has **nowhere bounded to put data**: `alloc`, `list`,
//! `map`, `string`, and `rc` are all `L0441`-rejected by the tier gate, by design
//! (each implies the host allocator, which a kernel does not have). A static-buffer
//! arena is the freestanding answer: the memory is a fixed `array<i64>` **the
//! caller already owns**, and allocation is a bump cursor within it. No host
//! allocator is involved at any point, so `L0441` correctly does not reject it —
//! and a fixture pins exactly that.
//!
//! # The delivered surface, and how it differs from §5's sketch
//!
//! §5 sketches `region work in kernel_scratch` as a *block* whose body's
//! allocations implicitly bump into the buffer, over a `static ... array<byte>`.
//! Two delivered facts make that sketch unbuildable as literally written, so this
//! module implements the **minimal consistent form** instead (recorded as a
//! deviation in §5's delivery record):
//!
//! 1. **There is no `region` block.** The delivered `region` is a flat *metadata
//!    declaration* (`region NAME: size=N, kind=static`) that lowers to a
//!    `region_create` marker and has no scoping or allocation behaviour. §5's claim
//!    that it "reuses the *exact* delivered `region` block grammar" is mistaken.
//!    So the arena form is a **statement** in that same delivered statement
//!    position, scoped to its enclosing block — which is what "reset at dedent"
//!    means for a region declared at the top of a block.
//! 2. **Implicit arena allocation is vacuous in `no-runtime`.** Every type whose
//!    allocation the escape analysis would arena-ize (`list`/`string`/`map`/`rc`)
//!    is `L0441`-rejected in this tier; what remains is by-value scalars, structs,
//!    and fixed arrays, which never heap-allocate. So an *implicit* bump would have
//!    literally nothing to allocate. Allocation is therefore **explicit** —
//!    `arena_alloc(work, n)` — which is also what a kernel actually writes.
//!
//! The bump unit is the **8-byte cell**, not the byte, and the buffer is
//! `array<i64>` rather than §5's `array<byte>`. That is forced by the delivered
//! native value model: every Lullaby scalar is a *normalized 8-byte cell* (an `i32`
//! local occupies a full sign-extended word), which is exactly why native `addr_of`
//! lowers for 8-byte scalars only. A byte-granular arena over `array<byte>` would
//! need a packed-byte representation that no tier has.
//!
//! # Diagnostics
//!
//! **`L0445`** — the static-buffer arena is malformed: the backing name is not in
//! scope, is not a fixed `array<i64>`, the region name collides, or an
//! `arena_alloc` does not name a static-buffer region in scope. One code, one
//! meaning ("this static-buffer arena is not well-formed"), which is what §5
//! proposed `L0445` for.
//!
//! `L0446`–`L0449` are deliberately **not** used here even though they are
//! unassigned: the design document proposes them for other, undelivered sections
//! (`naked fn` §6, `repr`/`align` §7, `panic fn` §8, `section` §9). The
//! interpreter refusal therefore takes **`L0460`**, past the delivered `L0459`
//! tail and unclaimed by any proposal.

use super::*;

/// The element type a static-buffer arena's backing buffer must have. See the
/// module docs: the native value model normalizes every scalar to an 8-byte cell,
/// so the arena's bump unit is the cell and the buffer is a cell array.
pub(crate) const ARENA_BUFFER_TYPE: &str = "array<i64>";

/// The `arena_alloc` builtin name.
pub(crate) const ARENA_ALLOC_BUILTIN: &str = "arena_alloc";

impl Checker<'_> {
    /// Check a `region NAME in BUFFER` static-buffer arena declaration (§5).
    ///
    /// `BUFFER` must name a binding **already in scope** whose type is a fixed
    /// `array<i64>`; the region takes its extent from that buffer. The region name
    /// is then recorded so `arena_alloc(NAME, n)` in the rest of the block resolves
    /// to it.
    pub(crate) fn check_region_arena(
        &mut self,
        decl: &RegionDecl,
        backing: &str,
        scope: &Scope,
        function: &Function,
    ) {
        let Some(buffer_ty) = scope.locals.get(backing).cloned() else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0445",
                format!(
                    "region `{}` is backed by `{backing}`, which is not a binding in scope; a \
                     static-buffer arena must name a fixed `{ARENA_BUFFER_TYPE}` buffer the \
                     caller already owns",
                    decl.name
                ),
                Some(function.name.clone()),
                decl.span,
            ));
            return;
        };
        if buffer_ty.name != ARENA_BUFFER_TYPE {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0445",
                format!(
                    "region `{}` is backed by `{backing}`, which is `{}`, but a static-buffer \
                     arena must be backed by a fixed `{ARENA_BUFFER_TYPE}`: the arena bumps in \
                     8-byte cells because every Lullaby scalar is a normalized 8-byte cell",
                    decl.name, buffer_ty.name
                ),
                Some(function.name.clone()),
                decl.span,
            ));
            return;
        }
        // Two arenas over ONE buffer would silently ALIAS. Each region gets its own
        // bump cursor starting at zero, so `region a in buf` and `region b in buf`
        // both hand out `&buf[0]` — two logically distinct arenas returning
        // overlapping cells, with every write through one clobbering the other. That
        // is a silent wrong answer, and it is exactly the shape §5's per-CPU-pool
        // motivation invites an author to reach for. Separate pools need separate
        // buffers; reject rather than corrupt.
        if let Some((existing, _)) = self
            .arena_regions
            .iter()
            .find(|(_, buffer)| buffer.as_str() == backing)
        {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0445",
                format!(
                    "region `{}` is backed by `{backing}`, which already backs region \
                     `{existing}`: two arenas over one buffer each bump from their own cursor, \
                     so they would hand out the SAME cells and silently clobber each other. \
                     Give each arena its own buffer",
                    decl.name
                ),
                Some(function.name.clone()),
                decl.span,
            ));
            return;
        }
        if !self.region_names.insert(decl.name.clone()) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "L0445",
                format!("duplicate region `{}`", decl.name),
                Some(function.name.clone()),
                decl.span,
            ));
            return;
        }
        self.arena_regions
            .insert(decl.name.clone(), backing.to_string());
    }

    /// `arena_alloc(region, count) -> ptr<T>`: bump `count` 8-byte cells out of
    /// `region`'s backing buffer and return a pointer to the first.
    ///
    /// `region` is a **region name**, not a value expression — it resolves to a
    /// `region ... in ...` declaration in scope (`L0446` otherwise), exactly as a
    /// `region` name is a compile-time entity everywhere else in the language.
    /// `count` is an `i64` cell count. The pointee `T` comes from the caller's
    /// expected annotation when it is a raw pointer, defaulting to `ptr<i64>` —
    /// mirroring the delivered `int_to_ptr` / `ptr_cast` context rule, since the
    /// raw-pointer builtins take no turbofish.
    ///
    /// `unsafe`-gated with `L0330` like every other raw-pointer producer: the
    /// result is an unchecked `ptr<T>` into caller memory.
    pub(crate) fn check_arena_alloc(
        &mut self,
        args: &[Expr],
        call_span: Span,
        expected: Option<&TypeRef>,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        self.expect_arg_count(ARENA_ALLOC_BUILTIN, args, 2, function)?;
        let region = match &args[0].kind {
            ExprKind::Variable(name) if self.arena_regions.contains_key(name) => name.clone(),
            _ => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "L0445",
                    format!(
                        "`{ARENA_ALLOC_BUILTIN}` requires the name of a static-buffer arena \
                         declared with `region <name> in <buffer>` in scope"
                    ),
                    Some(function.name.clone()),
                    args[0].span,
                ));
                return None;
            }
        };
        let _ = region;
        self.expect_arg_type(ARENA_ALLOC_BUILTIN, 2, &args[1], "i64", scope, function);
        self.require_unsafe(ARENA_ALLOC_BUILTIN, call_span, function)?;
        Some(
            expected
                .filter(|ty| ty.is_raw_pointer())
                .cloned()
                .unwrap_or_else(|| TypeRef::new("ptr<i64>")),
        )
    }
}
