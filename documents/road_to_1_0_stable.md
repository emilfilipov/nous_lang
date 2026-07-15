# Road to 1.0-stable — Decisions and Gaps

**Purpose:** the single tracking doc for what still needs deciding or building before
Lullaby can be stamped **1.0-stable** (a real API-stability promise, not preview).
Complements the architecture docs — it does not restate them:
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md) (identity,
memory model, two tiers, kernel scope), [concurrency_model_design.md](concurrency_model_design.md),
[freestanding_tier_design.md](freestanding_tier_design.md), and the split/opt
backlogs ([large_file_split_plan.md](large_file_split_plan.md),
[optimization_opportunities.md](optimization_opportunities.md)).

Status values: **DECIDED** (owner call made) · **PLANNED** (decided, needs
building) · **CONFIRMED GAP** (verified missing in the current compiler).

**Decision milestone — 2026-07-15:** the owner accepted all five A-recommendations
(A1–A5). They are now DECIDED and move into the implementation backlog.

## Already decided AND built (context — not open)
- **Module/package system** — file-as-module, `import` + `pub` exports, multi-file
  projects via a `lullaby.json` manifest with local path dependencies.
- **Numeric type set** — `i8…i64`, `u8…u64`, `isize`/`usize`, `f32`/`f64`, `bool`,
  `char`, `byte`.
- **FFI (base)** — `extern`/`export fn` over the C scalar + `ptr<T>` + `cstr` set
  (L0423/L0424/L0426).
- **Traits + bounded generics on functions** — `trait`/`impl` with receiver dispatch,
  `<T: Trait>` / `<T: A + B>` bounds.
- **Memory model** (arena-first + RC + unsafe), **concurrency** (actors) and
  **freestanding** surface syntax — all decided (see the architecture docs).

## Already decided, NOT yet built (the engineering bulk — tracked, not open)
Actors, the freestanding/kernel tier, arena stages 3–5 (explicit `region` blocks,
escape/promotion, static-buffer arenas), native-aggregate expansion + the native
optimization backlog (O(n²) `for c in s`, array-by-ref), and Linux tier-1 /
direct-ELF. These need no new decision — they're in flight or queued.

---

## A. Decisions (A1–A5 — DECIDED 2026-07-15, owner accepted the recommendations)

### A1. Generic user types — **DECIDED: in 1.0** (was CONFIRMED GAP)
User-defined generic types (`struct Stack<T>`, `enum Opt<T>`) do not parse today
(both → **L0205**). Bounded generic *functions* work; generic *types* do not.
- **Decision: ship in 1.0.** Reusable containers / a data-structure library require
  them; core to the "spanning set." Largest of the five.
- **Strategy (design complete — [generics_design.md](generics_design.md)):** HYBRID —
  type erasure on the interpreters (dynamic `Value`, free), monomorphization on
  native/WASM (per-instantiation layout + reclamation need the concrete `T`). Two
  sub-forks adopted per the design's recs (owner may veto): **inference-only
  construction** (no turbofish) and **recursion-through-indirection required**.
- **Next:** 5-stage impl (scalar-`T` struct → heap-`T` → generic enums + recursion
  rule → methods → multi-param + bounds). Queued behind A2-const (parser/semantics
  occupied) + native-aggregate (native occupied).

### A2. Const / compile-time evaluation — **DECIDED: minimal in 1.0** (was CONFIRMED GAP)
No `const` keyword or compile-time-constant story today (`const N i64 = 5` →
**L0201**; no `const` token in lexer/parser).
- **Decision: minimal const-eval in 1.0** — named compile-time constants and
  const-sized arrays. Full const-fn evaluation is post-1.0.
- **Next:** named constants (parser + semantic const-fold to literals) first;
  const-sized arrays as a follow-up (needs type-system work). Named-constant
  increment dispatched.

### A3. FFI completeness — **DECIDED: callbacks in 1.0**
Base FFI ships; deferred today (L0424): callbacks (fn pointers), struct-by-value,
and `string`/`list`/`map` marshalling.
- **Decision: callbacks (fn pointers across the C ABI) in 1.0.** Deep
  struct/collection marshalling stays post-1.0.
- **Next:** semantics marshalling-rule extension + native codegen for fn-pointer
  params/returns. Queued behind native-aggregate (native codegen is occupied).

### A4. Integer overflow semantics — **DECIDED: wrapping default + checked ops**
Arithmetic is wrapping everywhere today.
- **Decision: wrapping is the conscious 1.0 default; add explicit `checked_*` /
  `saturating_*` operations in the stdlib.** Document it as intentional.
- **Next:** verify what overflow/checked builtins already exist (native+WASM
  overflow builtins reportedly shipped), then fill the `checked_*`/`saturating_*`
  surface + document the wrapping default. Queued (touches native codegen).

### A5. Safe-tier failure semantics — **DECIDED: abort + diagnostic, no unwinding**
What a bounds-fail / unwrap-on-`none` / divide-by-zero does in the safe tier.
- **Decision: abort-with-diagnostic, no unwinding.** Deterministic, GC-free-friendly;
  recoverable errors flow through `result`/`?`/throw-catch, so panics are for bugs.
- **Next:** audit current bounds/unwrap/div-by-zero behavior across backends for
  consistency (native already traps via `ud2`), wire a clear diagnostic/message on
  abort, and document the guarantee. Partly verification. Queued.

---

## B. Planned but unscheduled — no decision needed, needs building

### B1. Closures native codegen — **PLANNED**
Closures run on the interpreters only today; native-AOT-completeness (the
"no silent native fallback" decision) requires native codegen for them. Sizeable
native-backend work; schedule after the arena/native-aggregate line settles.

### B2. Concrete stdlib contents — **PLANNED**
The API-stability *posture* is decided (freeze a small core, version the rest) but
the *contents* are not enumerated. Define the 1.0 stdlib surface — strings,
collections, math, fs, io, time, os, (maybe net) — and mark each item **stable** vs
**extended/experimental**. Do this near the finish line, informed by dogfooding.

### B3. "Stable"-grade toolchain — **PLANNED**
Before stamping "stable": a built-in **test runner**, **debug info on Linux/macOS**
(DWARF — CodeView is Windows-only today), and **LSP + package-manager maturity**.
These are toolchain-completeness items, not language decisions.

---

## Decision log
| # | Item | Status | Decision | Date |
|---|------|--------|----------|------|
| A1 | Generic user types | **DECIDED** | Ship in 1.0 | 2026-07-15 |
| A2 | Const / compile-time eval | **DECIDED** | Minimal in 1.0 (named consts + const-sized arrays) | 2026-07-15 |
| A3 | FFI completeness | **DECIDED** | Callbacks in 1.0; deep marshalling post-1.0 | 2026-07-15 |
| A4 | Integer overflow semantics | **DECIDED** | Wrapping default + stdlib `checked_*`/`saturating_*` | 2026-07-15 |
| A5 | Safe-tier failure semantics | **DECIDED** | Abort + diagnostic, no unwinding | 2026-07-15 |
| B1 | Closures native codegen | PLANNED | schedule post-arena | — |
| B2 | Concrete stdlib contents | PLANNED | enumerate near finish | — |
| B3 | Stable-grade toolchain | PLANNED | test runner + DWARF + LSP/pkg | — |

**Scheduling note.** A1 (generics design spike) and A2 (named constants) are startable
now (parser/semantics are free). A3/A4/A5 touch native codegen and are queued behind
the in-flight native-aggregate work.
