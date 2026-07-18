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

## Failure semantics (decided — A5)

Both tiers share one failure model, split into two disjoint families:

- **Contract / memory-safety violations abort — they do not unwind.** An
  out-of-bounds index, an out-of-range slice, a `pop` of an empty list, a
  divide-by-zero, a zero loop step: each is a *bug*, and it terminates the
  program **deterministically** with a clear `L####` diagnostic. It is **not**
  catchable — a safety abort propagates straight through any `try`/`catch`. No
  stack unwinding, no allocation on the abort path, so the guarantee holds under
  the GC-free arena model and in freestanding code. There is no forced-`unwrap`
  that panics on `none`; `option`/`result` payloads are reached only through an
  exhaustive `match` or the recoverable `?`.
- **Modeled failures are recoverable values**, flowing through `result`/`option`,
  the postfix `?` operator, and `throw`/`try`/`catch`. They let the program keep
  running to a normal exit. Panics (aborts) are therefore reserved for bugs.

The three interpreters enforce this identically; **native** expresses the same
aborts as a hardware trap (`ud2` → `STATUS_ILLEGAL_INSTRUCTION` for bounds/heap,
`#DE` for divide-by-zero) because native code has no diagnostic printer. In the
**freestanding tier** the terminal action is routed to a **user-provided panic
handler** instead of an OS abort, so a kernel/embedded target chooses what a
contract failure does — the check machinery is identical, only the panic sink is
pluggable. The canonical statement, the full violation→diagnostic table, and the
recoverable surface live in `documents/lullaby_error_handling.md`
("Safe-Tier Failure Semantics").

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

## Region model — surface syntax + implementation contract (decided)

Owner delegated the choice; decision is the **hybrid model**: implicit
region management by default, explicit regions when you want control. This is the
memory-model mirror of the two-tier identity — safe/terse by default,
explicit/drop-to-metal when needed.

### Surface syntax

- **Implicit (default, no syntax).** Everyday code names no regions. The compiler
  gives each function an arena and each loop a per-iteration sub-region; heap
  allocations that provably don't escape their scope live there and are reclaimed
  by a bulk reset when the scope exits. LLM-friendly: the model writes normal
  code and arenas are invisible.
- **Explicit `region` block (indentation-only — NO braces).** Introduces a nested
  arena reclaimed at dedent. This is the arena-per-frame / arena-per-request
  pattern for servers and games:

  ```
  region
      scratch = build_report data
      print scratch
  # scratch's arena is bulk-freed here, at dedent
  ```

- **`ref` (RC)** — the opt-in secondary tool for values that escape to dynamic or
  shared ownership (see memory model above).
- **`unsafe` / freestanding** — raw pointers + manual memory for the hardware edge.

### Semantics

- **Implicit function region:** reset on *every* return/exit edge (reuses the
  proven multi-edge drop-insertion machinery from the RC work).
- **Implicit loop sub-region:** reset per iteration, so a hot loop inside a long
  function reclaims its per-iteration intermediates each pass. This is exactly the
  case per-object RC struggled with (collection grow/copy intermediates,
  per-iteration temps) — arenas reclaim it for free. A loop earns a sub-region only
  when its body **confines** its heap to the iteration (default-deny): no heap value
  is stored into a location that outlives the iteration. The confinement rule is
  **target-aware** (increment I4) — a whole-variable rebind of a **provably
  iteration-local** binding (a name introduced by a top-level `let` of the loop body,
  in a body free of closures / raw pointers / inline asm) does NOT escape and is
  reclaimed, while a store into a binding declared outside the loop (a loop-carried
  accumulator), a value read after the loop, a `return`/`throw`, a closure capture, a
  raw-pointer alias, or an element/field store into an outliving aggregate all stay
  denied. Soundness rests on two frontend facts: a top-level `let` re-initializes the
  binding before any use each iteration (never loop-carried) and is lexically scoped
  to the loop body (never read after the loop). See
  `documents/native_backend_contract.md` for the exact predicate and its verification.
- **Explicit `region` block:** a nested arena; allocations inside are freed at
  block exit.
- **Escape → promotion policy:** when a value outlives its region, the compiler
  **auto-copies** it into the enclosing region where value-semantically sound
  (small/sized values already copy under value semantics); genuinely
  shared/dynamic/cyclic ownership requires an explicit **`ref`** (RC is never
  silently inserted). **Default-deny:** if the analysis cannot prove a value stays
  local, it is treated as escaping and kept alive — never freed early. Correctness
  never depends on the escape analysis being generous.
