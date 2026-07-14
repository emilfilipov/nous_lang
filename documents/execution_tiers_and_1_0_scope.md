# Lullaby Execution Tiers, Memory Model, and 1.0 Scope

**Status:** Owner decision, 2026-07-14. This document is the current source of
truth for Lullaby's memory model and 1.0 identity. It **supersedes the RC-first
framing** in `memory_model_decision.md` (reference counting is retained but
demoted from the default model to a secondary tool) and the GC-hybrid pitch in
`lullaby_memory_management.md`. Those two files, plus `roadmap_1_0.md` and
`repository_map.md`, must be reconciled to this document in a follow-up doc pass.

## Identity (decided)

Lullaby 1.0 is a **memory-safe-by-default systems language you can write apps,
services, AND a kernel in.** It pairs C-class runtime speed with three things no
mainstream systems language offers together:

- **~8× faster compile** — direct object/executable writers, no external linker.
- **Memory safety without a borrow checker.**
- **Terse, LLM-friendly syntax** — and, more importantly, a model in which
  LLM-generated code actually runs (no manual-`free` footguns by default).

Above 1.0 is **spot optimization and convenience / standard-library modules
only.** Nothing after 1.0 changes the fundamental shape of the language. A 1.0
that could not express a kernel would not be a true 1.0.

## Two execution tiers

One syntax, one type system, presented in two tiers that differ only in what
runtime they assume. Conceptually this is Rust's `core` vs `std` + `no_std` +
`unsafe`, but with **arena-first safe defaults instead of borrow-checking**.

### Safe tier (default) — apps & services

- **Arena / region allocation is the primary, default memory model.**
- Memory-safe: bounds-checked indexing, no use-after-free, no dangling `ref`,
  data-race-free (actor concurrency).
- **Reference counting (`ref`/RC) is a SECONDARY, opt-in tool** for data whose
  lifetime is dynamic or shared across a graph, i.e. that outlives its region.
- Assumes a minimal runtime: allocator backing arenas, RC helpers, panic→abort,
  actor scheduler.

### Freestanding tier (`no-runtime` / kernel mode) — kernels, boot, embedded, FFI

- **No mandatory runtime:** no CRT, no host allocator, no RC, and critically
  **no hidden allocation and no hidden control flow** (the language must never
  secretly call an allocator or insert refcount ops — unacceptable in an
  interrupt handler or boot path).
- **Raw pointers + pointer arithmetic + `unsafe` blocks are first-class**, not an
  afterthought — early boot is entirely raw.
- **Arenas still work, backed by a caller-provided static buffer** — safe
  allocation discipline without a host allocator. Consequence: most of a kernel
  can stay arena-safe; only the hardware edge drops to raw. This is a *stronger*
  kernel story than C (raw everywhere) and gentler than Rust kernels (no
  borrow-checker fights).
- **Hardware access:** volatile / MMIO loads and stores, port I/O intrinsics,
  **inline assembly**, control-register and privileged-instruction access.
- **Interrupt-handler calling conventions** (or naked functions + inline asm).
- **Data-layout control:** `repr(C)` / packed structs, explicit alignment.
- **Bounds-check failure calls a user-provided panic handler** (cf. Rust's
  `#[panic_handler]`), not an OS abort.
- **Freestanding binary output:** custom entry symbol, no CRT startup, section
  control; direct-ELF and flat-binary emission.

The safe tier is *built on* primitives that also exist in the freestanding tier
(static-buffer arenas, value types, bounds checks with a pluggable panic hook).

## Memory model (decided)

- **PRIMARY — arena / region allocation.** Bump-allocate (faster than `malloc`),
  bulk region reset (faster than per-object `free`), zero per-object refcount
  traffic, and simpler to implement than RC. Keep the region model
  **dumb-simple** — explicit region scopes or a conservative "obviously-local"
  escape heuristic, **NOT** Tofte-Talpin/MLKit region *inference* (compile-time
  expensive; would threaten the ~7 ms fast-compile edge).
- **SECONDARY — reference counting (`ref`/RC).** Opt-in, for dynamic-lifetime or
  shared data that escapes its region. **Never present in the freestanding tier.**
- **ESCAPE HATCH — raw pointers + manual memory** inside `unsafe` / freestanding.
- **Perceus (RC-traffic elision + in-place reuse) is now LOWER priority.** It
  optimizes refcount traffic, but with arenas as the default most allocation
  never touches RC. Still worthwhile for the RC path; no longer the headline
  perf lever it was under the RC-first plan. No external dependency (in-house
  emitter analysis, as before).

Why arenas over raw pointers (the option considered and rejected): raw C-style
pointers are marginally simpler and faster than RC, but they throw away the
memory-safety pillar — the actual competitive moat and the reason LLM-generated
code runs. "Terser C with C's footguns" competes only with C, which already owns
that niche with 50 years of ecosystem. Arenas are *both* more efficient than
naive `malloc`/`free` (which the performance-serious C/C++/Zig/Odin crowd already
avoids in favor of arena allocators) *and* safe *and* simpler than RC. Raw
pointers remain available, but as an explicit `unsafe` escape hatch, not the
default that infects every program.

## Kernel-capability: the concrete 1.0 checklist

The freestanding tier turns "1.0" into this spanning capability set:

1. Raw pointer types + pointer arithmetic; `unsafe` blocks.
2. Static-buffer-backed arenas (allocation without a host allocator).
3. Inline assembly.
4. Volatile / MMIO load and store; port I/O intrinsics.
5. Control-register / privileged-instruction access.
6. Interrupt-handler calling conventions (or naked functions).
7. `repr(C)` / packed structs; explicit alignment control.
8. Pluggable panic handler for bounds/safety failures (`no-runtime`).
9. `no-runtime` mode: no CRT, custom entry symbol, no implicit startup/alloc.
10. Freestanding output: direct-ELF, flat binary, section/entry control.

## Sequencing

1. **Finish correct RC (in flight).** RC survives as a correct, fuzzer-covered
   *secondary* tool. Demoted from the default, not deleted — the work is not
   wasted.
2. **Arena-first primary allocation model** — the new default path.
3. **Freestanding / `unsafe` / kernel tier** — the big 1.0 leap (checklist above).
4. **Perceus** — deferred; RC-path optimization only.

Proceeding alongside, per the architecture-decision analysis: **actors** as the
concurrency model (keeps non-atomic RC + no borrow checker, data-race-free by
construction), and **direct-ELF for Linux** (extends the compile-speed moat to
where most systems developers are). Stdlib API-stability split (stable core +
versioned extended modules) is decided near the 1.0 finish line.

## Coupling notes

- **Actors + runtime-enforced safety** are the safe-tier identity. The
  freestanding tier drops actors and RC and exposes raw concurrency primitives
  under `unsafe`.
- The **native bounds-check gap** (static-length arrays indexed by a dynamic
  index) must be closed for the safe-tier guarantee to hold — and the check must
  be **parameterizable with a user panic hook** so the same machinery serves the
  freestanding tier.
- Single-threaded-per-actor heaps mean the queued arena + RC work needs **no
  rework** for concurrency: each actor owns its heap; messages are values.
