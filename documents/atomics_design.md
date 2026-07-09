# Atomics & Memory Ordering Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Builds on the concurrency runtime in [[concurrency_design.md]] (threads, channels,
`Mutex`) and the wider-integer-types work in [[roadmap_1_0.md]] (Phase 1). This is a
Phase 4 (ClickUp list **14 Concurrency**) design; it depends on Phase 1's scalar set
landing so that `atomic_i32`/`atomic_u64` etc. have real element types to wrap.

Lullaby's interpreters are real, multi-threaded Rust programs (see
[[concurrency_design.md]]: detached threads run over an `Arc<Program>`/`Arc<IrModule>`
share with **per-thread heaps**). That makes atomics implementable *for real* — the
runtime backs each Lullaby atomic with a `std::sync::atomic` cell in **shared** memory,
so cross-thread visibility and ordering behave exactly as the native backend must, not
as an approximation. This document specifies the atomic types, operations, orderings,
fences, thread-local storage, the backend semantics that keep all backends observationally
identical, and the diagnostics.

## Implementation status (delivered)

Memory orderings and fences are **delivered on `atomic_i64`** across the AST, IR,
and bytecode interpreters. The shipped surface refines the original proposal
below (which sketched a wider, per-width design); where they differ, this section
is authoritative for what exists today:

- **`MemoryOrder` enum, not `Ordering`.** Orderings are the five unit variants of
  a compiler-provided `MemoryOrder` enum (registered like `option`/`result`),
  spelled `relaxed`, `acquire`, `release`, `acq_rel`, `seq_cst` (note the
  underscored `acq_rel`/`seq_cst`, not `acqrel`/`seqcst`). They decode straight to
  `std::sync::atomic::Ordering::{Relaxed,Acquire,Release,AcqRel,SeqCst}`, so an
  `acquire` load runs a genuine `Ordering::Acquire` load — the argument selects
  the real std/hardware ordering, never seq_cst-for-everything.
- **Separate `*_ordered` variants, not an optional trailing argument.** The bare
  `atomic_load`/`atomic_store`/`atomic_swap`/`atomic_cas`/fetch-op builtins are
  unchanged and remain the `seq_cst`-default convenience forms. Each gains an
  ordering-taking sibling: `atomic_load_ordered(a, order)`,
  `atomic_store_ordered(a, v, order)`, `atomic_swap_ordered`/`atomic_add_ordered`/
  `atomic_sub_ordered`/`atomic_and_ordered`/`atomic_or_ordered`/
  `atomic_xor_ordered(a, v, order)`, and
  `atomic_cas_ordered(a, expected, new, success_order, failure_order)`.
- **`fence(order MemoryOrder)`**, not `atomic_fence`. Maps to
  `std::sync::atomic::fence`; accepts `acquire`/`release`/`acq_rel`/`seq_cst`. The
  single-thread compiler fence (`atomic_fence_signal`/`compiler_fence`) is
  deferred.
- **Ordering validity is enforced under a single code, `L0432`** — not the
  proposed `L0433`/`L0434`. (The proposed `L0430`–`L0432` numbers below predate
  the registry and are now assigned to unrelated features; the live registry's
  next free code was `L0432`.) A load or a CAS failure ordering may not be
  `release`/`acq_rel`; a store may not be `acquire`/`acq_rel`; a fence may not be
  `relaxed`. A literal keyword ordering is rejected statically; a `MemoryOrder`
  chosen dynamically through a variable type-checks and is guarded at runtime
  (a clean `L0432` runtime error, never a `std` panic).
- **Dynamic orderings are supported.** Unlike the "compile-time-only classifier"
  note below, a `MemoryOrder` is an ordinary runtime `Value::Enum`, so it can be
  bound to a local and passed to an ordered op; the interpreters resolve it at
  the call.
- **Scope.** Delivered on `atomic_i64` only. The other widths, `atomic_bool`,
  weak CAS, and native/WASM lowering of the ordered ops remain deferred (the
  ordered atomics are interpreter-only today, exactly like the base atomics). A
  `run_atomic_orderings.lby` parity fixture asserts an identical deterministic
  result on AST/IR/bytecode.

The remainder of this document is the original design exploration and retains the
proposal's wider naming/scoping for reference.

## Motivation and non-goals

