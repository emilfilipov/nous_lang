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
> **direct-ELF SHIPPED** (`elf_image.rs`): a linker-free `ET_EXEC` executable emitter
> for `--freestanding` Linux programs — the ELF analog of direct-PE. Fixed load base
> `0x400000`, one page-aligned `PT_LOAD` per permission set (R+X / R / R+W, W^X
> enforced by construction), every relocation resolved to a final REL32 at emit time
> (no load-time fixup), clean `None` fallback to the object+`ld.lld` path for anything
> outside the freestanding-linkable subset. **Execution-verified** under `linux/amd64`
> Docker with no linker (37, 11), reviewed PASS. So the freestanding **output** side
> now runs linker-free on both Windows (PE) and Linux (ELF).
>
> **Inline `asm` operand binding SHIPPED** (`ade219e`, owner-chose Option 1 — bytes +
> operand block, explicit registers, no assembler): `asm <bytes>` gains an indented
> `in <reg> = <expr>` / `out <reg> = <lvalue>` / `clobber <reg|mem|cc>` block, so a
> Lullaby value marshals into/out of an architectural register around the verbatim
> bytes. Two soundness invariants, both pinned with inject-the-bug teeth: the
> **register-promotion exclusion** (a function containing `asm` is never promoted, so
> a bound local stays frame-resident — reviewed against a promoting-caller attack) and
> **callee-saved clobber save/restore** (Win64∪SysV union preserved around the block,
> via `rbp`-relative reserved slots so a `call` in an input expr stays 16-aligned).
> Native-only (`L0425` on interpreters), `unsafe`-gated; new `L0443` (operand shape) /
> `L0461` (width). 64-bit operands this release (sub-width honestly refused, not
> half-extended). **A Linux `write`+`exit` syscall driven entirely through operand
> `asm` runs under Docker** (`stdout="H"`, exit 42), reviewed PASS.
>
> **The kernel-capability tripod is COMPLETE**: **output** (direct-ELF, linker-free),
> **memory** (the safe-tier arena, runtime-free reclamation), and **control** (inline
> `asm` operands — syscalls/control-registers/MSRs over program values), on top of the
> already-shipped freestanding raw pointers, MMIO, port I/O, and static-buffer arenas.
> Lullaby can now express a kernel. Remaining kernel niceties (not blockers):
> interrupt/naked function attributes for the IDT, and a mnemonic-template `asm` (needs
> an encoder — deferred, a clean superset of the shipped byte form).

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
- **Const-sized arrays: SHIPPED (frontend + all tiers via erasure).** `array<T, N>`
  where `N` folds to a positive constant (a literal or a named `const`), plus a
  `[value; count]` fill literal. A dedicated pass (`semantics_array_extent.rs`)
  runs after const-folding: it validates every extent and construction count, then
  **erases** `array<T, N>`→`array<T>` and **expands** each fill to an ordinary array
  literal, so the checker and every backend see only `array<T>` — a fixed-extent
  program therefore compiles/runs exactly where the same-length `array<T>` already
  does, with no new codegen and no miscompile surface (native-eligible shapes
  native-compile; the rest skip cleanly with `L0339`, identically to plain arrays).
  New diagnostics `L0463` (non-const extent/count), `L0464` (zero/negative),
  `L0465` (construction count ≠ extent). The `[value; count]` form uses a lexer
  bracket-depth rule (`;` is a symbol only inside `[]`; statement-level `;` stays
  `L0103`), reviewed immune to the in-string bracket-counter bug. The genuinely-new
  **inline, no-heap, by-value native storage** for fixed-extent arrays is the
  queued follow-up increment (a representation change, not a new miscompile
  surface). See `semantics_array_extent.rs`, `lullaby_type_system.md`, `suite23`.
- **Native inline fixed-array struct fields: SHIPPED (increment 2).** A struct field
  typed `array<T, N>` now gets a distinct **inline, by-value** native representation
  (closing the pre-existing "array in a struct skips natively" gap; a fixed `N` is
  what makes a static inline layout possible). Mechanism (b): the extent pass still
  erases globally, but captures each extent-bearing struct field into a
  `FieldExtents` side-table (`SemanticInfo::field_extents` → `IrStructDef::
  field_extents`, serde-defaulted so `.lbc`/COFF snapshots stay byte-identical) that
  **only** native's `resolve_struct_fields` reads — a field is the one case needing
  it (no initializer to infer a length from). The existing aggregate ABI (word-copy,
  hidden-result-pointer return) then carries it. Correct-or-refuse: scalar-only
  element fields lower inline; dynamic `array<T>` / `array<string, N>` /
  `for x in f.field` / whole-field binding skip cleanly (`L0339`). Reviewed PASS
  adversarially (by-value copy isolation, param mutation, large hidden-pointer
  return, narrow off-by-one, nesting — all native == interpreters; the
  `array<string, N>` skip verified not to pointer-share). Interpreters/WASM
  byte-for-byte unchanged. See `native_backend_contract.md`, `suite24`.
