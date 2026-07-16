# Lullaby Memory Management System

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

This document covers the memory management architecture for **Lullaby** (lullaby), a compiled systems programming language. The memory system is designed to balance performance, safety, and LLM-understandability while maintaining explicit control suitable for OS development.

> **Model direction (canonical):** Lullaby is **arena-first** — arena / region
> allocation is the primary, default memory model. Reference counting (`ref`/RC)
> is a **secondary, opt-in tool** for dynamic-lifetime or shared data that escapes
> its region, and raw pointers + manual memory are an `unsafe` escape hatch. The
> model is presented in **two tiers** (a safe tier that assumes a minimal runtime,
> and a freestanding `no-runtime`/kernel tier with no host allocator and no RC).
> This document covers the *mechanics and behavior*; the memory-model decision and
> the 1.0 identity are owned by
> [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md), which is
> canonical. Implementation staging lives in
> [memory_model_decision.md](memory_model_decision.md).

---

## Current Runtime Slice

The first executable runtime implements a deliberately small memory model so the language can be tested end to end before the full arena + RC design lands:

- `alloc(value)` stores a runtime value in an internal heap slot and returns an interim pointer value. **It is a box constructor, not a byte allocator** — the argument is a *value*, not a size. `alloc(8)` is a cell holding `8`, so `load`/`ptr_read` of it yields `8`; it does not reserve 8 bytes. The result types as `ptr_{typeof value}`.
- `load(ptr)` reads a cloned value from a valid heap slot.
- `store(ptr, value)` replaces the value in a valid heap slot. Static checking requires the stored value type to match the pointer element type.
- `dealloc(ptr)` clears a heap slot and reports a runtime error on invalid or double deallocation. Note the free tracking is layered: `L0350` catches a use-after-free / double-free **statically but by variable name**, so it does not survive aliasing (`let q = p` / `dealloc(p)` / `ptr_read(q)` compiles); the aliased case is caught at run time by the interpreters' `L0406`.
- Static semantic checking models pointer types with concrete names such as `ptr_i64`. This spelling is usable in a `let` annotation, a parameter, and a return type — and it is **distinct from, and not convertible to, the typed `ptr<T>`** family (`let p ptr<i64> = alloc(8)` is `L0303`; passing a `ptr_i64` to a `ptr<i64>` parameter is `L0313`). `ptr_read`/`ptr_write` accept both spellings; `dealloc` accepts only `ptr_T`. See "The Two Pointer Models" in [native_backend_contract.md](native_backend_contract.md) for the full survey and the open owner decision on unifying them.
- **Native backend status.** `alloc` has native codegen: one 8-byte cell from the shared `__lullaby_alloc` bump/RC helper plus an initializing store, returning the cell's real address, so a heap-box program compiles to a real executable and agrees with all three interpreters — including **across frame boundaries** (the out-parameter idiom and a returned allocation), which is the one pointer form with that property on every tier. `dealloc` is deliberately **not** lowered natively and skips cleanly (`L0339`) to the interpreters, because they *detect* a later use / double free (`L0406`) and no lowering on a bump/RC heap can reproduce that without turning a detected error into silent corruption. An `alloc`'d cell is never reclaimed natively (matching the interpreters, whose heap `Vec` also only grows) and, being invisible to the arena escape analysis, excludes its function from arena routing (`alloc_defeats_arena`) so no bump rewind can free a live box.
- Typed IR analysis reports memory operations with artifact-order sequence metadata and safety metadata for live-resource requirements, bounds checks, memory mutation, cleanup role, and unsafe-boundary handling. Current reported operations are `alloc`, `load`, `store`, `dealloc`, and array-index bounds checks. Region create, region resize, copy, and compiler cleanup operation kinds now have safety metadata reserved for future lowering. Version 5 `.lbc` bytecode artifacts preserve this metadata in `memory_operations`, and artifact decoding validates that the metadata still matches the module instructions.
- Arena / region allocation is the primary, default heap model; reference counting is the secondary, opt-in tool for escaping data (see [memory_model_decision.md](memory_model_decision.md) for the implementation staging). The native backend already ships the RC substrate — a free-list allocator with a per-block RC header plus `rc_dec`/`rc_free` helpers and scope-based drop insertion for uniquely-owned, borrow-only `string`/`array<string>` loop temporaries. Arena-first allocation, broader RC drop coverage, and the freestanding `unsafe`/raw-pointer tier remain planned work; Perceus-style reuse is deferred (with arenas as the default, most allocation never touches RC). There is no tracing garbage collector.