Atomics are the primitive under lock-free data structures, the building block the existing
`Mutex` is implemented *from* at the Rust level, and a hard requirement for kernel/bare-metal
concurrency (the freestanding native target). The delivered `Mutex`
(`mutex_new`/`mutex_add`, [[concurrency_design.md]]) gives *coarse* mutual exclusion; atomics
give *fine-grained*, allocation-free, wait-free counters and flags with explicit ordering.

Non-goals for the first increment: `atomic<T>` over arbitrary user types, atomic
floating-point, 128-bit atomics (`atomic_i128`), tagged/DWCAS pointers, and hazard-pointer /
epoch reclamation helpers. These are called out in **Scope & sequencing**.

## Atomic types

An atomic is a **distinct nominal handle type** wrapping one integer cell. It is a shared
reference-semantics handle (like `Chan`/`Mutex` in [[concurrency_design.md]]): cloning the
value shares the same underlying cell, so two threads holding copies see each other's writes.
It is **not** the same as its element type — you cannot do arithmetic on an atomic directly;
every access goes through an operation that names an ordering.

The wrapped element types are exactly the fixed-width integers delivered by the wider-integer
work, plus a pointer-sized cell:

| Lullaby type   | Element      | Backing (interpreter)          | Native lowering          |
| :------------- | :----------- | :----------------------------- | :----------------------- |
| `atomic_i8`    | `i8`         | `std::sync::atomic::AtomicI8`  | `lock`-prefixed byte ops |
| `atomic_u8`    | `u8`         | `AtomicU8`                     | byte ops                 |
| `atomic_i16`   | `i16`        | `AtomicI16`                    | word ops                 |
| `atomic_u16`   | `u16`        | `AtomicU16`                    | word ops                 |
| `atomic_i32`   | `i32`        | `AtomicI32`                    | dword ops                |
| `atomic_u32`   | `u32`        | `AtomicU32`                    | dword ops                |
| `atomic_i64`   | `i64`        | `AtomicI64`                    | qword ops                |
| `atomic_u64`   | `u64`        | `AtomicU64`                    | qword ops                |
| `atomic_bool`  | `bool`       | `AtomicBool`                   | byte ops                 |
| `atomic_usize` | `usize`      | `AtomicUsize`                  | pointer-width ops        |
| `atomic_isize` | `isize`      | `AtomicIsize`                  | pointer-width ops        |

Notes:

- The spelling is a **single reserved type name per width** (`atomic_i32`), not a generic
  `atomic<i32>`. This mirrors how `Chan`/`Task`/`Mutex` are plain reserved names today and
  avoids committing to `atomic<T>` before user generic *types* exist (deferred, Phase 1). When
  generic user types land, `atomic<T>` may become an alias family over these; the operation
  surface below is chosen so that migration is source-compatible.