- **Native whole-field ops: SHIPPED (increment 4).** `for x in f.field` and `let c =
  f.field` over an inline fixed-extent struct field now lower natively — both reduce
  to one by-value aggregate copy of a `Field`/`Index` place plus planning-time length
  inference (`inline_array_type_of_path`), since `for-in` desugars to a hidden
  `let __foreach_coll = f.field`. Works for `i64`/`f64`/narrow-packed/nested; still
  skips (`L0339`, sound demotion) for `array<string, N>` fields, runtime-indexed
  sources, and aggregate-element `for-in`. Reviewed PASS adversarially (aliasing
  isolation, pre/post-field integrity, length inference, narrow sign-extension).
  **A2 native array operations are now complete**: construct, copy, pass, return,
  `[i]`, `len`, `addr_of`, iterate, whole-field bind — all inline and by-value.
- **WASM:** fixed-extent array struct fields are already at observable parity via
  WASM's pointer-per-field model (no inline mechanism needed — measured). A
  **WASM value-copy miscompile class is now closed** (`9a8f046`): `let`/assign/
  field-store and struct/enum/array **construction** all deep-copy an lvalue
  aggregate operand (keyed on one rule — copy iff the stored source is an lvalue —
  at every store site; a freshly-constructed whole record still binds directly, but
  its operands are copied). Found via a **new `wasmi` execution-parity harness**
  (owner-approved, dev-dependency, pure-Rust in-process) that runs the emitted module
  and asserts WASM == the IR interpreter — WASM was previously verified structurally
  only (emitted bytes), never executed. The harness caught the original bug and, on
  review, three surviving construction sites (the class is closed under an independent
  31-site store enumeration). Full const-fn evaluation stays post-1.0.
- **WASM execution testing: NEW GATE.** `crates/lullaby_ir/src/wasm_exec_tests.rs`
  (via `wasmi`) is now a standing execution-parity gate — the WASM analogue of the
  native compile-and-run tests. WASM correctness is gated, not just structural.

### A3. FFI completeness — **DECIDED: callbacks in 1.0**
Base FFI ships; deferred today (L0424): callbacks (fn pointers), struct-by-value,
and `string`/`list`/`map` marshalling.
- **Decision: callbacks (fn pointers across the C ABI) in 1.0.** Deep
  struct/collection marshalling stays post-1.0.
- **Fn-pointer PARAMETERS: SHIPPED (`66971f5`).** A Lullaby function passes to C as a
  C-ABI function pointer: `extern fn apply_cmp cmp fn(i64, i64) -> i64 a i64 b i64
  -> i64`, and a C driver calls the passed callback back under the Win64 ABI
  (`ffi_callback_roundtrip_when_compilable`, `ffi_callbacks.rs`). The interpreters
  reject the extern call (`L0423`, no C to call) as with all FFI.
- **Next (remaining A3):** fn-pointer **returns/storage** (a C function that returns a
  fn pointer, or storing one) — the narrower remaining case. Deep struct/collection
  marshalling stays post-1.0.

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

### B1. Closures native codegen — **stages 1–3a SHIPPED**
- **Stage 2 (scalar completeness): SHIPPED** — direct non-escaping closure calls,
  int + float captures/params/returns, any param count.
- **Stage 3a (non-escaping higher-order functions): SHIPPED** (`native for-x`… merge
  after `7b3251d`). A closure passed as a **call-only** argument (`g(f, …)` where `g`
  only ever *calls* its fn-param — never stores/returns/reassigns/captures/reads-as-
  value/passes-onward) lowers to a real indirect call through the existing closure
  ABI; the closure's caller-owned capture block stays valid for the call's extent.
  Anything not provably non-escaping skips cleanly (`L0339`). Reviewed PASS against 13
  escape-construction probes + the positional-XMM/spill ABI matrix; FFI callbacks
  (shared indirect-call ABI) unaffected; AArch64/WASM still skip HOF cleanly. x86-64
  native only.
- **Stage 3b (returned/escaping closures): SHIPPED** (`15b67a7`, via arena stage-4a).
  A factory returning a scalar-capture closure literal, and a caller **invoking** a
  call-returned closure (`let g = make_adder(5); g(3)`), now compile natively. The
  factory stays **off-arena** (criterion 1b refuses a `fn` return → no reclamation →
  the returned block stays live → no dangling), so it is sound with **no promotion** —
  reviewed PASS against a dangling hunt (the off-arena guard is load-bearing: disabling
  it segfaults) and an aliasing hunt (a returned `fn` **parameter** stays refused,
  defended in depth). Only a fresh flat scalar-capture literal return lowers; a returned
  param / heap capture / stored / re-returned closure skips cleanly (`L0339`).
