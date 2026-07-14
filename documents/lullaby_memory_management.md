# Lullaby Memory Management System

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

This document covers the memory management architecture for **Lullaby** (lullaby), a compiled systems programming language. The memory system is designed to balance performance, safety, and LLM-understandability while maintaining explicit control suitable for OS development.

---

## Current Runtime Slice

The first executable runtime implements a deliberately small memory model so the language can be tested end to end before the full region/ARC design lands:

- `alloc(value)` stores a runtime value in an internal heap slot and returns an interim pointer value.
- `load(ptr)` reads a cloned value from a valid heap slot.
- `store(ptr, value)` replaces the value in a valid heap slot. Static checking requires the stored value type to match the pointer element type.
- `dealloc(ptr)` clears a heap slot and reports a runtime error on invalid or double deallocation.
- Static semantic checking models pointer types with concrete names such as `ptr_i64`.
- Typed IR analysis reports memory operations with artifact-order sequence metadata and safety metadata for live-resource requirements, bounds checks, memory mutation, cleanup role, and unsafe-boundary handling. Current reported operations are `alloc`, `load`, `store`, `dealloc`, and array-index bounds checks. Region create, region resize, copy, and compiler cleanup operation kinds now have safety metadata reserved for future lowering. Version 5 `.lbc` bytecode artifacts preserve this metadata in `memory_operations`, and artifact decoding validates that the metadata still matches the module instructions.
- Reference counting is the heap model (see [memory_model_decision.md](memory_model_decision.md)). The native backend now ships a free-list allocator with a per-block RC header plus `rc_dec`/`rc_free` helpers and scope-based drop insertion for uniquely-owned, borrow-only `string`/`array<string>` loop temporaries. Region (arena) allocation, broader RC drop coverage, and Perceus-style reuse remain planned work. There is no tracing garbage collector.

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

> **Model decision (supersedes any tracing-GC framing below):** Lullaby's heap is
> **reference-counted**, layered with **arenas** (regions) for scope-local data —
> **not** garbage-collected. This matches `language_specification.md` and the shipped
> `rc<T>`/`ref<T>` surface. The rationale, options analysis, and staged plan are in
> [memory_model_decision.md](memory_model_decision.md); where the sections below
> describe tracing GC they are historical/rejected and are being rewritten.

### Core Principles

1. **Reference-counted heap + regions**: reference counting frees deterministically
   at scope exit (no GC pauses); arenas (regions) bulk-allocate scope-local data
2. **Region-Based Allocation**: Primary allocation mechanism optimized for predictable memory layout (critical for OS kernels)
3. **Lifetime Tracking**: Automatic scope-based lifetime management through AST analysis
4. **Minimal Runtime Overhead**: Syntax-directed `inc`/`dec`/`drop` insertion — no collector, no stop-the-world, no compile-time cost that erodes the fast build
5. **Deterministic Behavior**: Memory operations explicitly trackable and verifiable by the compiler

### Key Differentiators from Existing Languages

| Traditional Language | Lullaby Approach |
|---------------------|--------------------|
| Global heap GC (Java, C#, Go) | **Reference counting** + **region-based allocation** (no GC pauses) |
| Manual memory management (C, C++) | **Explicit regions** + **automatic RC cleanup** for local scopes |
| Refcount without reuse (Python, Swift) | **RC + arenas**, architected toward Perceus-style in-place reuse |
| Garbage collector as afterthought | **Integrated into compilation pipeline** as core optimization phase |

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

### Heap Allocation (Reference-Counted)

For objects that cannot be region-bound or have indeterminate lifetime, the heap
is **reference-counted**. Each heap block carries a 16-byte header
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

Lullaby's heap is **reference-counted**, not garbage-collected. Tracing GC was
evaluated and rejected (it needs precise stack maps — a poor fit for the
hand-written, no-SSA native emitter — introduces stop-the-world pauses that are
wrong for a systems language, and contradicts the shipped reference-counting
commitment). The rationale, options analysis, and staged plan are in
[memory_model_decision.md](memory_model_decision.md).

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
its last owner drops. The performance levers are instead about reducing refcount
*traffic*, and are layered in as independently-shippable increments:

- **Arenas (regions)**: scope-local, provably-non-escaping allocations bump-allocate
  into a scope arena and bulk-free at scope exit, skipping refcounting entirely.
- **Perceus-style reuse**: when a refcount is provably 1 and the object dies, its
  memory is reused in place (turning a functional-style update into in-place
  mutation), eliding redundant `inc`/`dec` pairs — approaching ownership-level
  performance without a borrow checker.

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

1. **Region-based allocation** for deterministic, efficient systems programming
2. **Stack allocation** with automatic lifetime tracking via scopes
3. **Reference-counted heap** for objects when region binding is impractical — freed
   deterministically at scope exit, with no tracing collector and no pauses
4. **Comprehensive safety guarantees** through bounds checking and null validation
5. **Explicit memory operations** visible to the compiler for optimization
6. **Integrated compilation pipeline** that analyzes and optimizes memory usage

This model offers the performance and determinism of manual memory management
(C/Rust) with the safety of automatic reclamation, while maintaining LLM-friendly
syntax through explicit region declarations and scope-based reference-counting
rules. Reference counting is the heap foundation; arenas and Perceus-style reuse
layer on top as performance stages (see
[memory_model_decision.md](memory_model_decision.md)).

---

*Next Document: Compilation Architecture - Covers the lexer, parser, AST construction, IR generation, optimization phases, and code emission strategies.*

**Version**: 1.1
**Last Updated**: July 14, 2026