- `atomic_usize`/`atomic_isize` are the **pointer-sized** atomics. On the interpreters `usize`
  is 64-bit (the interpreter's address space); on native it is the target pointer width. Raw
  atomic *pointers* (`atomic_ptr<T>`) are deferred until pointer casts land (Phase 4, list 04);
  `atomic_usize` covers the common tagged-index / bump-pointer case in the interim.
- The element widths track [[roadmap_1_0.md]]'s scalar set. An `atomic_i128` is deferred
  because not every target lowers it without a library helper.

## Operations

Every operation is a **builtin call** (consistent with `mutex_get`/`mutex_add` and the rest
of the prelude in [[standard_library.md]]). The ordering is the **last argument**, a value of
the reserved `Ordering` type (see the next section); when omitted, the operation-specific
default applies. The element type `E` below is the atomic's wrapped integer type.

### Construction and access

```lby
fn make -> atomic_i64
    atomic_new(0)                 # atomic_i64 with initial value 0 (type from context)
```

- `atomic_new(v E) -> atomic_E` — allocate a new atomic cell initialized to `v`. The atomic
  type is inferred from the expected type at the `let`/return/argument site exactly as
  `list_new`/`none` are (context-directed inference, [[option_result_design.md]]); with no
  expected atomic type the element type is inferred from `v`'s width and signedness. Ambiguity
  is **L0431**.

### Load and store

```lby
let x i64 = atomic_load(a)                 # default seqcst
let y i64 = atomic_load(a, acquire)        # explicit acquire
atomic_store(a, 42)                         # default seqcst
atomic_store(a, 42, release)                # explicit release
```

- `atomic_load(a atomic_E [, ord Ordering]) -> E` — read the cell. Valid orderings:
  `relaxed`, `acquire`, `seqcst`. Default `seqcst`.
- `atomic_store(a atomic_E, v E [, ord Ordering]) -> void` — write the cell. Valid orderings:
  `relaxed`, `release`, `seqcst`. Default `seqcst`.

A `load` with `release`/`acqrel` or a `store` with `acquire`/`acqrel` is a **compile error**
(**L0433**), matching the C++/Rust memory model rule.

### Swap and compare-and-swap

```lby
let prev i64 = atomic_swap(a, 7, acqrel)   # unconditional exchange, returns old value

# strong CAS: returns the observed value; caller compares to `expected`
let seen i64 = atomic_cas(a, expected, desired, acqrel, acquire)

# weak CAS: may spuriously fail; returns ok(old) on success, err(cur) on failure
let r result<i64, i64> = atomic_cas_weak(a, expected, desired, acqrel, acquire)
match r
    ok(old) -> old
    err(cur) -> cur
```

- `atomic_swap(a atomic_E, v E [, ord Ordering]) -> E` — store `v`, return the previous value.
  Any of the five orderings; default `seqcst`.
- `atomic_cas(a atomic_E, expected E, desired E [, success Ordering [, failure Ordering]]) -> E`
  — **strong** compare-and-swap. If the cell equals `expected`, store `desired`; either way
  return the value that was in the cell (so `returned == expected` means it succeeded). This is
  the value-returning shape (like C11 `atomic_compare_exchange` reporting through the expected
  slot) rather than a `bool`, because Lullaby has no out-parameters; the caller re-tests
  `returned == expected`. Defaults: `success = seqcst`, `failure` defaults to the load-strength
  of `success` (see ordering rules).
- `atomic_cas_weak(...) -> result<E, E>` — **weak** compare-and-swap; may fail spuriously even
  when the value matches, so it is meant for retry loops. It returns `ok(old)` on success and
  `err(current)` on failure, so a loop reads the failure branch's value directly and retries
  without a second load. The weak form maps to LLVM `cmpxchg weak` on native and to
  `compare_exchange_weak` on the interpreters; on x86 (no spurious failure) it is identical to
  strong but is still allowed so portable code compiles unchanged.

The `failure` ordering must not be stronger than `success` and must be a load ordering
(`relaxed`/`acquire`/`seqcst`, never `release`/`acqrel`); violations are **L0433**.

### Fetch-and-op

```lby
let old i64 = atomic_add(a, 1, relaxed)    # returns the value *before* the add
atomic_sub(a, 1, acqrel)
let mask i64 = atomic_and(flags, 15, seqcst)
atomic_or(flags, 16, seqcst)
atomic_xor(flags, 1, seqcst)
let lo i64 = atomic_min(a, candidate, acqrel)
let hi i64 = atomic_max(a, candidate, acqrel)
```

Each fetch-and-op atomically applies the operation and **returns the previous value**
(`fetch_*` semantics, so a caller can compute the new value locally when needed):

- `atomic_add(a, v [, ord]) -> E`, `atomic_sub(a, v [, ord]) -> E`
- `atomic_and(a, v [, ord]) -> E`, `atomic_or(a, v [, ord]) -> E`, `atomic_xor(a, v [, ord]) -> E`
- `atomic_min(a, v [, ord]) -> E`, `atomic_max(a, v [, ord]) -> E` (signed/unsigned per the
  atomic's element type)

All five orderings are valid on a read-modify-write; default `seqcst`. `add`/`sub`/`and`/`or`/
`xor`/`min`/`max` are rejected on `atomic_bool` (only `load`/`store`/`swap`/`cas` apply to a
flag) — **L0432**.

Wrap-around for `add`/`sub` follows the element type's defined wrapping arithmetic from the
wider-integer work; there is no atomic checked/saturating variant in this increment (a retry
loop over `atomic_cas` expresses those).

## Memory orderings

Orderings are values of a reserved enum-like type `Ordering` with exactly five nullary
constructors, spelled as bare lowercase keywords at the call site (no `Ordering.` prefix,
matching how bare enum variants resolve today, [[enum_and_match_design.md]]):

| Keyword   | Meaning                                             | Legal on                          |
| :-------- | :-------------------------------------------------- | :-------------------------------- |
| `relaxed` | No ordering, only atomicity.                        | load, store, swap, cas, fetch-op  |
| `acquire` | Later reads/writes cannot move before this.         | load, swap, cas, fetch-op         |
| `release` | Earlier reads/writes cannot move after this.        | store, swap, cas, fetch-op        |
| `acqrel`  | Acquire + release (read-modify-write only).         | swap, cas, fetch-op               |
| `seqcst`  | Single total order across all `seqcst` operations.  | everything (default)              |

Rules the type checker enforces (all under **L0433** "invalid memory ordering for this
operation"):

- **Loads** accept `relaxed | acquire | seqcst`. `release`/`acqrel` on a load → L0433.
- **Stores** accept `relaxed | release | seqcst`. `acquire`/`acqrel` on a store → L0433.
- **Read-modify-write** (`swap`, `cas`, all `fetch_*`) accept all five.
- **CAS failure ordering** must be a load ordering and no stronger than the success ordering.
- The ordering argument must be one of the five keyword constructors; an arbitrary `i64` or a
  variable of another type there is **L0430** (not an `Ordering`).

**Default ordering is `seqcst`** for every operation when the ordering argument is omitted.
Sequential consistency is the safe default: it is the easiest to reason about and the only one
that never introduces a subtle bug when a programmer omits the argument. Performance-sensitive
code opts *down* to `acquire`/`release`/`relaxed` explicitly. This matches Rust's guidance of
"reach for `SeqCst` unless you can prove weaker is correct" and keeps the default from being a
footgun.

`Ordering` is a compile-time-only classifier: it is erased before the backends run (the lowered
IR carries an ordering enum on the atomic op node, not a runtime `Value`). So there is no
`Ordering` value to store in a variable or pass around dynamically in this increment — the
ordering argument must be a literal keyword. A dynamic ordering value is deferred (see Scope).

## Fences

A standalone fence orders the *surrounding* non-atomic and atomic accesses without touching a
specific cell:

```lby
atomic_fence(acquire)         # standalone acquire fence
atomic_fence(release)
atomic_fence(seqcst)
atomic_fence_signal(acquire)  # compiler-only fence (single thread / signal handler)
```

- `atomic_fence(ord Ordering) -> void` — a thread fence. Valid orderings:
  `acquire | release | acqrel | seqcst` (a `relaxed` fence is a no-op and is rejected as
  **L0434** to catch a likely mistake). Maps to `std::sync::atomic::fence` on the interpreters
  and to the target fence instruction on native (`mfence`/`dmb ish`/etc.).
- `atomic_fence_signal(ord Ordering) -> void` — a **compiler** fence: it restrains reordering
  within a single thread (e.g. between a handler and the interrupted code) but emits no CPU
  barrier. Maps to `compiler_fence` on the interpreters and to a codegen scheduling barrier on
  native. Same ordering validity as `atomic_fence`. This is primarily for the freestanding/kernel
  target; ordinary user code should prefer the cell-carrying operations above.

## Backend semantics

The invariant is that **all backends are observationally identical** for well-defined programs,
exactly like the existing concurrency parity harness ([[concurrency_design.md]]): tests assert
on *results*, never on interleaving.

### AST and IR interpreters (and the bytecode VM)

- **Shared cell, not per-thread.** The per-thread heaps in [[concurrency_design.md]] make
  ordinary values thread-local, which is correct for message passing but wrong for a *shared*
  atomic. An atomic value is therefore a handle into a **process-shared** table
  (`Arc<AtomicCell>`), the same pattern `Mutex` already uses (`Arc<Mutex<i64>>`). `atomic_new`
  allocates an `Arc<AtomicI64>` (etc.) and stores it in a new
  `Value::Atomic { width, signed, cell }` variant; cloning the `Value` clones the `Arc`, so a
  cell handed to a `spawn`ed worker (once `spawn` can carry it — see the note below) refers to
  the same memory.
- **Real atomics, real orderings.** Each operation calls the matching `std::sync::atomic` method
  with the ordering translated straight across: `relaxed → Relaxed`, `acquire → Acquire`,
  `release → Release`, `acqrel → AcqRel`, `seqcst → SeqCst`. Because the interpreters run on real
  OS threads over a single address space, the standard library gives the interpreters *genuine*
  hardware-backed ordering — the interpreter behavior is the specification the native backend
  must match, not a simulation.
- **CAS shape.** Strong `atomic_cas` calls `compare_exchange` and returns whichever value the
  cell held (the `Ok`/`Err` payload of `compare_exchange` both carry it). `atomic_cas_weak` calls
  `compare_exchange_weak` and maps `Ok(old) → ok(old)`, `Err(cur) → err(cur)` into a
  `Value::Enum` `result` (mirroring [[option_result_design.md]]).
- **Bytecode VM** round-trips through the IR interpreter (as it does today), so it inherits the
  same behavior with no separate implementation; the `.lbc` encoding gains an atomic-op
  instruction carrying `(op, width, signed, ordering[, failure_ordering])`.
- **`Value` is `Send`.** The atomic handle is `Arc<Atomic*>`, which is `Send + Sync`, so it
  crosses thread boundaries safely once the worker-argument shape allows it.

**Interaction with the current worker-argument limitation.** [[concurrency_design.md]] notes
that `spawn`'s argument shape is fixed to `(Chan, i64)` and first-class functions do not yet
capture their environment, so a worker cannot *today* receive a `Mutex` or a second handle
directly. An atomic handle has the same constraint: it is `Send`/shared-on-clone at the runtime
level (covered by a runtime unit test that shares one `Arc<AtomicI64>` across threads and asserts
the final count), but passing it *into* a Lullaby worker waits on capturing closures or a more
general `spawn` (Phase 1). Until then, atomics are exercised through `parallel_map`-style scoped
parallelism and runtime tests. This sequencing is intentional — the atomic primitive lands and is
proven at the runtime level before the surface that feeds it a worker.

### Native backend

- Each operation lowers to the target's atomic instruction with the ordering carried into
  codegen: on x86-64, `lock`-prefixed RMW (`lock xadd`, `lock cmpxchg`, `lock and`, …), a plain
  `mov` for `relaxed`/`acquire`/`release`/`seqcst` load/store (x86 loads/stores are already
  acquire/release; a `seqcst` store additionally uses `xchg`/`mfence`), and `mfence` for a
  `seqcst` fence. On ARM64, load-acquire/store-release (`ldar`/`stlr`), `ldaxr`/`stlxr` retry
  pairs for RMW/CAS, and `dmb ish` for fences.
- The native backend reuses the IR atomic-op node (op + width + signed + ordering) that the
  interpreters consume, so there is one lowering contract. Widths map to the natural operand
  size; `atomic_usize`/`atomic_isize` map to the target pointer width.
- Native codegen is where atomics are *most* load-bearing (freestanding/kernel concurrency), but
  it trails the interpreter delivery: the interpreters give real, testable atomic behavior first,
  and native lowering is validated against interpreter results for the scalar functions the
  native backend already supports.

### Parity harness

A `run_atomics.lby` fixture builds a shared counter, drives a deterministic number of
increments (via `parallel_map` and/or scoped workers), and reads the final total — a value that
is deterministic regardless of scheduling — asserting the identical result on AST, IR, bytecode,
and the optimized IR/bytecode variants, exactly like `run_parallel.lby`/`run_spawn.lby`. A
second fixture exercises an `atomic_cas_weak` retry loop building a lock-free maximum. Never
assert on interleaving.

## Thread-local storage (brief; also its own ticket)

TLS gives each thread its own instance of a named value — the counterpart to atomics' *shared*
cell. It is a small, separable increment (its own ClickUp ticket under list **14 Concurrency**)
and is sketched here for coherence:

- **Declaration:** a top-level `threadlocal NAME TYPE = INIT` binding (indentation-only, no new
  block form). Each thread lazily initializes its own copy from `INIT` on first access.
- **Access:** `tls_get(NAME) -> TYPE` and `tls_set(NAME, v TYPE) -> void`, or plain reads/writes
  of `NAME` treated as a per-thread global. The first increment restricts `TYPE` to a scalar
  (an integer/`bool`) to avoid per-thread heap-value lifetime questions.
- **Interpreter mapping:** Rust's `thread_local!` macro gives real per-thread storage; each
  Lullaby `threadlocal` becomes one `thread_local!` cell keyed by name, so worker threads
  spawned over the shared program each see their own copy while the program (`Arc`) stays shared.
- **Native mapping:** the platform TLS mechanism (`.tls` section / `__thread` / `gs`-relative on
  Windows x64, `tpidr_el0` on ARM64).
- **Diagnostics:** a non-scalar TLS type or a `tls_get`/`tls_set` on an undeclared/mistyped name
  is **L0435** (reserved here; the TLS ticket owns its final definition).

TLS is deferred behind atomics because atomics unblock lock-free structures directly, while TLS
is mostly an ergonomic alternative to passing state explicitly.

## Diagnostics

Proposed new codes in the semantic band, following the registry's convention that concurrency/
memory-completeness diagnostics extend upward from the existing L03xx concurrency codes
(`L0334` `parallel_map`, `L0337` channels/mutex, `L0344` async). These are **proposals only**;
this document does not edit [[diagnostic_registry.md]].

| Code    | Phase    | Meaning                                                                 |
| :------ | :------- | :---------------------------------------------------------------------- |
| `L0430` | semantic | Atomic builtin argument type or arity mismatch (non-atomic handle where an atomic is required, wrong element type for the cell, non-`Ordering` ordering argument, or wrong argument count). |
| `L0431` | semantic | Uninferable atomic element type — `atomic_new` had no expected `atomic_*` type from context and the element width/signedness could not be inferred from the initializer. |
| `L0432` | semantic | Operation not supported on this atomic type (e.g. `atomic_add`/`atomic_min`/`atomic_and` on `atomic_bool`). |
| `L0433` | semantic | Invalid memory ordering for this operation (`release`/`acqrel` on a load, `acquire`/`acqrel` on a store, or a CAS failure ordering that is a store ordering or stronger than the success ordering). |
| `L0434` | semantic | Invalid fence ordering (`relaxed` fence, or a fence with no ordering argument). |
| `L0435` | semantic | Invalid thread-local declaration or access (non-scalar TLS type, or `tls_get`/`tls_set` on an undeclared or mistyped name). Owned by the TLS ticket. |

Reuse existing codes where they already fit rather than minting new ones: a `match` on the
`result` returned by `atomic_cas_weak` uses the existing `L0384`/`L0385`, and a non-exhaustive
handling of it is the ordinary `L0384`.

## Scope & sequencing

**First increment (this design):**

1. The atomic types over the delivered integer widths + `bool` + `usize`/`isize`.
2. `atomic_new`, `atomic_load`, `atomic_store`, `atomic_swap`, `atomic_cas`,
   `atomic_cas_weak`, and the fetch-and-op family.
3. The five orderings as literal keyword arguments with `seqcst` default and the checker rules.
4. `atomic_fence` / `atomic_fence_signal`.
5. Interpreter (AST/IR/bytecode) implementation with real `std::sync::atomic` backing and a
   deterministic parity fixture; runtime tests that share a cell across threads directly.
6. Native lowering of the same IR atomic-op node, validated against interpreter results.

**Sequenced next / deferred (each a separate ticket under list 14):**

- **Thread-local storage** (`threadlocal` + `tls_get`/`tls_set`), as sketched above.
- **Passing an atomic into a Lullaby worker** — unblocked by capturing closures / a general
  `spawn` (Phase 1), the same dependency `Mutex` has in [[concurrency_design.md]].
- **Dynamic ordering values** — treating `Ordering` as a first-class runtime value; not needed
  until a use case appears, and it complicates native codegen (the ordering must be static there).
- **`atomic_ptr<T>` / tagged pointers / DWCAS** — waits on pointer casts (Phase 4, list 04).
- **`atomic<T>` generic spelling**, atomic floating-point, `atomic_i128`, and reclamation
  helpers (hazard pointers / epoch GC).

## Why these choices

- **Distinct per-width handle types, not `atomic<T>`.** Matches the existing `Chan`/`Mutex`
  reserved-name pattern and does not front-run user generic types; the operation surface is
  chosen to migrate cleanly to `atomic<T>` later.
- **Real atomics in the interpreters.** Because the interpreters already run real OS threads over
  a single address space (`Arc` share, [[concurrency_design.md]]), backing each cell with
  `std::sync::atomic` gives genuine, hardware-enforced ordering — so the interpreter *is* the
  reference semantics the native backend matches, not an approximation, and the parity harness
  stays honest.
- **Ordering as a static keyword with a `seqcst` default.** Sequential consistency is the
  correct-by-default choice; weaker orderings are an explicit opt-in. Keeping `Ordering`
  compile-time-only means native codegen always has a static ordering to lower and there is no
  dynamic-ordering complexity in the first increment.
- **Value-returning CAS + weak/strong split.** Lullaby has no out-parameters, so returning the
  observed value (strong) or a `result<E, E>` (weak) is the idiomatic shape, and it lets a retry
  loop reuse the failure value without a re-load. Exposing `weak` even where the target has no
  spurious failure keeps portable lock-free code compiling unchanged.
- **`fetch_*` returns the previous value.** Consistent with `std::sync::atomic` and C11, and it
  is the strictly-more-informative choice (the new value is a local add away).
- **Fences and a signal/compiler fence.** Required for the freestanding/kernel target; kept as a
  thin standalone surface so ordinary code prefers the cell-carrying operations.
