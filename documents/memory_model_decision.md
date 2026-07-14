# Native Memory Model — Implementation Staging

> **Superseded for the high-level decision by
> [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md)** (owner
> decision, 2026-07-14), which is canonical for the memory model and 1.0 identity.
> That decision is **arena-first**: arena / region allocation is the primary,
> default model; reference counting (`ref`/RC) is a **secondary, opt-in** tool for
> escaping data; raw pointers + manual memory are an `unsafe` escape hatch; and
> **Perceus is deferred / lower priority** (with arenas as the default, most
> allocation never touches RC). This document no longer sets the high-level model —
> it now details the **implementation staging under the arena-first model**: how the
> already-in-flight RC substrate lands as the secondary tool and how the arena-first
> default is built on top. Where the historical text below reads "RC-first", treat
> it as the *sequencing of the RC substrate*, not a claim that RC is the default.

This document records the implementation staging for Lullaby's **native backend**
memory model and the reasoning behind it. The mechanics and runtime behavior live in
[`lullaby_memory_management.md`](lullaby_memory_management.md).

## Context

The native backend is a **hand-written x86-64 byte emitter with no LLVM/Cranelift
and no SSA/instruction IR**. Today it **bump-allocates on the heap with no
reclamation** — fine for short-lived tools, untenable for real programs. Growable
`list`/`map`, heap `string` values, heap-payload `enum`/`match`, closures, and
`throw`/`try` currently make a function native-ineligible (it falls back to the
interpreter). The interpreters already implement the full value model, including
`rc<T>`; native is the tier catching up.

Four constraints shape the choice, in priority order:

1. **Fast compile** — ~14 ms whole-program vs ~192 ms C (`cl /O2`) and ~341 ms
   Rust (`rustc -O`). Anything that adds heavy *compile-time* analysis (a borrow
   checker; precise GC stack maps) erodes the headline advantage.
2. **Memory safety** — array indexing already traps out-of-bounds on every tier.
3. **Systems use** — deterministic, predictable timing and memory; no pauses.
4. **Value semantics** — arrays (and aggregates) pass by value / copy. This is an
   *asset*: most data does not alias, so reclamation is cheaper and escape
   reasoning is easier than in a reference-heavy language.

## The decision

**Arenas are the primary, default native heap model; reference counting is the
secondary, opt-in tool for escaping data** (canonical:
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md)). Neither is a
green-field choice — the RC substrate is already in flight and the language already
committed to reference counting as its *safe automatic* model:

- `language_specification.md` lists **"Reference Counting: automatic memory
  management without garbage-collection pauses"** as a core principle and sets the
  memory model to *"Reference counting"* explicitly against *"GC (pause risks)"* in
  its comparison table.
- The surface already ships it: `rc<T>` with `rc_new`/`rc_clone`/`rc_release`/
  `rc_get`/`rc_borrow`, plus non-owning `ref<T>` borrows and raw `ptr<T>` — all
  running today on the three interpreters.
- The safety diagnostics already assume an RC/ownership model: `L0350`
  (use-after-free / double-free) and `L0351` (a borrowed `ref<T>` may not escape
  its owner). The "a function may return an owning value or `rc<T>`, never a
  borrowed `ref<T>`" rule is exactly the cheap, *local* lifetime rule both arenas
  (escape reasoning) and RC need.

The arena-first decision demotes RC from the default to the escaping-data tool: most
allocation is scope-local and bump-allocates into an arena that bulk-frees at scope
exit, so it never touches a refcount. RC catches only the dynamic-lifetime / shared
minority. So the real question is not *which* model but *what native must emit* to
honor arena-first defaults with RC underneath as the safe escape for data that
outlives its region.