Example:
```lullaby
fn main -> i64
    let ptr ptr_i64 = alloc(0)
    store(ptr, 41)
    let value i64 = load(ptr)
    dealloc(ptr)
    value + 1
```

---

## Design Philosophy

> **Model decision (supersedes any tracing-GC framing below):** Lullaby is
> **arena-first** — arena / region allocation is the primary, default model.
> **Reference counting** (`ref`/RC) is a **secondary, opt-in tool** for
> dynamic-lifetime or shared data that escapes its region, and raw pointers +
> manual memory are an `unsafe` escape hatch. The heap is **not** garbage-collected.
> This matches `language_specification.md` and the shipped `rc<T>`/`ref<T>` surface.
> The high-level decision and 1.0 identity are canonical in
> [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md); the
> implementation staging is in
> [memory_model_decision.md](memory_model_decision.md). Where the sections below
> describe tracing GC they are historical/rejected.

### Core Principles

1. **Arena / region allocation (primary)**: scope-local data bump-allocates into a
   region and bulk-frees at scope exit — faster than `malloc`/`free`, zero
   per-object refcount traffic, optimized for predictable memory layout (critical
   for OS kernels)
2. **Reference counting (secondary, opt-in)**: for data whose lifetime is dynamic or
   shared across a graph, RC frees deterministically when the last owner drops (no
   GC pauses); it never runs in the freestanding tier
3. **Raw pointers + manual memory (escape hatch)**: available inside `unsafe` /
   freestanding for the hardware edge, not the default that infects every program
4. **Lifetime Tracking**: Automatic scope-based lifetime management through AST analysis
5. **Minimal Runtime Overhead**: Syntax-directed `inc`/`dec`/`drop` insertion on the
   RC path — no collector, no stop-the-world, no compile-time cost that erodes the fast build
6. **Deterministic Behavior**: Memory operations explicitly trackable and verifiable by the compiler

### Two Tiers

Lullaby presents one syntax and one type system in **two tiers** that differ only in
what runtime they assume (canonical detail:
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md)):

- **Safe tier (default)** — apps & services. Arena-first, bounds-checked, actor
  concurrency, RC opt-in. Assumes a minimal runtime (allocator backing arenas, RC
  helpers, panic→abort, actor scheduler).
- **Freestanding tier (`no-runtime` / kernel)** — kernels, boot, embedded, FFI. No
  CRT, no host allocator, **no RC**, and no hidden allocation or control flow. Arenas
  still work, backed by a **caller-provided static buffer**; raw pointers + `unsafe`
  are first-class; bounds-check failure calls a user-provided panic handler.

### Key Differentiators from Existing Languages