- **Still deferred (stage 3c+):** heap/aggregate captures, mutable-capture rebind,
  onward-passed (multi-level) HOF chains, and stored (not returned) closures. (**Arena
  reclamation of the factory's scratch — stage-4b — has since SHIPPED** at `c356b2f`
  via mark-advance promotion of the returned block into the caller's region; the arena
  model is complete.) Everything still deferred skips cleanly to the interpreters via
  `L0339`, so these are native-coverage gaps, not correctness or expressiveness gaps.
  Note D5's strict-RC policy bounds what is implementable: an **escaping** heap-capture
  closure that promotion cannot reach stays `L0339`-deferred by decision rather than
  being RC-managed — so stage 3c is not simply "unblocked", it needs a design pass to
  separate the soundly-implementable cases from the policy-bounded ones.

### B2. Concrete stdlib contents — **PLANNED**
The API-stability *posture* is decided (freeze a small core, version the rest) but
the *contents* are not enumerated. Define the 1.0 stdlib surface — strings,
collections, math, fs, io, time, os, (maybe net) — and mark each item **stable** vs
**extended/experimental**. Do this near the finish line, informed by dogfooding.

### B3. "Stable"-grade toolchain — **PARTIALLY SHIPPED**
Before stamping "stable": a built-in **test runner** (SHIPPED), **debug info on
Linux/macOS** (DWARF source lines — SHIPPED; CodeView was Windows-only), and
**LSP + package-manager maturity** (still planned).
These are toolchain-completeness items, not language decisions.