- **Freestanding tier:** the same `region` surface, but arenas are backed by a
  **caller-provided static buffer** (no host allocator); `ref`/RC is unavailable.
  Most of a kernel can therefore stay arena-safe; only the raw hardware edge is
  `unsafe`.

### Implementation representation

- **AST:** an explicit `region` block node (a scoped block like `if`/`while`);
  implicit function/loop region markers are inserted during lowering, not written
  by the user.
- **IR:** region-enter / region-reset ops around the relevant scopes; an escape
  annotation on allocating expressions produced by a conservative, **local**
  escape pass (cheap — protects the fast-compile edge; never Tofte-Talpin
  inference).
- **Native:** shared `.text` helpers `__lullaby_arena_alloc` (aligned bump; grow a
  new chunk on overflow in the safe tier, or fail to the panic handler on a fixed
  freestanding buffer) and `__lullaby_arena_reset` (rewind the bump pointer to the
  region base, release extra chunks). Arena allocation and the existing RC /
  free-list path **coexist**: provably-local data uses the arena; escaping data
  keeps the RC path.

### Implementation staging (each increment production-complete, default-deny)

1. **Arena allocator + function-scoped implicit region for the non-escaping case**
   — the foundation. Value-neutral; verified by a bounded-heap reclaim fixture and
   the differential fuzzer.
2. **Loop sub-regions** — per-iteration reset.
3. **Explicit `region` block** — lexer/parser/AST/semantics + IR + native.
   **Frontend + value-neutral execution DELIVERED (increment I1):** the bare
   `region` + indented-block surface parses to a `RegionBlock` AST node, formats
   idempotently, and type-checks its body in a **lexically scoped** block — a
   binding declared inside is dead after dedent (referencing one afterward is the
   same `L0306` a post-loop read raises, so block-local values are sound with no
   escape analysis). It lowers **value-neutrally**: the body is inlined into the
   enclosing IR block (exactly like `unsafe`), so all four interpreter/native tiers
   run it and NO tier reclaims — native == interpreters trivially. **Deferred to a
   follow-up:** native bulk-reclamation of the block's sub-region (saving
   `__lullaby_heap_next` at entry, rewinding at dedent). Reclamation must reclaim
   only when the block provably **confines** its heap — nothing stored into a
   binding that outlives the block (the existing `native_object_confine.rs`
   predicate answers this) — and never a value that escapes. The two blockers that
   scoped it out of I1: a durable reclaim boundary requires a `BytecodeInstruction`
   node that forces exhaustive-match arms across the arena eligibility/confinement
   modules a parallel design pass owns, and the headline server-loop case
   (reclaiming across calls in a NON-leaf function) needs interprocedural retention
   analysis. The value-neutral fixtures in `suite26.rs` (esp. the escaping-store
   channel) pin the floor that reclamation must preserve.
4. **Escape/promotion** — auto-copy on escape; `ref` for shared/dynamic.
5. **Freestanding static-buffer arenas** — folds into the `no-runtime` tier work.

## Decided surface syntax (owner, 2026-07-14)

The surface-syntax forks from the concurrency and freestanding design proposals
are **decided** (each was the design's recommended option). Implementation must
build these forms; the "OWNER DECISION NEEDED" markers in
`concurrency_model_design.md` and `freestanding_tier_design.md` are now resolved
(a follow-up doc pass should annotate them as decided).

- **Actors:** dedicated `actor` block with a `state` section and `on` handlers;
  `spawn Name(args)` returns `Actor<T>`; sends are `tell` (fire-and-forget) and
  `ask` (request-reply → `Future<T>`, resolved by `await`). Avoids the reserved
  `!` error-throw clash; makes the thread boundary explicit.
- **Freestanding gating:** a **module-level `no-runtime` directive** is the
  semantic gate (kernel-mode visible in source; compiler rejects hidden
  allocation / hidden control flow / RC / actors with a hard diagnostic). The
  existing `--freestanding` flag stays as the orthogonal *output* contract.
- **Raw pointers / `unsafe`:** builtin functions (`ptr_read`/`ptr_write`/
  `addr_of`/`ptr_offset`/`ptr_cast<U>`/`ptr_null`) inside `unsafe` blocks,
  extending the shipped `ptr<T>`/`unsafe`/`volatile_*` set. No new operators
  (`*`/`&` are operators, `#` is the comment char).
- **Inline assembly:** an `asm "<text>"` template followed by an indented block
  of `in`/`out`/`clobber` operand clauses (indentation, not `: : :`); instruction
  text is forwarded to the assembler. The shipped raw-byte form is retained as
  `asm_bytes`.