| Traditional Language | Lullaby Approach |
|---------------------|--------------------|
| Global heap GC (Java, C#, Go) | **Arena-first allocation** + opt-in **reference counting** (no GC pauses) |
| Manual memory management (C, C++) | **Arena-first safe defaults** + `unsafe` raw pointers only at the hardware edge |
| Borrow checker (Rust) | **Arenas + runtime-enforced safety** — memory-safe without borrow-check fights |
| Safe *or* low-level — pick one | **Two tiers, one language** — safe arena defaults; `no-runtime` freestanding tier for kernels/embedded |

---

## Memory Model Architecture

### Memory Regions

Memory in lullaby is organized into distinct regions, each with specific properties:

```lullaby
# Region definition syntax

region [NAME]: size=SIZE, align=ALIGN [optional]
```

#### Region Types
1. **Code Region**: Stores executable instructions (read-only after compilation)
   ```lullaby
   region code_kernel: size=65536, align=4096
   ```

2. **Data Region**: Stores mutable data structures
   ```lullaby
   region kernel_data: size=131072, align=8192
   ```

3. **Stack Region**: Function call frames and local variables
   ```lullaby
   region stack: size=4194304, align=16
   ```

4. **Heap Region**: Dynamically allocated, **reference-counted** objects (freed
   deterministically at scope exit — no garbage collector)
   ```lullaby
   heap_objects: max_size=524288048
   heap_limit = 500 MB
   ```

#### Region Properties
```lullaby
region buffers:
    size = 16384
    align = 8
    type = writeable
end_region
```

### Memory Addressing

#### Direct Address Access
```lullaby
# Via region identifier

access region_buffers[0x100]

# Via variable address (compiler tracks addresses automatically)

addr(my_var)  # Returns memory address of variable

# Pointer dereferencing

ptr_x = data[i]
value = deref(ptr_x)
```

#### Memory Layout Visualization
The compiler generates a memory map showing region boundaries and contents:
```
Address Range        Region Name          Size (bytes)    Align
0x00000 - 0x0FFFF    code_kernel           64 KB           4096 (page-aligned)
0x10000 - 0x2FFFF    kernel_data           128 KB          8192 (cache-line aligned)
0x30000 - 0x4FFFF    stack                256 KB          16 (call-stack aligned)
0x50000 - 0x9FFFF    heap_objects          ~475 MB         8   (reference-counted, free-list)
```

---

## Memory Allocation Mechanisms

### Static-Buffer Arenas (Freestanding Tier) — DELIVERED (2026-07-16)

In the freestanding / `no-runtime` tier there **is no host allocator**, so every
mechanism below that grows a heap is unavailable (`L0441`). The one allocation form
that tier has is a **static-buffer arena**: memory the *caller already owns*, handed
out by a bump cursor.

```lby
no-runtime

fn two_cells -> i64
    let buf array<i64> = [0, 0, 0, 0, 0, 0, 0, 0]   # the caller's buffer
    region scratch in buf                            # a bump arena over it
    unsafe
        let a ptr<i64> = arena_alloc(scratch, 1)     # bump 1 cell
        let b ptr<i64> = arena_alloc(scratch, 1)
        ptr_write(a, 30)
        ptr_write(b, 12)
        ptr_read(a) + ptr_read(b)                    # 42 — distinct cells
```

- The arena's extent is the buffer's; it **never grows and never calls an
  allocator**, which is exactly why the `no-runtime` gate permits it.
- The bump unit is the **8-byte cell** (hence `array<i64>`), because every Lullaby
  scalar is stored as a normalized 8-byte cell — the same reason `addr_of` is
  8-byte-only.
- **Overflow is a defined, deterministic edge**: a bump past the buffer traps
  (`ud2`) natively and aborts with **`L0460`** on the interpreters — the same
  relationship the array-bounds failure already has (`L0413` / `ud2`), and what
  decision A5 requires: abort with a diagnostic, no unwinding. It can never hand
  back a pointer past the buffer's end. The pluggable panic handler
  ([freestanding_tier_design.md](freestanding_tier_design.md) §8, undelivered) will
  route both edges to the program's own `panic fn`.
- **Full four-tier parity.** An arena cell is an ordinary `array<i64>` element, so
  `arena_alloc(r, n)` is `addr_of(buf[cursor])` plus an integer cursor — which every
  tier defines. The interpreters reuse that same place-backed `addr_of` machinery, so
  the pointer genuinely aliases the buffer.
- **Two arenas over one buffer are rejected** (`L0445`). Each bumps from its own
  cursor starting at zero, so they would hand out the *same* cells and silently
  clobber each other. Separate bounded pools need separate buffers.
- **Lifetime.** The arena's memory *is* the buffer, so a pointer into it is valid
  exactly as long as the buffer's binding — its enclosing frame. Using one after
  that frame returns is real undefined behaviour, precisely as the equivalent C is,
  which is why the surface is `unsafe`-gated. Do not return an arena pointer from
  the function that owns the buffer.
- **Available in both tiers**, like `unsafe` and the raw-pointer builtins — it is
  load-bearing for `no-runtime`, but a safe-tier function may use one too.

Canonical design and as-built record:
[freestanding_tier_design.md](freestanding_tier_design.md) §5 / §5.2; native
contract: [native_backend_contract.md](native_backend_contract.md).

### Region-Based Allocation (Primary)

Regions provide explicit, deterministic memory management:

#### Static Regions (Compile-Time Allocation)
Allocated at program startup, fixed size:
```lullaby
region config_data: size=4096
    # Contents allocated here during initialization
    settings: array[config_entry]
init(config_data)  # Compiler reserves space, runtime initializes
```

#### Dynamic Regions (Runtime Resizing)
Regions that can grow/shrink based on runtime conditions:
```lullaby
region dynamic_buffer: size=1024
# Can be resized at any point

buffer_size = 8192
resize(dynamic_buffer, buffer_size)
```

### Stack Allocation (Implicit)

Automatic memory for local variables and function frames:
```lullaby
func process_data(data: array[float]) -> float
    sum is 0          # Stack allocated (auto-cleanup on exit)
    count is len(data) # Stack allocated

    for value in data do
        temp = sum + value  # Stack allocated per iteration
        sum = temp
    end_for

    return sum / count

# All variables declared with 'let' or 'var' are stack-allocated

```

#### Stack Frame Structure
```lullaby
function_frame:
    return_address: int        # Return address for unwind
    frame_pointer: ptr         # Points to previous frame base
    local_variables:           # All 'let', 'var', function params
        x: int
        y: float
        result: string
```

### Heap Allocation (Reference-Counted, Secondary)

For objects that cannot be region-bound — data whose lifetime is dynamic or shared
across a graph, i.e. that escapes its region — the heap is **reference-counted**.
This is the *secondary* allocation tool: arenas handle the common, scope-local case;
RC handles the escaping minority. Each heap block carries a 16-byte header
`[size][refcount]` before its payload; the allocator is a free-list allocator that
first-fit-reuses freed blocks. A block is freed **deterministically** the moment
its refcount reaches zero (typically at scope exit) — there is no tracing garbage
collector, no stop-the-world pause, and no compaction pass.

#### Object Definition
```lullaby
type Node [id: int, data: array[byte]]
    methods:
        read() -> bool
end_type

node = alloc(Node, id=5)  # Heap allocation (returns pointer, refcount 1)
data_ptr = node.data      # Access via dereference
```

#### Scope-Directed Reclamation
Heap usage is tracked by refcount, not by a collector. Binding or copying a shared
handle inserts an `inc`; each scope exit inserts a `dec`/`drop`, and the block is
reclaimed when the last owner drops it:
```lullaby
# Objects go to the reference-counted heap when they escape stack/region scope

user_profile [Profile]
    name: string
    age: int
# The compiler inserts inc/dec so the block frees when its last owner drops

```

---

## Memory Lifecycle Management

### Lifetime Tracking System

The compiler analyzes variable scopes and lifetimes to determine memory lifetime:

#### Scope-Based Lifetimes
```lullaby
func process() -> result
    local_var is 42           # Lifetime: function scope
    shared_data = load_resource()  # Lifetime: module/global scope

    if condition then
        temp_result is compute()  # Lifetime: block scope (auto-deleted after end_if)
    else
        other_result is alt_compute()  # Also auto-cleanup
    end_if

    return local_var + shared_data  # local_var deleted before return

# Memory cleanup happens automatically when:

# - Function exits

# - Control flow leaves block scope

# - Region explicitly deallocated (region free command)

```

#### Lifetime Categories
1. **Stack Variables** (`let`, `var`): Auto-cleanup on scope exit
2. **Global/Static Variables**: Persistent lifetime until explicit cleanup or program end
3. **Heap Objects**: Reference-counted — freed deterministically when the last owner
   drops (refcount reaches zero), not by a tracing collector

### Memory Cleanup Mechanisms

```lullaby
# Explicit region deallocation

region temp_buffer: size=1024
    # ... use buffer ...
free temp_buffer  # Deallocates entire region immediately

# Implicit variable cleanup (scope exit)

for i from 0 to 10 do
    local_i = i  # Cleanup when loop exits or end_for reached
end_for

# Explicit object reference clearing

ref obj = alloc(MyObject)
obj.process()
clear ref obj  # Drops this owner; block frees if it was the last reference
```

---

## Reference-Counting System

Reference counting is Lullaby's **secondary, opt-in** heap tool — for escaping
data that arenas cannot bound — **not** a garbage collector. Tracing GC was
evaluated and rejected (it needs precise stack maps — a poor fit for the
hand-written, no-SSA native emitter — introduces stop-the-world pauses that are
wrong for a systems language, and contradicts the shipped reference-counting
commitment). The high-level model decision is canonical in
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md); the options
analysis and staged plan are in
[memory_model_decision.md](memory_model_decision.md). RC is **never present in the
freestanding tier**, which has no host allocator and no runtime.