- **Test runner — SHIPPED; both containment gaps now CLOSED.**
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
  `crates/lullaby_cli/tests/cli/suite17.rs` (9 tests, the runner's surface) and
  `crates/lullaby_cli/tests/cli/suite19.rs` (6 tests, the isolation mechanism)
  over `tests/fixtures/test_runner/`.
  - **Isolation holds for every runtime error** — `assert(false)`, `throw`, A5
    contract violations (bounds fail, divide-by-zero), `L0423` — each is reported
    as a `FAIL` and the run continues. The mechanism is the execution tier, not
    A5: A5's abort-without-unwinding governs the *native* tier, while the runner
    runs on the AST interpreter, which returns these as ordinary `RuntimeError`s.
  - **The two gaps — CLOSED (subprocess isolation + per-test timeout).** Both
    shapes that escape the interpreter's `Result` are now contained, so no test
    can take the runner down. The suite runs in a **child process** under a
    **per-test deadline** (`--timeout <seconds>`, default 60s; `0` disables):
    a **stack overflow** kills the child, which the parent reports as
    `FAIL ... terminated abnormally` before resuming at the next test; a
    **non-terminating** test trips the deadline, is killed, and is reported as
    `FAIL ... timed out after Ns`. In both cases the remaining tests still run and
    the summary is still correct with a non-zero exit. Exit codes stay
    unpinned — a stack overflow is `0xC00000FD` on Windows and 127/a signal on
    POSIX, so only the *observable* (reported as a failure, run continues) is
    contract. Pinned by `suite19.rs` over `tests/fixtures/test_runner/`
    (`stack_overflow.lby`, `infinite_loop.lby`); the hang pin passes an explicit
    short `--timeout`, so a non-terminating test cannot stall CI.
  - **Tree-kill, not child-kill (review finding).** The first cut killed only the
    child and joined the stderr reader. But `sys_status`/`sys_output`/`proc_spawn`
    let a `test_*` function spawn real processes, and a grandchild outlives a
    killed child *and* inherits its stderr pipe handle — so EOF never arrived and
    the runner waited out the grandchild: `--timeout 3` measured at **14s**,
    scaling linearly with the grandchild (`ping -n 60` -> 60s). The deadline
    bounded nothing, which is the exact gap B3 exists to close, reachable from the
    documented surface. Fixed by killing the process **tree** (`taskkill /T` on
    Windows; a process-group signal on POSIX, the child being made group leader),
    by *not* joining the reader (so the deadline holds even if a kill fails), and
    by having the child signal completion explicitly instead of the parent
    inferring it from EOF (which a grandchild can defer, stalling even a wholly
    passing suite). Pinned by a wall-clock bound in `suite19.rs` — the `FAIL` line
    alone printed on schedule while the run was unbounded, so only elapsed time
    catches it.
  - **Unforgeable reporting (review finding, three rounds).** Isolating the
    *process* is not enough if the *report* can be faked, and this took three
    attempts. Every escape came through the **OS**, not the language surface —
    and none was findable by running the suite: `cargo test --all` was green,
    444 tests, while the hole was open. Only asking "what if the program under
    test is hostile?" found them.
    1. The protocol rode on the child's **stderr** with a per-run nonce in
       `argv`. `warn()` writes to that same stderr, and a process may read its
       own command line (`/proc/self/cmdline`; `Get-CimInstance Win32_Process`
       via `sys_output`), so the nonce was never secret. A forged `done`
       truncated the run to a green `0 passed, 0 failed` + exit 0 — "passes
       having run nothing", a mode this project has shipped once before.
    2. Moving it to a **private pipe in the stdin slot** was defeated too:
       `proc_spawn` spawns with stdin **inherited**, so a grandchild received a
       writable handle and `cmd /c echo pass 1 >&0` forged a line. The audit
       that missed this verified no builtin *names* a descriptor (true) and
       concluded the channel was unreachable — but **process inheritance hands
       the descriptor over with no builtin involved**.
    3. **Fixed:** before any test code runs, the child takes the descriptor out
       of the stdin slot, duplicates it onto one that cannot be inherited
       (no-inherit `DuplicateHandle`; `fcntl(F_DUPFD_CLOEXEC, 3)`), and puts the
       null device in the slot — `SetStdHandle(STD_INPUT_HANDLE, NUL)` /
       `dup2(nul, 0)`, which disposes of the original too. The channel then has
       no name a program can reach and no slot a child can inherit — holding
       against spawn routes nobody has thought of, rather than depending on every
       spawning builtin to null its own stdin (which would also be outside the
       CLI's scope). POSIX needs `dup2` rather than close-then-reopen: `File::open`
       always sets `O_CLOEXEC` and `Command` does no `dup2` for an inherited
       stdin, so a reopened fd 0 reaches the grandchild *closed* — safe but a
       footgun (its next `open()` lands on descriptor 0). Bonus: `read_line`
       regains a clean EOF.
    Validating `done` against `last_reported` is an integrity check, NOT a
    security control — the forger reported every index first, satisfying it, so
    `done` then completed the batch legitimately. Only unreachability works.
    Pinned in `suite19.rs` by an **end-to-end** grandchild forgery (the only kind
    that would have caught any of the three), a direct `println`/`warn` forgery,
    and a structural check that no verb reaches stdout/stderr — the last is
    necessary but not sufficient, and believing otherwise is what let round 2
    through. Out of scope, stated: a test can `proc_spawn` an arbitrary **native**
    binary, which could steal a handle out of the process — but that is arbitrary
    code execution as the user, against which no in-process boundary helps.
  - **Design: batch-with-resume, not process-per-test.** A process per test would
    pay a spawn (~13ms) plus a full recompile per test — ~1.6s on a 100-test
    suite against ~20ms in-process, a tax on the all-passing path where nothing
    needs isolating. Instead the child runs the whole remaining suite sequentially
    in one process (preserving the old in-process semantics exactly), and the
    parent respawns only when a test actually kills it, resuming at the next
    index. Measured overhead on the all-passing path is **one** extra spawn +
    compile regardless of test count: **+12ms on 4 tests, +22ms on 100**.
  - **Still open here:** `--backend native|ir|bytecode` for `test` (the runner is
    AST-only) would need new named-function entry points — `lullaby_ir` exposes
    only `run_main` today. Not a containment gap; a backend-coverage one.
- **Open sub-item — declaration surface (owner call).** The `test_*` convention is
  implicit magic: a name prefix silently confers semantics, which sits awkwardly
  with Lullaby's explicitness. A `test "name"` block (Zig-style) would be explicit,
  terse, natural in an indentation-only language, and would give the runner a real
  name string. It is **not** scheduled: the convention is specified in
  `language_surface.md` and ships in released binaries, so changing it is a
  breaking surface change and an owner decision.
- **DWARF debug info — SHIPPED (source lines).** `--debug`/`-g` now emits DWARF
  `.debug_line`/`.debug_info`/`.debug_abbrev` on the ELF and Mach-O targets, at
  the same per-function granularity CodeView already gave COFF. Verified by
  decoding the output with `gimli` (an independent DWARF reader) on both formats,
  and once end-to-end through `rust-lld` → linked binary → `gimli`. Variables,
  types, and frame/CFI info remain deferred.
- **LSP/package-manager maturity — still PLANNED.**

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
| B3 | Stable-grade toolchain | **PARTIAL** | test runner SHIPPED (+`--filter`, suite17); isolation now COMPLETE — subprocess + process-tree kill + per-test `--timeout` (default 60s) contain stack-overflow, non-terminating AND grandchild-spawning tests, pinned by suite19 (incl. a wall-clock bound), +12ms/+22ms overhead on the all-passing path; results reported on a private pipe whose stdin slot is reopened onto the null device, so it has no name a program can reach and no slot a child can inherit (a nonce in argv was not secret; a reclaimed-but-not-reopened slot was inherited by `proc_spawn` grandchildren); remaining uncontained: machine-wide resource exhaustion (reachable), a descendant that leaves the tree, `spawn`-thread blame misattribution, crash-vs-timeout attribution under a sub-teardown deadline. **DWARF source lines SHIPPED** (ELF+Mach-O, `--debug`/`-g`, opt-in and byte-identical without it; decoded independently by `gimli` and by GNU readelf, and linked+decoded end-to-end; suite20 — variables/types/CFI deferred, and CFI is the biggest remaining gap for real debugging). `test --backend` (AST-only) and LSP/pkg maturity remain; a live gdb/lldb session awaits a Linux job (this repo has no CI — the POSIX/ELF paths are verified by inspection, cross-lint and independent decoders, never executed). `test` declaration surface (`test_*` vs `test "name"` block) is an open owner call | 2026-07-17 |

## Owner decisions — 2026-07-16

| # | Item | Decision |
|---|------|----------|
| C1 | **Actor failure model (stage 4)** | **Result-based, A5 unchanged.** A fallible handler returns `result<R, E>`; a supervisor observes `err` and applies a restart policy. A genuine panic (bounds fail, unwrap-on-`none`, div-by-zero) still **aborts the program** — you do not restart an actor that hit a bug. Erlang-style crash-supervision would require unwinding the language deliberately lacks, copying Erlang's surface without its substrate (isolated cheap processes, no shared state). Pinned by a test so the boundary cannot be softened into a comment. Revisiting this means revisiting **A5 itself**, deliberately. |
| C2 | **`L0401` overload** | **The concurrency meaning keeps `L0401`** (~31 live emission sites); "call target not found" (~3 sites) takes a new code. Follows the `L0210` precedent: the number stays with the live, most-emitted meaning so the fewest references break. |
| C3 | **Accepted limitations** | **All funded — no "we'll live with it".** Owner: *"no reason to leave it out… go for making things properly and in the best versions they can possibly be."* See below. |

### C3 — limitations previously accepted, now funded work
These are all **loud refusals, never miscompiles** — but each was a deferral, and 1.0-stable should mean they are decided, not drifted past. #1 and #2 are now both **shipped**; #3 and #4 remain. (These were planned to serialize on the interpreters as a shared bottleneck. #2 did rework them, via the env shelf; #1 in the end needed **no** interpreter change at all — the interpreters already strode narrow elements correctly, and that is precisely what forced native's packing.)

1. **Narrow-element array walking** (`array<i32>`/`array<u8>`) — **SHIPPED** (`feat/native-narrow-arrays`). Previously refused natively: the element cell was 8 bytes while `ptr_offset` strided by `size_of(T)`, so a walk desynchronized. Refusing was correct; the fix landed as **packed narrow array elements** — an `array<T>` for a narrow integer `T` (`i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`byte`) now stores its elements at their **C width** (`NativeType::Narrow`, produced only by `narrow_array_element`), so `addr_of(a[0])` + `ptr_offset` strides `size_of(T)` and lands on the next element. The interpreters needed no change — their `addr_of` region stride was already `Value::layout_size` of the element, which is *why* packing was forced rather than chosen: an 8-byte-cell native layout gave a defined program two answers. Scope is tight and is the safety argument: only the array-ELEMENT position packs (a narrow *scalar* is still a normalized 8-byte cell, and `addr_of` of one is still refused — `addr_of_narrow_scalar.lby` → `L0339`); the array still occupies whole words and stays word-aligned; and auto-vectorization disqualifies narrow arrays by construction (all four SIMD bases refuse a `Narrow` element). `bool`/`char`/`f32` are deliberately NOT packed — `addr_of` of them is refused before a stride is observable. Pinned at four-tier agreement by `tests/cli/suite22.rs` (the `i32` walk → `18`, the `u8` byte-buffer walk → `101`, write-through-read-back-by-index → `42`, `i16`/`i8`/`u16` + whole-array decay → `83`, a `--freestanding` byte-buffer walk → `120`) plus the dual-oracle fuzzer `tests/cli/fuzz_narrow.rs`. **Kernel-critical** — byte buffers are pervasive in drivers — and the driver idiom now compiles in a `no-runtime` module.
2. **Cross-frame `addr_of` on the interpreters** (`L0459`) — **SHIPPED** (`feat/interp-env-shelf`). Out-parameters and buffer-passing (*valid C*, per C11 6.2.4p6: a call does not end the caller's block) now resolve on all three interpreters, matching native's real `lea`. The fix is an **env shelf**: an interpreter-owned stack of the *ancestor* frames' `Env`s, swapped in at the call boundary and looked up by the `RootSlot::env` id an `addr_of` already recorded. Keeping the *current* frame a plain `&mut Env` and shelving only ancestors is what avoided the hot-path tax that the sketched `Vec<Env>`-indexed-by-frame-id design (a bounds check per variable access) and `Rc<RefCell<Env>>` (a borrow check per access) would both have imposed — measured within the ±2% noise floor on fib and the loop across all three tiers. `L0459` now covers **genuinely dangling pointers only** (a returned `addr_of(local)`; a pointer to an inner-block or loop-body local used after that block — all UB in C), plus the one residue of a pointer handed to another *thread*. Pinned at 327 on all four tiers (`freestanding_cross_frame.lby`) and 432 on the three interpreters (`raw_ptr_cross_frame.lby`).
3. **Void `export fn`** — `is_exportable_scalar` admits only `i64`/`f64`/`f32`, so a C-callable `void NAME(...)` is `L0424`. Small frontend fix.
4. **`alloc` is not in the native subset** — the one pointer form that works across frames on every tier demotes the whole program.

## Owner decisions — 2026-07-18 (safe-tier arena memory model)

Made while starting the arena track. A read-only map (`native_object_eligibility.rs`
+ `native_object_stmt.rs` + `native_object_heapbox.rs`) established the state first:
**stages 1–2 are shipped native-only** (implicit function region rewinds on return;
confined loops get per-iteration sub-regions; the interpreters never reclaim, so
native == interpreters is the safety oracle for a value-neutral bump-heap). The
92-vs-2116 sub-region-rewind miscompile class is **guarded** by `alloc_defeats_arena`
(any `alloc` in a function → arena-ineligible; conservative, load-bearing — it also
contains the `ptr<ptr_i64>`/`int_to_ptr` laundering routes). The arena currently fires
**only for leaf functions** (calls no user code).

| # | Item | Decision |
|---|------|----------|
| D1 | **Arena track is the next major direction** | Chosen over inline-`asm` operands and the narrower completions. Proceed via tightly-scoped, adversarially-reviewed increments; **serialize the arena *code* changes** (each lands into a fully-reviewed tree) even where files wouldn't collide — it's the subsystem with a demonstrated use-after-free class. |
| I1 | **Explicit `region` block: frontend SHIPPED** (`c0ce091`) | Bare `region` + indentation block (reclaim-at-dedent syntax), disambiguated from `region NAME in BUFFER` / `region NAME:` by the absent name. A **real scoped IR node** (`IrStmt::RegionBlock`/`BytecodeInstruction::RegionBlock`, threaded through ~35 sites) — NOT flattened like `unsafe` (the first attempt flattened and reintroduced the just-closed shadowing slot-alias UAF for region scopes; caught in review). Block bindings are lexically scoped (dead after dedent, `L0306`), so the common scratch case is sound by scoping alone. Runs **value-neutrally on all four tiers + WASM** (no reclamation yet — native==interpreters trivially). Reviewed PASS twice (blocker found + fixed). **Native sub-region reclamation deferred** to a follow-up (needs the confinement escape-handling for the assign-to-outer channel and, for the server-loop case, I2's cross-call retention). |
| D2 | **First increment: I4 — target-aware loop confinement** | **SHIPPED.** `loop_body_confines_heap` (now `native_object_confine.rs`) admits a heap store iff its target is a top-level loop-body `Let` (empty path), the body is widenable (no closure/`ptr`/`asm`), and — for nested loops — the target is a `Let` of an enclosing confined loop. Sound via two frontend-guaranteed facts: a top-level loop `let` re-inits before use (not loop-carried) and is lexically dead after the loop (not live-out). Reviewed PASS — the reviewer tried to construct a use-after-free in a correct program and could not. **Surfaced a pre-existing, arena-independent native miscompile**: a loop-body `let` shadowing a live outer `let` of the same name aliases one stack slot (native ≠ interpreters *on main*), which under the arena is a UAF; I4 broadens its trigger to rebind loops but affects no *correct* program (shadowing is already miscompiled). **Fixed separately** (`native_object_rename.rs`, `0ff806a`): an alpha-renaming pass runs before frame planning and renames any binding that shadows a strictly-enclosing scope to a source-illegal `v#sN`, so the flat-map slot planner cannot alias them — reviewed PASS against an exhaustive under-rename hunt across every scope kind; the UAF is closed on all four tiers. |
| D3 | **Cross-call arena (I2): inferred per-function retention summary — SHIPPED (`dc05187`)** | Widen past the leaf limit via a bottom-up, locally-computed "does-not-retain-a-heap-pointer-past-return" bit, **default-deny on recursion/cycles/indirect calls**. No annotations, no whole-program pass — preserves the compile-speed moat. Direction set; build sequenced behind I1. **Design complete** (read-only pass): `non_retaining(f)` = **R1** scalar return ∧ **R2** no closure/spawn/await ∧ **R3** no `ptr`/`asm`/`alloc` ∧ **R4** every callee is a builtin or an already-non-retaining module fn (extern/indirect/recursive/SCC ⇒ retaining). Computed in **one reverse-topological sweep**, cycles pre-poisoned to `retaining` — no fixpoint. Caller-side: revise criterion 3 from `body_calls_user` to `all_callees_non_retaining` (the walk must **invert** the default — unknown/indirect callee ⇒ DENY). Lullaby's own design closes most channels for free: **no mutable globals** (stash-in-global not expressible), **value semantics** (no write-through a param). Slice 1 = the R1–R4 above (defer heap-returning-non-retaining callees, non-escaping closures, recursion relaxation). **Prerequisite the design surfaced: the `alloc_mode` flag is not stack-disciplined** — an arena caller invoking an arena callee has the callee's reset clear the flag mid-caller-body (invisible until cross-call arena exists). Must land in the same increment: save/restore `alloc_mode` in `emit_arena_prologue`/`emit_arena_reset` instead of hard 1/0. New module `native_object_retain.rs`; one small criterion-3 swap in `eligibility.rs`. **Shipped as specified** — `non_retaining` = R1–R4, single memoized DFS with cycles pre-poisoned, the callee walk inverts the default (indirect/extern ⇒ deny). The **`alloc_mode` prerequisite** landed with it (prologue saves/reset restores the prior mode — without it a nested arena call crashes `0xC0000005`, proven by injection). Reviewed PASS against a cross-call-UAF construction hunt (every channel blocked; the closure-literal refinement verified sound — a function with a closure literal is itself R2-retaining, so no caller arenas over it). Widening surfaced + fixed **two latent bugs unreachable pre-I2** (a `match`-scratch-over-a-call under-reservation segfault; a per-iteration closure reclaim). Deferred frontier: fresh-heap-returning callees (relax R1), non-escaping closures (R2), recursion (keep default-deny). **The arena now reclaims across real call graphs, not just leaves.** |
| D4 | **Escape→promotion (stage 4): Option A — mark-advance compaction — SHIPPED (`c356b2f`)** | Promote an escaping value by relocating it to the region mark and advancing the mark past it, so the single-cursor rewind preserves it. Edits only the two rewind choke points (`emit_arena_reset`, `emit_arena_loop_rewind`) + the two eligibility predicates. **Phased**: flat return-edge (closure scalar-capture blocks — a pure `memmove` — and scalar aggregates) → `string` → deep aggregates + loop-edge accumulation. Chosen over a two-cursor arena (heavier runtime surface) and an RC-fallback (see D5). Copy-soundness boundary: scalars/`string`/flat blocks/heap-carrying aggregates promotable by **deep** copy; `alloc`-boxes/`ref`/`shared`/raw-pointer values never auto-promoted. |
| D5 | **RC policy: strict — no compiler-synthesized silent RC** | The decided "`ref`/RC is never silently inserted" rule is **literal**. Native either **arena-promotes** (Option A) a provably-escaping value or **defers to the interpreter** (`L0339`, always correct) — it does **not** synthesize a hidden refcounted path with automatic drop glue to unblock escaping closures/above-cap aggregates. Escaping closures are therefore unblocked only where Option A promotion covers them (a flat block born in an arena region); a returned closure a promotion can't reach stays `L0339`-deferred rather than RC-managed. More conservative, keeps the model's promise exact. |

**Stage-4 design refreshed against shipped I2 (2026-07-19).** Re-validating Option A phase 1 against the I2 code found three shifts: (1) a **third** eligibility predicate now exists — the retention summary — whose R1 marks any heap/`fn` return retaining, so it needs a carve-out; (2) `emit_arena_reset` now restores the saved `alloc_mode` (I2 co-fix), which the promoting reset must preserve; (3) a **non-memory prerequisite the original design missed** — a closure obtained as a *return value* isn't invocable (`indirect_callable_sig` covers only literals + `fn` params), so promotion is untestable end-to-end until returned closures can be called. **Crux resolution:** a promoting function is `non_retaining` (same bit, no third state) because its returned flat block lands in the caller's region (`markF ≥ markC`) and the caller's own R1–R4 eligibility forbids leaking it past the caller's rewind; the R1 carve-out must be **purely local** (a fn of F's body only) to avoid a summary→eligibility→summary cycle the single-sweep DFS can't express. **Scope sharpened:** the closure scalar-capture block is the ONLY genuinely flat, self-contained heap block in the language (a scalar struct returns via sret with no heap block; a heap-carrying aggregate needs pointer relocation, violating the pure-memmove boundary) — so it is the sole phase-1 target. Phase 1 is therefore **two increments**: (a) returned-closure **invoke plumbing** (call a call-returned closure — non-memory, a prerequisite), then (b) the **mark-advance promotion** (`emit_arena_reset` gains a per-site survivor size; copy survivor to mark, `heap_next = mark+size`, `rax = mark`, preserve the mode restore; four eligibility edits gated on a shared `returns_promotable_closure` predicate). No new frame word needed. Open sub-decisions resolved with defaults: closure-only phase 1; size cap ~8 flat words (`L0339` above); string/deep/loop-edge deferred.

**Stage-4b SHIPPED (`c356b2f`) — the arena memory model is now COMPLETE.** The promoting `emit_arena_reset` copies the returned closure survivor to the mark, sets `heap_next = mark+size`, returns `rax = mark`, and preserves the I2 `alloc_mode` restore — so a closure-returning factory becomes arena-eligible and reclaims its per-call scratch while the returned closure is promoted into the caller's region. Classified `non_retaining` via a **purely-local** R1/R2 carve-out (never gated on arena status → no summary↔eligibility cycle). Reviewed PASS against a 15-program cross-call-UAF construction hunt (all held) + three injection teeth (plain reset → `0xC0000005`; wrong size → corruption; non-fresh survivor → refused at 5 independent layers). Bounded-heap proof: a 20 000-iteration factory reclaims its scratch (would otherwise exhaust the heap). A self-caught edge — an unseeded `heap_next=0` making the first-allocation copy a null write — is fixed by an idempotent prologue seed. Deferred beyond 1.0-relevance: string/deep-aggregate/loop-edge promotion, recursion relaxation. **The safe-tier arena-first memory model — implicit function + loop regions, cross-call reclamation, explicit `region` blocks, and escape promotion — is fully realized.** (Follow-up: `native_object_eligibility.rs` is 6 lines over the ~1500 cap — a behavior-preserving split, non-blocking.)

## Delivery progress (updated 2026-07-20)

- **Phase 1 hardening underway — the interpreter oracle is now deep-recursion-valid**
  (`9268169`+`ab7c272`). Sweep #1 came up clean on the codegen surface; its one
  oracle-integrity finding is fixed: all three interpreters evaluate on a 2 GiB-stack
  thread under a uniform `INTERPRETER_RECURSION_LIMIT = 20000` → a clean **non-catchable**
  `L0466` (fault-class, not `try`/`catch`-recoverable). The differential fuzzers were
  blind past ~200 frames; `fuzz_recursion.rs` now generates deep recursion (tiers agree
  at 9000 frames) so the blind spot cannot reopen. Bonus O(depth²)→O(depth) traceback
  fix. P0.1 `native_object_eligibility.rs` split landed (`6455bc3`). Live log:
  `finish_line_plan.md`.
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
- **Actors — stages 1–4 SHIPPED** (AST interpreter tier; IR/bytecode/native/WASM keep
  clean deferral). Stage 2 = `ask`/`await`/`Future<R>` request-response with a
  deterministic run-to-completion scheduler (deadlock → L0356). Stage 3 = message
  ownership: move-by-default + use-after-send (L0357), type-driven copy set, `shared<T>`,
  and a fully transitive sendability predicate (L0353 now recurses into struct
  fields/enum payloads). **Stage 4 = supervision** (`spawn X() supervise
  restart|stop|escalate`), per owner decision **C1**: failure is result-based and **A5
  stands** — `err(e)` is supervised, a genuine panic aborts. The boundary is structural
  (a `RuntimeError` propagates out of the turn before the policy is consulted) and pinned
  by a test verified load-bearing. Supervision is opt-in and `ask`-to-stopped is `L0359`
  rather than a fabricated `err` — both deviations from the design doc forced by C1 and
  confirmed in review. Restart storms are structurally impossible (the failing message is
  consumed, never replayed), so no backoff exists. Zero backend edits: the clause is a
  field on the existing `Spawn` node, so every tier deferral is inherited.
  **Stage 5 `join_all`/`select`: SHIPPED** (AST tier). `join_all EXPR` returns all
  results in input order (kind-preserving); `select EXPR` returns a compiler-provided
  generic `Selected<T> { index i64, value T }` — the winning slot + its reply, tie-break
  **lowest input index** (scan is input-order, not chronological), only the winner
  consumed, losers stay awaitable. Deterministic (byte-identical repeated runs);
  reviewed PASS. New `L0364`. Zero backend edits — combinators reject on IR/bytecode
  (`L0355`), defer on native/WASM (`L0339`/`L0338`), `no-runtime` `L0441`, like the
  rest of the actor model. **`select`/`join_all` are now reserved keywords.**
  **Stage 5 back-pressure DEFERRED** (documented in `concurrency_model_design.md`):
  block-until-space needs the `tell` hot path to pump the scheduler — a scheduler
  change, not a clean combinator increment.
  **Remaining**: stage-5 back-pressure, then stage-6 native/WASM actor codegen.
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
- **A3 FFI callbacks — fn-pointer PARAMETERS shipped** (`66971f5`; a Lullaby fn passes
  to C as a C-ABI fn pointer and is called back under the Win64 ABI). Fn-pointer
  returns/storage remain.
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
