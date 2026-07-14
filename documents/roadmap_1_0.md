# Road to Lullaby 1.0

Canonical language rules: see [core_language_rules.md](core_language_rules.md). This is the
stable, repo-owned plan for reaching Lullaby 1.0. Granular tickets live in the ClickUp
`Lullaby` folder (lists 01–17); this document is the source of truth for scope and order.

## Definition of 1.0

Lullaby 1.0 is a **memory-safe-by-default systems language you can write apps,
services, AND a kernel in.** The identity, execution-tier structure, and memory
model are canonical in
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md); this roadmap
sequences the work to get there. It has two halves:

1. **Technically capable of expressing any program.** Not "has a framework for X", but ships
   the *spanning set of primitives* so that any program — OS, kernel, backend, server,
   webhook, websocket, game, desktop, mobile, web frontend — is technically buildable, however
   hard. Ergonomic, higher-level "bigger things" are specialized modules layered on **after**
   1.0. **A 1.0 that cannot express a kernel is not considered 1.0.**
2. **Easy to access.** Branded, bundled, documented, and installable with minimal effort across
   Windows, Linux, and macOS through every reasonable channel (winget, MSI/EXE, .deb/.rpm,
   Homebrew, one-line web installer, portable archives).

Everything is built to production quality — see the Production Quality Standard in
[../CLAUDE.md](../CLAUDE.md). No placeholder or "good enough for now" work is committed.

### Two execution tiers and arena-first memory

Kernel capability is delivered as **two tiers of one language** (canonical detail and
the 10-item kernel checklist:
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md)):

- **Safe tier (default)** — apps & services: **arena-first** (arena / region
  allocation is the primary, default memory model), bounds-checked, actor
  concurrency, with **reference counting as a secondary, opt-in tool** for
  escaping data. Assumes a minimal runtime.
- **Freestanding tier (`no-runtime` / kernel)** — kernels, boot, embedded, FFI: no
  CRT, no host allocator, **no RC**, no hidden allocation or control flow. Arenas
  still work, backed by a caller-provided static buffer; raw pointers + `unsafe`,
  inline assembly, volatile/MMIO, interrupt handlers, `repr(C)`/packed/alignment,
  and a user-provided panic handler are first-class.

The 1.0 scope must therefore satisfy the canonical **kernel-capability checklist**
(raw pointers + `unsafe`; static-buffer-backed arenas; inline asm; volatile/MMIO +
port I/O; control-register access; interrupt calling conventions; `repr(C)`/packed
+ alignment; pluggable panic handler; `no-runtime` mode; freestanding direct-ELF /
flat-binary output). These are sequenced through the phases below — see the
canonical doc for the checklist itself rather than restating it.

## The spanning primitive set

If these exist and are complete, every target category above is technically reachable:

- **Native code generation + linker** to standalone executables (x86-64 + ARM64; PE/ELF/Mach-O),
  including a **freestanding/no-runtime mode + inline assembly** for kernels/bare metal (the
  freestanding tier: no CRT, no host allocator, no RC, static-buffer-backed arenas, pluggable
  panic handler, direct-ELF / flat-binary output).
- **Arena-first memory model** — arena / region allocation as the primary, default model, with
  **reference counting as a secondary, opt-in tool** for escaping data and raw pointers as the
  `unsafe` escape hatch (no garbage collector). Canonical:
  [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md).
- **FFI / C ABI** — call any external code and be called. The universal escape hatch that
  reaches every OS API and existing library (graphics, audio, GUI, DB, crypto).
- **Full scalar set + bitwise ops** — `i8…u64`, `usize`/`isize`, `f32`; `& | ^ << >> ~`;
  conversions and defined overflow.
- **Raw-memory completeness** — arbitrary pointer casts, address-of, `sizeof`/`alignof`/
  `offsetof`, volatile (MMIO).
- **Atomics + memory ordering** and fences for lock-free/kernel concurrency.
- **A WASM target with linear memory + host imports** — the browser is a distinct machine that
  native codegen cannot reach; required for web frontends.
- **Platform I/O primitives** — time/clock, OS randomness, process spawn/exec, non-blocking I/O.

## Phases and order

Phases 1 and 2 run in parallel now; FFI (Phase 3) is the critical path to the "hard" categories
and gets the most design care; WASM (Phase 5) is independent; Phase 8 (ease of access) trails the
functional work; Phase 9 gates the release.

- **Phase 0 — Design** (front-loaded, parallel, zero-collision): FFI design, atomics design,
  WASM-heap design, distribution/branding design docs. → ClickUp lists 13/14/16/07.
- **Phase 1 — Numeric & operator primitives**: bitwise ops → wider integer types → `f32` →
  typed literals/suffixes → conversions + wrapping/checked/saturating → bit intrinsics.
  Also: capturing closures, `?` operator, generic user types. → list 10 / 03.
- **Phase 2 — Native backend completion**: full IR coverage, heap-type lowering, calls/traits,
  linker for PE/ELF/Mach-O, ARM64, freestanding + inline asm, native runtime. Includes the
  **arena-first memory model** (arenas primary/default; the already-in-flight RC substrate as
  the secondary tool; Perceus deferred) and the **freestanding / `no-runtime` / kernel tier**
  (static-buffer-backed arenas, pluggable panic handler, no CRT/host-allocator/RC) — staged in
  [memory_model_decision.md](memory_model_decision.md) against the canonical kernel checklist in
  [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md). → list 13 / 06.
- **Phase 3 — FFI / C ABI** (flagship): extern decls + calling convention, type marshalling,
  library linking, callbacks/exports/header-gen. → list 13.
- **Phase 4 — Concurrency & memory completeness**: atomics + orderings, fences + TLS, volatile,
  pointer casts + sizeof/alignof/offsetof. → list 14 / 04.
- **Phase 5 — WASM with a heap**: linear memory + allocator, heap-type + match/option/result
  lowering, host imports (console + DOM) + JS glue. → list 16.
- **Phase 6 — Platform I/O completeness**: time/clock, OS randomness, process exec/pipes,
  non-blocking I/O. → list 05.
- **Phase 7 — Stdlib honesty pass**: `string`↔`bytes`/UTF-8 primitives; establish the stdlib
  module layout and move batteries (HTTP, etc.) out of the core into modules. → list 11.
- **Phase 8 — Ease of access**: branding + toolchain bundle; Windows MSI/EXE + winget; Linux
  .deb/.rpm + distro-agnostic; Homebrew; one-line web installer; macOS tarball; release
  automation; project scaffolding + first-run UX. → list 07 / 17.
- **Phase 9 — 1.0 hardening & release**: conformance suite + cross-platform CI green; spec
  freeze + semver guarantees; documentation completeness + release notes. → list 08.

### macOS note

macOS is supported via tarball + Homebrew (no Apple Developer account required; users clear the
Gatekeeper quarantine). A signed/notarized `.pkg`/`.dmg` needs a paid Apple Developer account and
a Mac to sign — an optional gated follow-up, not a 1.0 blocker.

## Execution model

Work is driven in continuous waves of parallel agents with **disjoint file footprints** to avoid
conflicts; hot-file compiler features (semantics/runtime/IR) are sequenced one at a time while
design docs, examples, packaging, and the WASM/native files proceed in parallel. Every increment
runs the full gate (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --all`, markdown
refs) and lands production-complete.