### RC Design Goals
1. **Deterministic reclamation**: a block frees the instant its last owner drops
   (refcount reaches zero) — a predictable point, no pauses, no collector runtime.
2. **Syntax-directed codegen**: `inc`/`dec`/`drop` are inserted at binds, copies,
   and scope exits — no global analysis and no precise stack maps, so the fast
   compile is preserved.
3. **Value semantics cut the cost**: copied (non-shared) aggregates need no
   refcount at all; only genuinely-shared handles carry counting traffic.
4. **LLM-trackable**: ownership follows the lexical scope, so reclamation points
   are visible in source.

### Runtime Mechanics (Native Backend)

Each heap block carries a 16-byte RC header `[size][refcount]` before its payload;
the returned pointer names the payload, so the refcount lives at `[ptr - 8]` and
the size at `[ptr - 16]`. Two `.text` runtime helpers implement counting:

- `__lullaby_rc_dec(ptr)` — decrements `[ptr - 8]`; if it reaches zero it
  tail-calls `__lullaby_rc_free`, otherwise the block stays live.
- `__lullaby_rc_free(ptr)` — pushes the block onto the LIFO free list
  (`__lullaby_free_head`); the allocator first-fit-reuses freed blocks.

Recursive drops (a `list<string>`, a struct with heap fields, an enum with a heap
payload) `rc_dec` each owned child before freeing the block, mirroring the
interpreters' recursive `Value::clone`/drop.

