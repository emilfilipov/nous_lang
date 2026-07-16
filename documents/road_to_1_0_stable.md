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
- **Decision: minimal const-eval in 1.0** — named compile-time constants and
  const-sized arrays. Full const-fn evaluation is post-1.0.
- **Named constants: SHIPPED.** `const NAME type = <expr>` parses (lexer `const`
  keyword + parser `ConstDecl`, `pub const` supported). Semantic analysis
  evaluates each initializer as a constant expression (literals + arithmetic/
  logical/bitwise/comparison/unary over literals and other constants, plus string
  concat), type-checks it, rejects non-constant initializers / type mismatches /
  cyclic / duplicate-or-colliding constants (`L0450`–`L0453`), and **folds every
  scope-aware reference into its literal before the checker runs** — so no backend
  (interpreters, native, WASM) sees a `const`; a folded constant in an all-`i64`
  function stays native-eligible. See `semantics_consts.rs`,
  `lullaby_type_system.md`, and `tests/fixtures/valid/const/`.
- **Next (remaining A2):** const-sized arrays (`array<T, N>` where `N` is a
  constant) — needs type-system work; the clean follow-up. Full const-fn
  evaluation stays post-1.0.

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

### B3. "Stable"-grade toolchain — **PARTIALLY SHIPPED**
Before stamping "stable": a built-in **test runner**, **debug info on Linux/macOS**
(DWARF — CodeView is Windows-only today), and **LSP + package-manager maturity**.
These are toolchain-completeness items, not language decisions.

- **Test runner — SHIPPED, with two known containment gaps (fix funded, below).**
  `lullaby test [--verbose] [--filter <substring>] <file.lby | project-dir |
  lullaby.json>` was already built and specified (`language_surface.md`) ahead of
  this item's audit — introduced `f4645f5` (2026-07-07) and shipped in tag
  `v1.0-preview`, so B3's "test runner" line was stale. The audit added `--filter`
  and the regression suite. Discovery is the `test_*` name convention (zero
  parameters, non-generic, returning `void`/`i64`/`bool`) over the **merged**
  program — so imported modules' `test_*` functions are discovered too — runs on
  the AST interpreter in deterministic order (source order within a file; loader
  merge order across modules), prints `PASS`/`FAIL` per test with a reason plus an
  `N passed, M failed` summary, and exits non-zero on any failure. Pinned by
  `crates/lullaby_cli/tests/cli/suite17.rs` (9 tests) over
  `tests/fixtures/test_runner/`.
  - **Isolation holds for every runtime error** — `assert(false)`, `throw`, A5
    contract violations (bounds fail, divide-by-zero), `L0423` — each is reported
    as a `FAIL` and the run continues. The mechanism is the execution tier, not
    A5: A5's abort-without-unwinding governs the *native* tier, while the runner
    runs on the AST interpreter, which returns these as ordinary `RuntimeError`s.
  - **Two gaps — CONFIRMED, verified by measurement.** A **stack overflow**
    (unbounded recursion) aborts the runner: remaining tests never run and no
    summary prints (exit is the OS fault code — Windows `0xC00000FD`; platform-
    specific, do not pin a value). A **non-terminating test hangs forever** —
    there is **no per-test timeout**. Both escape the interpreter's `Result`, so
    the in-process design cannot contain them.
  - **Next increment (FUNDED): subprocess isolation + a per-test timeout.** This
    is what closes both gaps **on the interpreter today** — it is not merely a
    prerequisite for a future `--backend`. Run each test in its own subprocess
    under a per-test deadline; report a crashed child as a `FAIL` and a timed-out
    child as a timeout `FAIL`, then continue and summarize. It additionally
    unblocks `--backend native|ir|bytecode` for `test`, which would separately
    need new named-function entry points (`lullaby_ir` exposes only `run_main`
    today). A runner that hangs on a non-terminating test does not meet the
    project's quality bar; this is scheduled work, not an accepted limitation.
- **Open sub-item — declaration surface (owner call).** The `test_*` convention is
  implicit magic: a name prefix silently confers semantics, which sits awkwardly
  with Lullaby's explicitness. A `test "name"` block (Zig-style) would be explicit,
  terse, natural in an indentation-only language, and would give the runner a real
  name string. It is **not** scheduled: the convention is specified in
  `language_surface.md` and ships in released binaries, so changing it is a
  breaking surface change and an owner decision.
- **DWARF debug info + LSP/package-manager maturity — still PLANNED.**

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
| B3 | Stable-grade toolchain | **PARTIAL** | test runner SHIPPED (+`--filter`, suite17); isolates every runtime error but a stack-overflow/non-terminating test still takes it down — subprocess+timeout FUNDED next; DWARF + LSP/pkg remain; `test` declaration surface (`test_*` vs `test "name"` block) is an open owner call | 2026-07-16 |