**Composition** (each ingredient is battle-tested; the combination is tuned to
Lullaby's constraints):

> **Arenas** as the primary, default scope-local zero-overhead model **+ reference
> counting** as the secondary semantic foundation for escaping data **+ non-owning
> `ref<T>` borrows** to keep owning cycles from forming **+ raw pointers / `unsafe`**
> as the freestanding escape hatch. **Perceus-style reuse is deferred** — an RC-path
> optimization only, no longer a headline lever now that arenas are the default.

Practically, the RC substrate ships **first** because it is already in flight (small,
syntax-directed, low-risk), then the arena-first default is built on top and becomes
the path most allocations take; RC remains for the escaping minority. This staging is
the *sequencing of the substrate*, not a statement that RC is the default model.

## Options weighed against Lullaby's constraints

Every model answers *"when is it safe to free this?"* — differing on **when** the
liveness is computed (compile time vs runtime) and **how** (proving, counting,
tracing).

### 1. Tracing garbage collection (mark/sweep, generational)
- **Pros:** simplest for the programmer; cycles handled for free; good throughput;
  no per-operation cost.
- **Cons (decisive here):** needs **precise stack maps** — at every safepoint the
  compiler must emit which registers/slots hold pointers. Producing those from a
  hand-written emitter with no SSA is a large new subsystem. Plus stop-the-world
  **pauses** (wrong for systems positioning), **non-deterministic** memory/timing,
  and a **collector runtime** in every binary. Contradicts the spec's anti-GC pitch.
- **Conservative GC** (Boehm-style: scan the stack, treat pointer-shaped words as
  roots) sidesteps stack maps and is far easier to bolt on — but can't move/compact,
  suffers false retention, and still ships a collector. It is "the GC you *could*
  implement here," and still a compromise on every axis. **Rejected.**

### 2. Reference counting
- **Pros:** **deterministic** (freed at scope exit, a predictable point); no
  pauses; **codegen is syntax-directed** — insert `inc`/`dec`/`drop` at
  binds/copies/scope-exits, no global analysis, no stack maps → **fits the emitter
  and preserves the 14 ms**. **Value semantics cut the cost:** copied (non-shared)
  aggregates need no refcount at all; only genuinely-shared handles do. Matches the
  shipped spec + `rc<T>`/`ref<T>` surface.
- **Cons:** **cycles leak** (`a→b→a` never reaches zero) — needs a `weak`/`ref`
  discipline or a later cycle collector; **refcount traffic** can cost ~10–30% on
  pointer-heavy hot loops; **drop-correctness across every exit edge** (`return`,
  `?`, `throw`, `match` arms) is fiddly bookkeeping — the main bug surface.
- **Chosen** (see composition below to mitigate the cons).

### 3. Ownership / borrow checker (Rust-like)
- **Pros:** **zero runtime overhead** (no counts, no collector, no headers); the
  fastest option; compile-time safety proofs.
- **Cons (decisive here):** the borrow checker is **heavy compile-time analysis** —
  *exactly the cost that makes Rust slow to compile*, so adopting it attacks the
  headline advantage we beat Rust on. It wants a control-flow graph + dataflow the
  no-IR emitter lacks, and brings the "fighting the borrow checker" tax that
  conflicts with Lullaby's easy/LLM-friendly goal. **Rejected** as the *primary*
  model. (Local *move* semantics — a much cheaper subset — are folded into the
  composition below.)

### 4. Regions / arenas
- **Pros:** nearly free (bump-allocate + one bulk free), deterministic, cache-
  friendly, zero per-object overhead; matches the docs' existing "regions" language.
- **Cons:** cannot express values that **escape** a region (returned upward, or
  captured by a longer-lived closure). Insufficient *alone*, but an excellent
  **layer underneath** RC.

### 5. Perceus-style RC (RC + reuse)
- RC refined with compile-time **reuse analysis** (Microsoft Research's *Perceus*;
  used by **Koka** and **Lean 4**): it elides redundant inc/dec pairs and, when a
  refcount is provably 1 and the object dies, **reuses its memory in place** —
  turning a functional-style update into in-place mutation. The result approaches
  ownership-level performance **without a borrow checker**, and its analysis is
  local enough to stay compile-fast. The single most relevant "fast RC" answer, and
  the reason RC need not mean "slow."

### Custom / novel?
"Inventing a memory model" almost always means **combining** the primitives above
(tracing / counting / proving / regions). Memory management is the one subsystem
where an unproven novelty causes use-after-free / corruption — the wrong place to
be original at the primitive level. The defensible originality is a **considered
composition** (§the decision), not new primitives.

## Staged implementation plan

Each stage is independently shippable and test-gated (native == interpreter parity;
deterministic refcount assertions). The staging reflects **build order, not model
priority**: the RC substrate (stages 1–3) is already in flight and ships first, but
under the arena-first decision the **arena layer (stage 4) is the primary, default
model** — most allocation takes that path, with RC serving the escaping minority.
The freestanding/`unsafe` tier (stage 5) is the big 1.0 leap, and Perceus (stage 6)
is deferred.

1. **A real allocator + RC runtime helpers.** ✅ **LANDED** (native, behavior-neutral).
   The bump allocator is now a **free-list allocator** with a 16-byte per-block RC
   header `[size][refcount]` (the returned pointer names the payload, so record
   offsets are unchanged; refcount at `[ptr − 8]`). `__lullaby_rc_dec` (`dec [ptr − 8]`,
   free-at-zero) and `__lullaby_rc_free` (push the block onto the LIFO free list
   `__lullaby_free_head`) are emitted as `.text` helpers; the allocator first-fit-reuses
   freed blocks. No drops were inserted at this stage, so it was behavior-neutral.
2. **Scope-based drop insertion (the core).** 🚧 **FIRST INCREMENT LANDED.** The
   backend inserts `rc_dec` (free-at-zero) for a **uniquely-owned, borrow-only
   `string` local in a loop body**, on the loop's fallthrough back-edge — reclaiming
   per-iteration temporaries that previously leaked and could exhaust the heap. A
   `string` concat whose operand is a fresh temp also frees that intermediate via the
   ownership-aware `__lullaby_str_concat_own(left, right, mask)` helper, so the common
   `to_string(i) + "…"` idiom fully reclaims (verified: 200k iterations / ~24 MB of
   records in a 1 MiB heap completes). The analysis is default-deny (any escape → not
   dropped → leak, never double-freed); early-exit edges leak safely. Remaining for
   this stage: `inc` for aliased/shared owners, non-concat intermediate temporaries,
   list/map/struct/enum drops, and drops on
   `return`/`break`/`continue`/`?`/`throw`/`match`-arm exit edges (not just
   fallthrough). At each binding/copy of a
   reference-counted value insert `inc`; at each scope exit insert `dec`/drop —
   **on every exit edge** (fallthrough, `return`, `?`/`throw`, `match` arms). Extend
   the existing scope-exit sequencing the backend already does for aggregates. This
   is the highest-blast-radius change and demands the strongest deterministic
   refcount test suite of any native increment so far. Prototype it in isolation
   first (see [RC prototype](#rc-prototype)).
3. **Eligibility unlock, one feature at a time.** Flip heap `string` values,
   growable `list`/`map`, heap-payload enums, then closures from
   "native-ineligible → demote" to emitted — each behind parity tests. Error
   unwinding (`?`/`throw`) reuses the same scope-exit drop sequence, so it needs no
   separate machinery.
4. **Arena layer — the primary, default model.** Scope-local, provably-non-escaping
   allocations bump-allocate into a scope arena and bulk-free at scope exit, skipping
   refcounting entirely. This is *not* merely a fast path bolted onto RC — it is the
   default path most allocation takes; RC (stages 1–3) becomes the secondary tool for
   the escaping minority. Needs a cheap, *local* "does this binding escape its scope?"
   heuristic (the same escape reasoning `L0351` already implies), **not**
   Tofte-Talpin/MLKit region *inference* (compile-time expensive; would threaten the
   fast-compile edge). Do arenas **before** any cycle collector — they cut refcount
   traffic and shrink where cycles even matter.
5. **Freestanding / `unsafe` / kernel tier — the big 1.0 leap.** Static-buffer-backed
   arenas (allocation with no host allocator), raw pointers + pointer arithmetic +
   `unsafe`, a pluggable panic handler for bounds/safety failures, and `no-runtime`
   output. **No RC in this tier** and no hidden allocation or control flow. See the
   10-item kernel checklist in
   [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md) (canonical);
   do not restate it here.
6. **Perceus reuse (deferred, lower priority) + (only if needed) a cycle collector.**
   Perceus optimizes refcount *traffic*, but with arenas as the default most
   allocation never touches RC, so it is deferred and no longer the headline perf
   lever it was under the RC-first plan. Add drop-specialization/reuse only once the
   RC-path traffic is measured to matter; add a trial-deletion cycle collector only if
   real programs demonstrably leak owning cycles.

## Biggest risks / unknowns

1. **Cycles leak** under non-cycle-collecting RC. Mitigate first with value
   semantics + non-owning `ref<T>`; measure whether idiomatic Lullaby even forms
   owning cycles before building a collector.
2. **Refcount traffic** on hot paths. Mitigate with the arena layer (skip
   refcounting non-escapers). The open question is elision *without* an optimizer
   IR — you need a cheap syntactic escape heuristic, not LLVM-style ARC elimination.
3. **Unwinding drop-correctness.** Every scope-exit edge must run the same drops
   exactly once — double-free or leak lives on the wrong edge. Very testable
   (deterministic refcount assertions), but budget the negative-test suite.

## Doc-conflict resolution

The high-level model decision is owned by
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md) (canonical):
**arena-first primary, RC secondary/opt-in, raw pointers as the `unsafe` escape
hatch, Perceus deferred, no tracing GC.** This document details the *implementation
staging* under that decision. [`lullaby_memory_management.md`](lullaby_memory_management.md)
covers the *mechanics and runtime behavior* and has been realigned to the same
arena-first framing (its historical tracing-GC sections are marked superseded).
`acceptance_criteria.md` lists arena/RC/region memory as *not-yet-built*. If any of
these disagree with the canonical doc, the canonical doc wins.