### Scope-Based Drop Insertion

Drops are inserted **default-deny**: a block is dropped only when the compiler can
prove it is uniquely owned (a fresh allocation, never aliased/shared) *and* dead
(no later use on that path). Whenever there is any doubt, no drop is emitted and
the block simply leaks — safe — rather than risking a double-free. A double-free
is memory corruption; a leak is not. Correctness never depends on the analysis
being generous.

The current native increment reclaims uniquely-owned, borrow-only `string` and
`array<string>` loop-body temporaries on the loop back-edge **and on the
`break`/`continue` early-exit edges** of the loop that declares them, so each
dynamic iteration drops each owned temporary exactly once. Early exits that span
multiple scopes (`return`, `throw`, `?`) and additional heap types (per-iteration
`list`/`map` values, heap-payload enums, cross-call fresh temporaries) are not yet
dropped natively; those paths leak safely until their increments land. See
[native_backend_contract.md](native_backend_contract.md) for the exact per-edge
drop contract and its verification.

### Cycles

Non-cycle-collecting RC leaks reference cycles (`a → b → a`). Lullaby mitigates
this first with value semantics (most data does not alias) and non-owning `ref<T>`
borrows (which do not raise a refcount), and defers a trial-deletion cycle
collector until real programs demonstrate owning cycles.

---

## Memory Safety Guarantees

### Bounds Checking (Compile-Time Optimized)

```lullaby
# Array access with automatic bounds checking

for i from 0 to len(arr) do
    value = arr[i]  # Compiler generates bounds check, optimizes away in safe contexts

    if i >= arr.length then
        error("Index out of bounds")
    end_if
end_for

# Pointer dereference validation

ptr_val = deref(ptr_x)
if ptr_null(ptr_x) then
    error("Dereferencing null pointer")
end_if
```

### Memory Access Rules
```lullaby
# Safe memory operations (compiler can verify)

read(addr)        # Returns value at address
write(addr, val)  # Stores value at address
copy(src, dst)    # Copies memory region to region/pointer

# Unsafe operations (require explicit 'unsafe' marker)

unsafe
    raw_write(0x12345678, buffer)  # Bypasses bounds checking
```

### Raw-Memory Layout Intrinsics (Implemented)

The compiler exposes a raw-memory intrinsic set that systems and FFI code need.
Layout follows the **C-natural layout** so it matches the platform C ABI:

- Scalar byte sizes: `i8`/`u8`/`bool`/`byte` = 1, `i16`/`u16` = 2,
  `i32`/`u32`/`f32`/`char` = 4, `i64`/`u64`/`f64`/`isize`/`usize` = 8; every
  pointer or reference handle (`ptr<T>`/`rc<T>`/`ref<T>`) = 8. A scalar's
  alignment equals its size (capped at 8).
- Struct: fields in declaration order, each aligned up to its natural alignment;
  the struct size is the offset past the last field rounded up to the struct's
  alignment (its maximum field alignment). Nested structs recurse.