## Delivery progress (updated 2026-07-16)

- **Freestanding `no-runtime` tier — stage 1 SHIPPED** (main `f6186d3`). Module-level
  `no-runtime` directive + semantic gate: **L0441** rejects every heap/runtime path
  (heap types nesting-aware, actors/spawn/tell/await/async, closures, alloc/dealloc,
  and any expression whose value type is heap/runtime) in a no-runtime module; the
  allowed subset is scalars/fixed-arrays/scalar-aggregates/option/result/control-flow
  + the raw-pointer/`unsafe` surface. Reviewed PASS (adversarial gate-soundness probes
  — type-alias-hidden string, nested `option<list>`, cross-module imported heap helper
  — all rejected). Stages 2+ (static-buffer arenas, inline asm operands, MMIO/port-IO,
  interrupt/naked, direct-ELF/flat-binary) remain — the long pole to kernel capability.
  **Stage 2 (raw-pointer surface `addr_of`/`ptr_offset`/`ptr_cast`) IN FLIGHT.**
- **Actors — stages 1–3 SHIPPED** (AST interpreter tier; IR/bytecode/native/WASM keep
  clean deferral). Stage 2 = `ask`/`await`/`Future<R>` request-response with a
  deterministic run-to-completion scheduler (deadlock → L0356). Stage 3 = message
  ownership: move-by-default + use-after-send (L0357), type-driven copy set, `shared<T>`,
  and a fully transitive sendability predicate (L0353 now recurses into struct
  fields/enum payloads). Stage 4 (supervision/failure) is PARKED pending a design pass:
  under A5 (abort, no unwinding) a supervisor can't catch a panicking child — likely
  needs result-based actor failure. Directional; raise with owner before building.
- **A1 generics — COMPLETE across native + WASM.** Native monomorphization (scalar +
  one-level-heap-`string`) + **inherent-method dispatch** (generic AND non-generic,
  reviewed PASS, zero miscompiles). **WASM at A1 parity** (monomorphization shipped;
  method dispatch IN FLIGHT). Deeper-than-one-level heap generics defer cleanly on both.
- **B1 closures native — stage 1 SHIPPED** (scalar-capture direct-call, loop-reclaim
  proven bounded; reviewed PASS). Deeper captures / HOF / escape defer (L0339).
- **File-size discipline:** native_object.rs re-split under the 1500 cap after the
  closures growth (new native_object_closure_ctx.rs); semantics actor-ownership carved
  into semantics_actor_ownership.rs to keep lib.rs from growing.
- **Follow-up (trivial, queued):** freestanding formatter emits a double blank line
  after the directive + repositions header comments (idempotent, cosmetic); cross-module
  L0441 span attributed to the importing file. Fold into a later freestanding stage.

### Progress as of 2026-07-15

- **A1 generic user types — mostly shipped.** Frontend complete: generic structs,
  enums, methods, multi-parameter types, and trait bounds on generic types all run
  on the three interpreters (erasure). Native scalar-`T` monomorphization
  implemented (in review). Deferred: heap-`T` native monomorphization
  (per-instantiation drop-glue) and native inherent-method dispatch.
- **A2 const — SHIPPED** (named compile-time constants; const-sized arrays deferred).
- **A3 FFI callbacks — not started** (queued behind native).
- **A4 wrapping default + checked ops — SHIPPED** (i64 + fixed-width
  `checked_`/`saturating_`/`wrapping_` + `checked_div`/`checked_rem`; wrapping default
  documented).
- **A5 safe-tier failure semantics — SHIPPED on the interpreters** (audit confirmed
  conformance; regression-locked by `suite13`; canonical doc written). Follow-up: a
  native safety gap (list `get`/`set`/`pop` unchecked) is being fixed to trap like
  the interpreters.
- **B1 closures native / B2 stdlib stable-vs-extended tagging / B3 toolchain (DWARF,
  LSP/pkg maturity) — not started.**
- **Native optimization backlog:** O(n²) `for c in s` → O(n) SHIPPED (native strings
  ~2.4× C, from ~40×); scalars ≤ C. Array pass-by-ref measured a regression and was
  reverted; array-scan descriptor register-promotion is the remaining lever.
- **File-size backlog:** parser, semantics helpers, runtime, ir_optimizer,
  native_object_stmt (5,634→1,156), and native_object (4,519→1,477) all split under
  the cap; `semantics_checker_calls.rs` + `bytecode_vm.rs` remain (high-risk method
  partitions).
