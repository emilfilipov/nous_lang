# Alpha 1 Native Backend Contract

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document records the first native backend contract for Alpha 1. The executable source of truth is `crates/lullaby_ir/src/native_contract.rs`.

## Status

Implemented now:

- A serializable `NativeBackendContract` data model in `lullaby_ir`.
- A deterministic `alpha1_native_backend_contract()` baseline.
- `alpha1_value_layout(TypeRef)` coverage for the current Alpha 1 type surface: `void`, `i64`, `bool`, `string`, `array<T>`, and `ptr_*`.
- Unit tests for target selection, current value layouts, cleanup sequencing, and JSON round-trip stability.
- A checked-in JSON snapshot under `crates/lullaby_ir/tests/snapshots/alpha1_native_backend_contract.json`.
- A first `x86_64-pc-windows-msvc` COFF object emitter in `crates/lullaby_ir/src/native_object.rs` for zero-argument `main` functions that return a literal `i64`, literal `bool`, `void`, stack-backed `i64` local arithmetic, or straight-line `i64` local assignment arithmetic.
- Checked-in object-emission snapshots under `crates/lullaby_ir/tests/snapshots/alpha1_return_42.coff.json`, `crates/lullaby_ir/tests/snapshots/alpha1_locals_add.coff.json`, and `crates/lullaby_ir/tests/snapshots/alpha1_assignments.coff.json`.
- An extended multi-function native program emitter (`emit_alpha1_native_program`) for the **i64-scalar subset** with control flow, calls, division, an entry stub, `ExitProcess` import, and COFF relocations, plus a best-effort `rust-lld` link-to-`.exe` behind the `lullaby native` command. See "Extended Native Program Emission And Link-To-Executable" below.
- **Stack-allocated scalar aggregates** on top of the i64-scalar subset: all-i64 (optionally nested) structs and fixed-length `i64` arrays as function locals, laid out contiguously in the stack frame. See "Stack-Allocated Scalar Structs And Fixed Arrays" below.
- **First heap step: string constants + a bump heap.** String literals used by `len("...")` are emitted into a read-only `.rdata` section, referenced by REL32 relocations, copied into a `.bss` bump-allocated heap region at runtime, and scanned for their byte length so `main` can derive an i64 from string data. See "First Heap Step: String Constants And Bump Allocator" below.

Not implemented yet:

- `f64`/`bool`/`char`/`byte` native scalar lowering and more than four parameters (stack arguments).
- String/enum/collection *values* (as locals, parameters, returns, or call arguments), `match`, and builtins beyond a constant-folded `len` on a fixed native array and `len` over a string literal. A string constant exists only as the immediate argument of `len`; there is no native string local, concatenation, or indexing yet.
- Aggregates (structs/arrays) as function parameters, return values, or call arguments — they are locals only for this increment.
- Growable `list`/`map` and arrays whose length is not known from a literal initializer. The bump heap has no `free`/reclamation and is not yet exposed to general heap allocation beyond the string-constant copy.
- Object file writing for non-COFF targets.
- Native runtime packaging.

## Targets

The first native prototype target is `x86_64-pc-windows-msvc` with COFF object emission. The contract also records the intended 64-bit target family for later work:

- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

All current contract targets are 64-bit little-endian targets.

## Calling Convention

The Alpha 1 backend uses an internal Lullaby ABI before adapting to platform object and linker conventions:

- Parameters lower in source order.
- `main` remains the zero-argument entry function for executable validation.
- Scalar and handle return values are returned directly.
- Variadic calls are not part of Alpha 1.
- Call boundaries require 16-byte alignment.

## Stack Frame

Native lowering must model stable slots for:

- parameters
- locals
- temporaries
- spills
- cleanup records

Cleanup order is driven by `IrMemoryOperation.sequence`, matching the bytecode artifact memory metadata.

## Value Layout

The current Alpha 1 value layouts are:

| Type pattern | Class | Size | Alignment | Pass/return mode |
| :--- | :--- | :--- | :--- | :--- |
| `void` | no payload | 0 bytes | 1 byte | no payload |
| `i64` | integer | 8 bytes | 8 bytes | direct value |
| `bool` | boolean | 1 byte | 1 byte | direct value |
| `string` | runtime handle | 8 bytes | 8 bytes | pointer-sized handle |
| `array<T>` | runtime descriptor handle | 8 bytes | 8 bytes | pointer-sized handle |
| `ptr_*` | heap pointer handle | 8 bytes | 8 bytes | pointer-sized handle |

The contract intentionally treats strings, arrays, and heap pointers as pointer-sized handles. Inline string bytes, array element storage, and heap-slot contents remain runtime-managed.

## Pointer And Array Rules

Safe source operations must not lower null pointer values. Native lowering for `load`, `store`, and `dealloc` must preserve the same live-resource requirements recorded in memory-operation metadata.

Arrays lower as runtime descriptor handles. The descriptor contains a logical `length: i64` and a pointer-sized handle to contiguous element storage. Indexing must preserve bounds-check semantics before element access.

## Cleanup And Diagnostics

Explicit release and future compiler cleanup must share `IrMemoryOperation.sequence` so bytecode and native backends make the same resource-order decisions.

Native backend diagnostics must use the shared `L####` diagnostic model. Target-specific failures must include the target triple.

## Prototype Object Emission

`lullaby_ir::native_object` emits the first prototype COFF object for `x86_64-pc-windows-msvc`. The current emitter deliberately supports a small reviewable native slice:

- source is still validated, lowered to typed IR, and lowered to bytecode before object emission
- the entry function must be zero-argument `main`
- return type must be `void`, `i64`, or `bool`
- the body may be empty `void`, `return <literal>`, a final literal expression, `i64` local bindings, `i64` `=`, `+=`, `-=`, and `*=` assignment, and an `i64` return expression using locals, literals, `+`, `-`, and `*`
- unsupported bytecode returns a structured `NativeObjectError`

For literal `i64`, the prototype emits `mov rax, imm64; ret`. For `bool`, it emits `mov eax, imm32; ret`. For `void`, it emits `ret`. For local `i64` arithmetic and assignment, it emits a frame pointer prologue, 16-byte-aligned stack slots, local loads/stores, arithmetic into `rax`, and a frame epilogue. This single-function prototype (`emit_alpha1_coff_object`) is retained and is still covered by the checked-in `native_object_snapshots` golden files.

## Extended Native Program Emission And Link-To-Executable (DELIVERED, best-effort link)

`lullaby_ir::native_object::emit_alpha1_native_program` extends the prototype into a linkable multi-function COFF object for the **i64-scalar subset** — the native mirror of the WASM backend's scalar subset, restricted to `i64` for this increment. It is exercised by `lullaby native` (see [alpha1_language_surface.md](alpha1_language_surface.md)).

Eligibility and lowering:

- source is still validated, lowered to typed IR, and lowered to bytecode before object emission
- a function is eligible when its parameters and return type are all `i64` and it has at most four parameters (Win64 register arguments `rcx`/`rdx`/`r8`/`r9`; stack arguments are deferred)
- supported bodies: integer literals, params/`let` locals, `+ - * /` (signed `idiv`, dividend sign-extended with `cqo`), comparisons producing `0`/`1`, short-circuiting `and`/`or` and `not`, `if`/`elif`/`else`, `while`, infinite `loop` with `break`/`continue`, range `for` (lowered to an `i64` counter loop mirroring the interpreter's inclusive range and optional step), `return`, a value-producing tail expression, and calls to other compiled i64 functions (including recursion)
- ineligible functions are recorded with a reason and still run on the interpreters; when no i64-scalar function (including `main`) is eligible the emitter returns an error carrying diagnostic code `L0339`

Code generation is a stack-machine model over `rax`: expressions evaluate into `rax`, binary operands spill to the stack with `push`/`pop rcx`, locals and spilled parameters live in `[rbp - slot]` frame slots, and the frame reserves 32 bytes of Win64 shadow space whenever the function issues a call so `rsp` stays 16-byte aligned at each `call`. Inter-function calls, the entry stub's call to `main`, and the entry stub's call to the imported `ExitProcess` are emitted as `call rel32` with `IMAGE_REL_AMD64_REL32` COFF relocations; symbol names longer than eight bytes are stored in the COFF string table. The emitted entry stub `_lullaby_start` calls `main`, moves the `i64` result into `ecx`, and calls `ExitProcess` (imported from `kernel32`), so the process exit code is `main`'s result mod 256.

Link-to-executable is best-effort. The CLI writes the COFF object unconditionally (the reliable floor), then attempts to link with `rust-lld` (`-flavor link`, `/subsystem:console`, `/entry:_lullaby_start`, the object, and `kernel32.lib`), discovering `rust-lld` under the rustc sysroot and library search paths from the MSVC `LIB` environment variable. When `rust-lld` or `kernel32.lib` cannot be located the command reports the object and explains that linking was unavailable rather than failing.

## Stack-Allocated Scalar Structs And Fixed Arrays (DELIVERED)

`emit_alpha1_native_program` additionally lowers **fixed-size aggregates of `i64` laid out on the stack** — no heap, no strings. This extends the eligibility gate: a function using only `i64` scalars, all-`i64` structs, fixed `i64` arrays, and the already-supported control flow/arithmetic/calls is now accepted; a function using strings, heap/growable `list`/`map`, enums, `match`, or floats is still rejected with the existing `L0339` behavior and continues to run on the interpreters.

Supported aggregate features:

- **Struct locals whose fields are all `i64`** (nested all-`i64` structs are allowed). The struct's flattened `i64` fields are laid out contiguously in the frame. Both positional (`Point(3, 4)`) and named (`Point(y: 10, x: 20)`) construction are supported (the IR lowerer already reorders named fields into declared order). Field reads (`p.x`, `l.end.a`) and field mutation (`p.x = ...`, `l.end.a += ...`) resolve the field's stack word and load/store it.
- **Fixed `i64` arrays** (`array<i64>` of a length known from a literal initializer). The array's element words are laid out contiguously. Index reads (`xs[i]`) and index writes (`xs[i] = ...`, `xs[i] += ...`) are supported with both a **constant index** (folded into a static frame displacement) and a **dynamic (runtime `i64`) index** (address computed as `rbp - (base + 8*const_words) - 8*elem_words*index`). Arrays of all-`i64` structs (`array<Cell>`) are supported too.
- `len(arr)` on a fixed native array folds to a compile-time integer constant (native arrays never grow).

Layout and codegen details:

- Each local is a run of 8-byte words: a scalar is one word, a struct the concatenation of its recursively flattened field words, and an array `len` copies of its element layout. Word `k` of a local whose base slot is `slot` lives at `[rbp - (slot + 8*k)]`. Array lengths enter the layout from the `let` initializer (`array<T>` carries no length in its type), so an array local must be initialized by an array literal.
- Aggregates never occupy a register. Scalar operations resolve an individual scalar word to a `[rbp - disp]` place (static, or dynamically address-computed into `rcx` for a runtime index) and load/store it. Struct/array construction and aggregate-to-aggregate copies materialize each flattened scalar word directly into the destination slots.
- **Bounds are not checked** for dynamic array indices in native code for this increment — an out-of-range dynamic index reads or writes an adjacent stack word rather than trapping. The interpreters (AST/IR/bytecode) still bounds-check; a program relying on trapping behavior must run on an interpreter. Trapping native bounds checks are deferred.

Aggregates as function parameters, return values, and call arguments are **deferred** — this increment supports them as locals only. The fixture `tests/fixtures/valid/native_aggregates.lby` (a `Point` struct plus a summed fixed `i64` array, `main` returns 43) is native-compiled, linked, and run by `native_aggregates_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts the `.exe` exit code equals the interpreter's `run` result mod 256 (gated on `rust-lld` + `kernel32.lib`).

## First Heap Step: String Constants And Bump Allocator (DELIVERED)

`emit_alpha1_native_program` takes the first heap step: string-literal constants live in a read-only data section and are copied into a runtime bump heap so a native function can derive an `i64` from string data. This is additive — the interpreters, IR, and bytecode backends are unchanged, and every string-free native program keeps its exact previous single-`.text` COFF layout (the string-free path is byte-for-byte identical, so the existing structural tests are untouched).

Supported native string surface (this increment):

- **`len(string_literal)`** is the one observable string operation. It is *not* constant-folded: the literal's bytes are interned into `.rdata`, `len` bump-allocates a heap copy of them, and scans the heap copy for its terminator to produce the returned `i64`. This exercises the whole first heap step end to end — a read-only constant, a REL32 relocation to its address, a real bump allocation into a writable region, and per-byte reads of both `.rdata` and the heap — and the result is observable through the process exit code. Only ASCII string literals are accepted so that the returned byte length equals the interpreter's char-count `len`; a non-ASCII literal demotes the function to the interpreters. `len` over a fixed native array still folds to a compile-time constant as before.
- There is **no native string value** yet: a string literal is legal only as the immediate argument of `len`. Assigning a string to a local, passing one to a function, returning one, concatenating (`+`), or indexing is still rejected, so such functions run on the interpreters (recorded as skipped; `L0339` if nothing is eligible).

Object layout and codegen (only when a program references string constants):

- The object gains two data sections and two `.text` helpers. `.rdata` (`IMAGE_SCN_CNT_INITIALIZED_DATA | MEM_READ`) holds the NUL-terminated string bytes; each unique literal is interned once (repeated literals dedup) and named `__str{i}`. `.bss` (`IMAGE_SCN_CNT_UNINITIALIZED_DATA | MEM_READ | MEM_WRITE`, `SizeOfRawData` = size, `PointerToRawData` = 0) reserves an 8-byte bump-pointer cell `__lullaby_heap_next` at offset 0 followed by a fixed 64 KiB heap region `__lullaby_heap_base` at offset 8.
- `__lullaby_alloc(size in rcx) -> ptr in rax` is a bump allocator: it reads `__lullaby_heap_next`, lazily seeds it to `&__lullaby_heap_base` on first use (a zero pointer means "uninitialized"), returns the current pointer, and advances it past an 8-byte-rounded allocation. It has **no `free`/reclamation** — allocations are never reclaimed for this increment.
- `__lullaby_strlen_copy(src in rcx) -> len in rax` measures the `.rdata` source length, calls `__lullaby_alloc` for `n + 1` bytes, `rep movsb`-copies the string (with terminator) into the heap, and scans the heap copy for its length. It saves/restores the non-volatile `rsi`/`rdi`/`rbx` and keeps `rsp` 16-aligned at the internal `call`.
- A `len(string_literal)` call site lowers to `lea rcx, [rip + __str{i}]` (a REL32 relocation to the `.rdata` symbol) followed by `call __lullaby_strlen_copy` (a REL32 relocation to the helper). Cross-section references reuse the existing `IMAGE_REL_AMD64_REL32` machinery; the `.rdata`/`.bss` data symbols carry COFF type `0` (not the function type `0x20`).
- The fixture `tests/fixtures/valid/native_strings.lby` (`main` returns `len("hello") + len("native") + len("") = 11`) runs on all backends for ground truth and is native-compiled, linked, and run by `native_strings_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts the `.exe` exit code equals the interpreter's `run` result mod 256 (gated on `rust-lld` + `kernel32.lib`).

## Deferred Native Work

Deferred beyond this increment: `f64`/`bool`/`char`/`byte` scalar lowering, more than four parameters (stack arguments), aggregates as parameters/returns/call arguments, trapping native array bounds checks, string/enum/collection *values* (native string locals, concatenation, indexing, comparison), heap allocation exposed beyond the string-constant copy, a heap `free`/reclamation path, `match`, builtins beyond a constant-folded array `len` and string-literal `len`, cross-platform ELF/Mach-O object emission, and CRT-driven `mainCRTStartup` entry. This work must not bypass the AST runtime, typed IR validation, bytecode VM, or existing release verification.