- Fixed `array<T>` of length `n`: `n * stride(T)`, where the element stride is
  `size_of(T)` rounded up to `align_of(T)`; alignment = `align_of(T)`.

```lullaby
struct Mixed
    a byte      # offset 0
    b i32       # offset 4  (3 bytes padding after `a`)
    c i16       # offset 8
    d i64       # offset 16 (6 bytes padding after `c`)
# size_of(Mixed) = 24, align_of(Mixed) = 8, offset_of(m, "d") = 16
```

- `size_of(x) -> i64` / `align_of(x) -> i64`: safe, compile-time layout queries
  over `x`'s static type (any scalar, pointer/reference handle, struct, or fixed
  `array<T>`). They fold to `i64` constants; a type with no defined layout is
  rejected with `L0431`.
- `offset_of(x, "field") -> i64`: the byte offset of `field` within struct value
  `x`. `field` must be a string literal naming an existing field; otherwise
  `L0431`.
- `ptr_to_int(ptr<T>) -> i64` / `int_to_ptr(i64) -> ptr<T>` (inside `unsafe`):
  reinterpret a raw pointer as its integer address/handle and back. They
  round-trip — `int_to_ptr(ptr_to_int(p))` is the same pointer. On the
  interpreters a pointer is a heap-slot handle, so the integer is that handle.
- `volatile_load(ptr<T>) -> T` / `volatile_store(ptr<T>, value) -> void` (inside
  `unsafe`): element read/write with volatile semantics — no elision and no
  reordering. On the AST, IR, and bytecode interpreters they behave exactly like
  `load`/`store` over the single-threaded abstract heap, which is a correct
  implementation; the volatility guarantee (suppressing compiler elision and
  reordering) is a code-generation concern realized on the native backend.
- **Raw-pointer addressing (freestanding-tier stage 2, inside `unsafe`).** The
  addressing trio for pointer arithmetic and address-of that boot/kernel code needs
  pervasively (see [freestanding_tier_design.md](freestanding_tier_design.md) §2.2,
  §10.4):
  - `addr_of(place) -> ptr<T>`: the address of an addressable place — a local, an
    array element (`a[i]`), or a struct field (`s.f`) — of type `T` (which must have
    a defined layout). A whole-array place decays to `ptr<element>`, so `addr_of(a)`
    and `addr_of(a[0])` agree. Taking the address of a temporary (a literal, an
    arithmetic/call result) is `L0458`.
  - `ptr_offset(p ptr<T>, n isize) -> ptr<T>`: element-scaled arithmetic,
    `base + n * size_of(T)`, with `n` a signed element count. `size_of(T)` is the
    C-natural size above; an *unsized* pointee (`string`/`list`/`map`/growable) is
    `L0431`. The size law holds:
    `ptr_to_int(ptr_offset(p, 1)) - ptr_to_int(p) == size_of(T)`.
  - `ptr_cast(p ptr<T>) -> ptr<U>`: reinterpret the pointee type — no value
    conversion, no address change. `U` comes from the surrounding annotation
    (`let bp ptr<byte> = ptr_cast(base)`), defaulting to `ptr<i64>`, exactly as
    `int_to_ptr` resolves its pointee.
  All three are `unsafe`-gated (`L0330` outside `unsafe`) and available in both tiers
  and in a `no-runtime` module (they are kernel core, not gate-rejected). On the
  interpreters `addr_of` snapshots the place into a byte-addressed region so
  `ptr_offset`/`ptr_read` walk it and the size law holds; native/WASM cleanly skip a
  function using any raw-pointer builtin (native raw-pointer codegen is later work).

  > **`addr_of` aliases the place it addresses.** The interpreters back an `addr_of`
  > address with the *place itself* (a root binding plus a field/index path), not a
  > copy, so a pointer genuinely aliases in both directions: `ptr_write(addr_of(x), 5)`
  > sets `x` to `5`, a `ptr_read` after an independent `x = 99` observes `99`, and a
  > write through `ptr_offset(addr_of(a[0]), i)` mutates `a[i]`. This matches real
  > `lea`-based native addressing.
  >
  > **An `addr_of` pointer may be passed into a callee**, which reads and writes the
  > caller's place for real — `poke(addr_of(x))` sets the caller's `x`. This is the
  > out-parameter idiom (C's `scanf("%d", &x)`, `strtol(s, &end, 10)`) and it is
  > well-defined: C11 6.2.4p6 ties an automatic object's lifetime to *its block*, and a
  > call does not end the caller's block. Native `addr_of` is a real `lea`; the
  > interpreters reach the caller's locals through an **env shelf** (a stack of the
  > ancestor frames' environments — see `crates/lullaby_runtime/src/raw_pointer.rs`).
  > All four tiers agree.
  >
  > **What is refused: a genuinely dangling pointer (`L0459`).** An address is only
  > meaningful while its place exists, and two things end that — both **your program's
  > error**, and both undefined behaviour in C:
  >
  > - **The block ended** — a pointer to an inner-block or loop-body local, used after
  >   that block.
  > - **The function returned** — returning `addr_of` of a local.
  >
  > These are refused rather than read, because the storage is no longer the program's:
  > reporting the value still sitting there would be a silent wrong answer.
  >
  > A pointer handed to **another thread** (`spawn`, an `async fn`) is a separate case:
  > each thread runs its own interpreter with its own raw-pointer space, so the address
  > names nothing there and is refused as an invalid pointer (`L0406`).
  >
  > To share memory across a thread boundary, send the *value* instead, or use an
  > `alloc`-backed pointer, which has no frame lifetime and is unaffected by any of
  > this. (Note the spelling: an `alloc` result is typed `ptr_T` — e.g. `ptr_i64` —
  > **not** `ptr<T>`; the two are distinct and not convertible.)

### Memory Safety Features

#### Null Pointer Handling
```lullaby
is_null(ptr)     # Returns true if pointer is NULL or points to deallocated memory
if is_null(user_ptr) then
    error("Accessing null reference")
end_if
```

#### Use-After-Free Detection
Automatic tracking of freed memory:
```lullaby
region temp_data: size=1024
    # ... operations using temp_data ...
free temp_data

# Later in code (use-after-free error):

value = deref(temp_data)  # Compiler flags this as unsafe after free
```

---

## Compilation Integration

### Memory Analysis Phase

During compilation, the analyzer performs:

1. **Memory Region Mapping**: Identifies which regions are referenced by each function
2. **Lifetime Analysis**: Tracks variable scopes to determine memory lifetimes and
   where `inc`/`dec`/`drop` must be inserted
3. **Access Validation**: Checks all memory accesses for bounds and null safety
4. **Ownership / Escape Analysis**: Determines which bindings are uniquely owned and
   dead at scope exit (droppable) versus shared or escaping (left to their owner)

### IR Memory Representation

Intermediate representation includes explicit memory operations:

```lullaby
# Original source code

func process(arr) -> sum
    total is 0
    for x in arr do
        total = total + x
    end_for

# Compiled to LLVM-like IR with memory ops

%result = alloca i64, name: "total"
%arr_ptr = alloca ptr, name: "arr"
store %arr_array, %arr_ptr

%i = alloca i32, name: "loop_i"
%x_val = alloca i64, name: "x_value"

loop:
    load %x_val from %arr_ptr[%i]  # Memory read with bounds check
    %new_total = add %total, %x_val
    store %new_total to %result
    inc %i

end_loop:
    return deref(%result)

free %arr_array
free %i
free %x_val
free %result
```

---

## Memory Performance Optimization

### Allocation Efficiency

#### Region Packing (Memory Compactification)
Regions are automatically packed to minimize fragmentation:
```lullaby
# Compiler packs region contents for efficient memory usage

region packed_data: size=262144, align=8
    # Objects here are contiguous in memory

# Performance impact: 90% memory utilization vs 75% without packing

```

#### Stack Optimization
Function parameters and local variables coalesced efficiently:
```lullaby
func multi_param(a, b, c) -> result
    # Compiler optimizes stack layout:
    # [return address]
    # [frame pointer]
    # [c, b, a] (in declaration order for cache efficiency)
```

### RC Performance Tuning

Reference counting has no collection frequency to tune — a block frees the instant
its last owner drops. The performance story is dominated by keeping most allocation
*off* the RC path entirely:

- **Arenas (regions)** — the primary, default model: scope-local, provably-non-escaping
  allocations bump-allocate into a scope arena and bulk-free at scope exit, skipping
  refcounting entirely. Because arenas are the default, the RC path carries only the
  escaping minority of allocations.
- **Perceus-style reuse (deferred, lower priority)**: when a refcount is provably 1
  and the object dies, its memory is reused in place (turning a functional-style
  update into in-place mutation), eliding redundant `inc`/`dec` pairs. This optimizes
  the RC path only; with arenas as the default it is no longer a headline perf lever,
  so it is deferred.

See [memory_model_decision.md](memory_model_decision.md) for the staging of these
layers.

---

## Memory System API (Runtime Library)

### Core Memory Functions

```lullaby
# Region operations

region_create(name: string, size: int, align: int) -> ptr
region_resize(ptr, new_size: int) -> bool
region_free(ptr) -> void
region_copy(src_region_ptr, dst_region_ptr) -> int  # Returns bytes copied

# Variable/stack operations

alloc_stack(type_name: string) -> ptr
dealloc_stack(ptr) -> void

# Heap / reference-counting operations

alloc_heap(type: Type) -> ptr    # Allocate a reference-counted block (refcount 1)
realloc(ptr: ptr, new_size: int) -> ptr
rc_clone(ptr: ptr) -> ptr        # Add an owner (inc refcount), returns the handle
rc_release(ptr: ptr) -> void     # Drop an owner (dec refcount; free at zero)
free(ptr: ptr) -> void           # Explicit drop (equivalent to the last release)

# Memory query functions

addr(x: variable_name) -> int
size(ptr: ptr) -> int
is_null(ptr: ptr) -> bool
ptr_valid(ptr: ptr, max_addr: int) -> bool
memory_stats()
    heap_used: int
    stack_used: int
    region_count: int
```

---

## Example: OS Kernel Memory Usage

### Process Control Block Management
```lullaby
process = struct [
    pid: uint,
    state: ProcessState,  # New, Running, Blocked, Terminated
    ppcb_addr: ptr        # Points to process control block
    memory_regions: array[MemoryRegion],
    heap_start: ptr       # Start of process heap region
    stack_start: ptr      # Start of process stack region
]

# Kernel initialization

kernel_process_table_size = 1024
process_region [array]: size=kernel_process_table_size * sizeof(process)

for pid from 0 to kernel_process_table_size do
    pcr = alloc_heap(ProcessControlRecord)
    if is_null(pcr.pcb_addr) then
        error("Failed to allocate PCB for process " + str(pid))
    end_if

    # Register process and initialize heap/stack regions
    process[pid].ppcb_addr = pcr.pcb_addr
    process[pid].heap_start = region_create(proc_heap_"pid" pid, size=8MB, align=4096)
    process[pid].stack_start = region_create(proc_stack_"pid" pid, size=2MB, align=16)
end_for

# Memory cleanup on kernel shutdown

for pid from 0 to kernel_process_table_size do
    if process[pid].state is ProcessState.Terminated then
        free(process[pid].ppcb_addr)
        free(process[pid].heap_start)
        free(process[pid].stack_start)
    end_if
end_for

# Implicit cleanup of active processes (auto-deallocate on kernel exit)

free process_region
```

---

## Summary

The Lullaby memory management system provides:

1. **Arena / region allocation (primary)** for deterministic, efficient systems programming
2. **Stack allocation** with automatic lifetime tracking via scopes
3. **Reference-counted heap (secondary, opt-in)** for objects that escape their region —
   freed deterministically when the last owner drops, with no tracing collector and no pauses
4. **Raw pointers + manual memory (`unsafe` escape hatch)** for the hardware edge and FFI
5. **Comprehensive safety guarantees** through bounds checking and null validation
6. **Explicit memory operations** visible to the compiler for optimization
7. **Integrated compilation pipeline** that analyzes and optimizes memory usage

This model offers the performance and determinism of manual memory management
(C/Rust) with the safety of automatic reclamation, while maintaining LLM-friendly
syntax. Arenas are the default foundation; reference counting is the secondary tool
for escaping data, and Perceus-style reuse is a deferred RC-path optimization (see
[memory_model_decision.md](memory_model_decision.md) for staging, and
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md) for the
canonical model decision).

---

*Next Document: Compilation Architecture - Covers the lexer, parser, AST construction, IR generation, optimization phases, and code emission strategies.*

**Version**: 1.1
**Last Updated**: July 14, 2026
