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
- **C-ABI FFI (calling C).** A body-less `extern fn NAME params -> Ret` declares an imported C function; a call lowers to a `call` of an undefined external symbol and links against the C runtime (`ucrt.lib`). Each scalar argument is routed to the register selected by its **position and type** (integer/pointer → `rcx`/`rdx`/`r8`/`r9`; `f32`/`f64` → `xmm0..3`); the return is read from `rax` (integer, narrow-normalized) or `xmm0` (float). This completes the scalar C-ABI story (all integer widths + `bool`/`char`/`byte` + `f32`/`f64`). Calling an extern on an interpreter is `L0423`. See "C-ABI FFI (calling C)" below.
- **C-ABI FFI (exposing Lullaby to C).** An `export fn NAME params -> Ret` is a normal (bodied) Lullaby function additionally exposed under its plain C name as an externally visible, defined `.text` symbol so C (or another object) can call **into** Lullaby. An export must have a C-marshallable scalar signature from the delivered set `i64`/`f64`/`f32` (`L0424` otherwise); a float parameter arrives in its positional SSE register and a float return leaves in `xmm0`. An export-only program (no `main`) emits a library object with no entry stub. See "C-ABI FFI (exposing Lullaby to C)" below.
- **Native source-line debug info (`--debug`).** `lullaby native --debug` (alias `-g`) emits a CodeView `.debug$S` section with a per-function line table mapping each compiled function's entry offset to its `.lby` declaration line, plus the source file name, so a debugger (via a linker-built PDB) can break at a function and show its source line. Opt-in: without `--debug` the object bytes are byte-for-byte unchanged. See "Debug info (`--debug`)" below.
- **Stack-allocated enums + `match` (scalar payloads).** Enum values whose payloads are native scalars — the built-in generics `option<T>`/`result<T, E>` (for scalar `T`/`E`) and user enums — are laid out on the stack as a tag word plus a shared payload region, and `match` dispatches on the tag with identical variant ordering to the interpreters. See "Stack-Allocated Enums And `match`" below.
- **Scalar-field aggregates across function boundaries.** Scalar-field structs, fixed arrays of scalars, and scalar-payload enums are now valid **parameters, return values, and call arguments** (not just locals), using a by-hidden-pointer ABI with copy-in value semantics. A function taking, returning, and mutating such an aggregate is compiled natively and agrees bit-for-bit with the interpreters. See "Scalar-Field Aggregates Across Function Boundaries" below.
- **Growable `list<T>` (scalar element).** A heap-backed, capacity-doubling growable list with a scalar element type is compiled to native machine code: `list_new`, `push`, `get`, `set`, `len`, and `pop`, with value semantics (`push`/`set`/`pop` return a new list) and lists crossing function boundaries. This brings the native backend to parity with the WASM backend's growable-list support. See "Growable `list<T>` (scalar element)" below.
- **Growable `map<K, V>` (scalar key/value).** A heap-backed, capacity-doubling, insertion-ordered association map with an integer-cell key and a scalar value is compiled to native machine code: `map_new`, `map_set`, `map_get`, `map_has`, and `map_len`, with value semantics (`map_set` returns a new map) and maps crossing function boundaries. `map_get` returns `option<V>` reusing the native enum/option layout. This brings the native backend to parity with the WASM backend's first map increment. See "Growable `map<K, V>` (scalar key/value)" below.

Now implemented (updated): `bool`/`char`/`byte` and the full fixed-width integer
lattice (`i8`…`u64`, `isize`/`usize`) and `f64`/`f32` floats are lowered as native
scalars within `i64`-signature functions — wrapping/normalized integer
arithmetic, signedness-correct comparison and division, bitwise and shifts, the
`to_<T>`/`to_f32`/`to_f64` conversions, and SSE/XMM float arithmetic/comparison.
Control flow (`if`/`while`/`loop`/`for`) and inter-function calls compile.
`extern fn` calls to C are compiled and linked for the **full scalar subset**:
all integer widths plus `bool`/`char`/`byte` (Win64 GPRs) and `f32`/`f64`
(SSE/XMM), including a top-level float scalar as an `extern`/`export`
parameter/return and a mixed float/int signature. `export fn` marshals the scalar
set `i64`/`f64`/`f32`.

Not implemented yet:

- More than four parameters (stack/XMM spill for a fifth-plus argument).
- String/collection *values* (as locals, parameters, returns, or call arguments) and builtins beyond a constant-folded `len` on a fixed native array and `len` over a string literal. A string constant exists only as the immediate argument of `len`; there is no native string local, concatenation, or indexing yet. **Enum values with scalar payloads and `match` over them are now delivered** (see below); enums whose payload is a heap type (`string`, `list`, `array`, another struct/enum) are still deferred.
- **Now delivered:** scalar-field aggregates (structs, fixed arrays of scalars, scalar-payload enums) as function **parameters, return values, and call arguments** — by a hidden-pointer ABI with copy-in value semantics. A function that returns an enum, and a `match` on the result of such a call, now compile. See "Scalar-Field Aggregates Across Function Boundaries". A top-level `f64`/`f32` **scalar** parameter/return is now delivered (routed through the SSE registers `xmm0..3`, positionally aligned with the integer registers, with the float return in `xmm0`). Still deferred at the aggregate boundary: aggregates containing heap values (`string`/`list`/`map`, or a heap element/field), and any signature whose effective register arguments (parameters plus a hidden return pointer) exceed four.
- **Now delivered:** growable **`list<T>` with a scalar element** (`i64`/fixed-width/`bool`/`char`/`byte`/`f32`/`f64`) — `list_new`, `push`, `get`, `set`, `len`, and `pop`, with value semantics and lists crossing function boundaries. See "Growable `list<T>` (scalar element)" below. Still deferred: lists of heap elements (`list<string>`/`list<struct>`/`list<list<…>>`/`list<map<…>>`), and arrays whose length is not known from a literal initializer. The bump heap still has no `free`/reclamation (a grown or copied list orphans its old block).
- **Now delivered:** growable **`map<K, V>` with an integer-cell key and a scalar value** — `map_new`, `map_set`, `map_get` (returning `option<V>`), `map_has`, and `map_len`, with insertion-ordered value semantics and maps crossing function boundaries. See "Growable `map<K, V>` (scalar key/value)" below. Still deferred: **string/heap keys and heap values** (`map<string, V>`, `map<K, string>`, `map<K, list<…>>`, …) — string-key equality needs the string heap for content comparison, matching the WASM map's first increment — **float keys** (their bit-pattern word compare would diverge from the interpreters' value equality on `±0.0`/NaN; float *values* are fine, stored/loaded bit-for-bit), and `map_keys`/`map_values`/`map_del`.
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
- supported bodies: integer literals, params/`let` locals, `+ - * /` (signed `idiv`, dividend sign-extended with `cqo`; the `i64::MIN / -1` overflow case is guarded so it wraps to `i64::MIN` — matching the interpreters' `wrapping_div` — instead of raising a hardware `#DE`), comparisons producing `0`/`1`, short-circuiting `and`/`or` and `not`, `if`/`elif`/`else`, `while`, infinite `loop` with `break`/`continue`, range `for` (lowered to an `i64` counter loop mirroring the interpreter's inclusive range and optional step), `return`, a value-producing tail expression, and calls to other compiled i64 functions (including recursion)
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

Aggregates as function parameters, return values, and call arguments are **now delivered** — see "Scalar-Field Aggregates Across Function Boundaries" for the by-pointer ABI. This section describes the aggregate **locals** layout the boundary ABI reuses. The fixture `tests/fixtures/valid/native_aggregates.lby` (a `Point` struct plus a summed fixed `i64` array, `main` returns 43) is native-compiled, linked, and run by `native_aggregates_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts the `.exe` exit code equals the interpreter's `run` result mod 256 (gated on `rust-lld` + `kernel32.lib`).

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

## Stack-Allocated Enums And `match` (DELIVERED)

`emit_alpha1_native_program` lowers **enum values with scalar payloads** and **`match`** over them. This covers the built-in generic enums `option<T>` and `result<T, E>` (when `T`/`E` are native scalars) and **user enums** whose variant payloads are all native scalars. It is additive: a function using only these plus the already-supported i64-scalar/aggregate/control-flow subset now compiles; anything outside it (a heap-payload enum, an enum parameter/return, or a `match` on an enum-returning call) is skipped with a clear reason and runs on the interpreters — never miscompiled.

### Enum layout

An enum local occupies a **tag word** followed by a shared **payload region** sized to the widest variant:

- **Word 0 is the discriminant tag** — the variant's index in the interpreter/IR variant order. For a **user enum** this is declared order; for the built-ins it is `some`(0)/`none`(1) and `ok`(0)/`err`(1). Because the interpreters select a `match` arm by variant *name*, any consistent tag numbering is correct; the native backend fixes the numbering to the declared/built-in order so construction and dispatch always agree.
- **Payload words follow at word 1, 2, …** in field order. The payload region is `max` over the variants of that variant's total scalar payload words (each scalar payload field is one 8-byte word, like a struct field). A narrower variant leaves the trailing payload words untouched; `match` only reads the words the *matched* variant defines, so stale bytes are never observed.
- A scalar payload word is an `i64`/fixed-width/`bool`/`char`/`byte` cell (stored through a GPR) or an `f32`/`f64` (stored through XMM), matching how scalar locals are stored elsewhere.

### Construction

`some(x)`, `none`, `ok(x)`, `err(e)`, and a user `Variant(payload…)` materialize directly into the local's words: `mov` the variant's tag into word 0, then evaluate each payload expression into its payload word (GPR for a scalar cell, XMM for a float). Unit variants (`none`, a payload-less user variant) write only the tag. A whole-value reassignment (`s = Pulse(n)`) re-materializes tag + payload in place.

### `match`

- The scrutinee is materialized into a stack region: a plain enum **local** is matched in place; a **freshly-constructed** enum scrutinee is spilled into a scratch region reserved during frame planning (one shared region sized to the widest temporary enum scrutinee in the function).
- Dispatch reloads the tag word and, per variant arm, emits `cmp rax, tag` + `jne next_arm`; the matched arm **binds the variant's payload words** into arm-scoped locals (a `load`/`store` per scalar word, GPR or XMM by width), lowers the arm body, then `jmp`s to the shared match end. A `_` **wildcard** arm binds nothing and is an unconditional fall-in (it is terminal, so no later arm is emitted).
- A wildcard-free match is exhaustive (semantics guarantee it), so the impossible fallthrough after the last variant arm emits a `ud2` trap rather than running off the end.
- A value-producing `match` (its arms yield the function's result) leaves each arm's value in `rax`; the caller emits the return epilogue after the shared end. A void `match` is a statement whose arm results are discarded.

### Deferred enum work

- **Heap payloads** — an enum whose payload is `string`, `list`, `array`, or another struct/enum (notably `result<i64, string>`, whose `err` carries a string) is out of the scalar subset and skips gracefully.
- **Enums as parameters, returns, and call arguments are now delivered** — a scalar-payload enum crosses a function boundary by the aggregate ABI (see "Scalar-Field Aggregates Across Function Boundaries"). A function returning an enum, and a `match` on the result of such a call (e.g. matching `lookup(k)` where `lookup -> option<i64>`), now compile. Matching a **local** enum or a **freshly-constructed** enum remains fully supported. A `match` on an enum-returning call whose *arguments* are themselves aggregates is deferred (the shared scratch region would need the scrutinee and argument copies simultaneously).
- **Nested enum values inside struct/array fields** are deferred.

### Verification

The fixtures `tests/fixtures/valid/native_enum_option.lby` (`option<i64>`, some=49-path), `native_enum_result.lby` (`result<i64, i64>`, ok/err scalar), and `native_enum_user.lby` (a user `Signal` enum with a scalar payload and a `_` arm) run on all interpreter backends for ground truth and are native-compiled, linked, and run by `native_enum_match_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts each `.exe` exit code equals the interpreter's `run` result mod 256 (gated on `rust-lld` + `kernel32.lib`). `tests/fixtures/valid/native_enum_returned.lby` (an `option<i64>`-returning `lookup` and a `match lookup(k)`) now native-compiles under the aggregate ABI. Unit tests in `native_program_tests` assert the option/result/user-enum functions report `compiled` (not `skipped`) and that the expected opcodes appear (tag load + `cmp`/`jne` dispatch, and a `ud2` for the wildcard-free exhaustive case), plus that heap-payload cases skip with clear reasons.

## Scalar-Field Aggregates Across Function Boundaries (DELIVERED)

`emit_alpha1_native_program` passes, returns, and mutates **scalar-field aggregates** — scalar-field structs (nested allowed), fixed arrays of scalars, and scalar-payload enums (`option`/`result`/user enums) — across native function boundaries, not just as locals. It preserves Lullaby's **value semantics** (an aggregate parameter/return/argument is an independent snapshot, exactly like the interpreters) and agrees bit-for-bit with the AST/IR/bytecode backends.

### ABI: by hidden pointer, copy-in value semantics

The aggregate boundary uses an internal by-pointer convention (all callers and callees are Lullaby, so it need not match the Win64 small-struct-in-register rule, only be self-consistent):

- **Aggregate parameter.** The caller materializes the aggregate into a caller-owned copy (in its frame or a scratch temp) and passes a **pointer** to it in the parameter's integer register (`rcx`/`rdx`/`r8`/`r9`). In its prologue the callee **copies the words in** to its own frame slots (`mov rcx, [rax - 8*k]` / `mov [rbp - slot], rcx`), so mutating the parameter never touches the caller's copy.
- **Aggregate return.** The caller allocates space for the result and passes its address as an implicit **hidden first argument** (`rcx`), shifting the visible parameters to the following registers. The callee writes the result words through that pointer (`mov [rax - 8*k], rcx`) and returns the pointer in `rax`. `main`'s scalar `i64` return path is unchanged (no hidden pointer).
- **Aggregate call argument.** The caller materializes a fresh copy in a scratch region, `lea`s its address, and passes that pointer per the parameter rule.
- **Word layout matches locals.** Aggregate words descend in memory (word `k` at the lower address `[base - 8*k]` from the word-0 pointer), matching the existing local layout, so field/element order and offsets are identical to the locals implementation.

### Arity and deferral

An aggregate return consumes one integer register for the hidden result pointer, so the number of **effective register arguments** (parameters plus a hidden return pointer, if any) must be at most four; a signature that would exceed four skips gracefully. **Deferred / skipped:** aggregates containing heap values (`string`/`list`/`map`, or a heap element/field), a top-level `f64`/`f32` **scalar** parameter/return (needs XMM argument routing — float payloads *inside* a by-pointer aggregate are fine, copied as raw bit-preserving words), and arity overflow. None of these miscompile; they run on the interpreters.

### Array-length inference

A fixed array carries no length in its `array<T>` type, so an array-typed parameter or return has its length inferred: a **returned** array's length comes from the function's returned array value (an array literal or a fixed-array local); a **parameter** array's length comes from every call site's argument in that position, which must all agree. A length that cannot be determined or that disagrees across callers demotes the function to the interpreters rather than guessing.

### Verification

The fixtures `tests/fixtures/valid/native_aggregate_params.lby` (a function taking a struct and returning an i64, one returning a struct, and a value-semantics check via `mutate_local`), `native_aggregate_array.lby` (taking and returning a fixed `i64` array plus a value-semantics check), `native_aggregate_enum.lby` (`option<i64>` as parameter and return, including a `match` on an enum-returning call), and `native_aggregate_value_semantics.lby` (a struct and an array whose callees clobber their parameter copies) each return a deterministic i64 < 256, run on all interpreter backends for ground truth, and are native-compiled, linked, and run by `native_aggregate_boundary_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, asserting the `.exe` exit code equals the interpreter's `run` result (gated on `rust-lld` + `kernel32.lib`; the compile-not-skip and interpreter-truth assertions always run). Unit tests in `native_program_tests` assert the struct/array/enum boundary functions report `compiled` (not `skipped`), that the hidden-return-pointer write (`mov [rax - 8], rcx`), the by-pointer argument `lea`, and the parameter copy-in read (`mov rcx, [rax + disp]`) appear in the code, and that heap-containing aggregates and arity-overflow signatures skip with clear reasons.

## C-ABI FFI (calling C) (DELIVERED, first increment)

The native backend can call C functions across the Win64 C ABI. This is the first FFI increment — calling *from* Lullaby *into* C.

### The `extern` surface

A body-less `extern fn` declares a C function imported at link time:

```lullaby
extern fn llabs x i64 -> i64

fn main -> i64
    llabs(-7)
```

Rules:

- `extern` prefixes a `fn` declaration (an optional `pub` may precede it: `pub extern fn`); `extern` and `async` cannot be combined. The declaration has **no indented body** — it ends after the return type.
- The Lullaby function name **is** the C symbol name. Calling it emits a call to that external symbol.
- Calls are type-checked exactly like ordinary calls (arity, argument types, return type) using the registered signature. An extern name that is not a built-in resolves through the ordinary user-function call path, so the builtin catalog is unaffected (choose an extern name that is not a Lullaby builtin — e.g. `llabs`, not `abs`).
- Marshalling: an extern's parameters and return type may be any **scalar C type** — every fixed-width integer (`i8`…`u64`, `isize`/`usize`) plus `bool` (`_Bool`, 0/1), `char` (`uint32_t`), `byte` (`uint8_t`), and the floats `f32` (`float`) / `f64` (`double`). Up to four arguments are routed by **position and type**: an integer/pointer at position N passes in the low bits of integer register N (`rcx`/`rdx`/`r8`/`r9`), a float at position N passes in `xmm N` — each position consuming its slot in exactly one register sequence (so `ldexp(double, int)` sends `double`→`xmm0`, `int`→`rdx`). The result is read from `rax` (integer, with a **narrow C return re-normalized in `rax`** — sign-extend for signed kinds, zero-extend for unsigned; `i64`/64-bit kinds a no-op) or from `xmm0` (a `f32`/`f64` return, `movsd`/`movss`), so downstream Lullaby code sees the same cell/value the interpreters produce. This completes the scalar C-ABI story. The extern C-ABI signatures are carried through the IR/bytecode as `extern_signatures` (serde-defaulted for artifact compatibility) so the native emitter can marshal each width.

### Win64 mapping and codegen

- A call to an `extern fn` stages its arguments left-to-right on the machine stack, then loads each staged word into the register chosen by its position and class — integer/pointer → `rcx`/`rdx`/`r8`/`r9` (`mov r64, [rsp+disp]`), float → `xmm0..3` (`movsd xmm, [rsp+disp]`) — before the `call`. Staging first, then loading, keeps one argument's evaluation from clobbering an already-loaded register. The caller already reserves 32 bytes of shadow space and keeps `rsp` 16-byte aligned at the `call`, exactly like inter-function calls.
- The call site emits `call rel32` with an `IMAGE_REL_AMD64_REL32` relocation against the C symbol. The COFF symbol table gains that name as an **undefined external symbol** (section number 0), reusing the exact mechanism that imports `ExitProcess` from kernel32. The result is read from `rax` (integer) or `xmm0` (a `f32`/`f64` return).
- `extern_functions` on `IrModule`/`BytecodeModule` carries the extern names, and `extern_signatures` carries each extern's ordered parameter types and return type (both serde-defaulted for artifact compatibility). The native emitter adds the names to the callable set, marshals each call per its signature's scalar widths, and the object writer materializes any unresolved relocation target as an undefined external.

### Linking

- When a program declares any `extern fn`, `emit_alpha1_native_program` reports the required C runtime import library (`ucrt.lib`) in `NativeProgram.import_libs`.
- The `lullaby native` link step discovers `ucrt.lib` the same way it discovers `kernel32.lib` — via the MSVC `LIB` environment variable (a Developer Command Prompt) — and passes it to `rust-lld` alongside `kernel32.lib`. If `rust-lld` or any required import library cannot be located, linking degrades gracefully: the object is written and an explanation is printed, exactly like the existing native path.
- The deterministic FFI fixture `tests/fixtures/native_only/ffi_llabs.lby` declares `extern fn llabs x i64 -> i64` and returns `llabs(-7)`; native-linked against `ucrt.lib`, the `.exe` calls the real C `llabs` and exits with code `7`. It lives under `tests/fixtures/native_only/` (not the auto-discovered parity directory) because it cannot run on the interpreters. `ffi_calls_c_abs_when_linkable` in `crates/lullaby_cli/tests/cli.rs` checks it, asserts `L0423` on every interpreter backend, and — when `rust-lld` + `kernel32.lib` + `ucrt.lib` are available — links and runs it asserting exit code 7.
- The non-`i64`-width FFI fixture `tests/fixtures/native_only/ffi_toupper.lby` declares `extern fn toupper c i32 -> i32` and returns `to_i64(toupper(to_i32(97)))`; `toupper('a')` is `'A'` (65), so the linked `.exe` exits with code `65`. `ffi_calls_c_toupper_i32_when_linkable` in `crates/lullaby_cli/tests/cli.rs` checks it exactly like the `llabs` test (`L0423` on every interpreter; link+run when the toolchain is present), exercising the `i32` argument (low bits of `rcx`) and the `i32` return re-normalization (`movsxd rax, eax`).
- The **float** FFI fixture `tests/fixtures/native_only/ffi_sqrt.lby` declares `extern fn sqrt x f64 -> f64`, computes `sqrt(16.0)` (== 4.0), and derives a deterministic `i64` via two float comparisons so the linked `.exe` exits `7`. The **mixed float/int** fixture `tests/fixtures/native_only/ffi_ldexp.lby` declares `extern fn ldexp x f64 e i32 -> f64`, computes `ldexp(1.5, 3)` (== 12.0), and exits `12` — verifying positional routing (`f64`→`xmm0`, `i32`→`rdx`, return in `xmm0`) against the real C runtime. `ffi_calls_c_sqrt_f64_when_linkable` and `ffi_calls_c_ldexp_mixed_scalars_when_linkable` in `crates/lullaby_cli/tests/cli.rs` check them like the integer tests; they source the MSVC library directories when `LIB` is unset so the link+run executes.
- Native unit tests `emits_i32_extern_call_with_import_and_return_normalization` and `emits_u8_extern_call_with_zero_extend_return_normalization` in `crates/lullaby_ir/src/native_object.rs` assert the external symbol and the exact return-normalization opcodes. `float_extern_arg_and_return_route_through_xmm0`, `mixed_float_then_int_extern_routes_xmm0_and_rdx`, and `int_then_float_extern_routes_rcx_and_xmm1` assert a float argument loads into its positional XMM register, a mixed signature routes each argument to the right register class, and the float return is consumed from `xmm0`. `function_with_float_signature_compiles_natively` and `export_fn_with_float_params_spills_from_xmm` assert a top-level float parameter is spilled from its positional SSE register. `rejects_extern_call_on_the_ast_interpreter` in `crates/lullaby_runtime/src/lib.rs` asserts an `i32` extern call raises `L0423` on the AST interpreter.

### Interpreter restriction and diagnostic

The AST, IR, and bytecode interpreters cannot execute real C FFI. Calling an `extern fn` on any of them raises diagnostic **`L0423`** (runtime) rather than panicking or silently no-op-ing:

> cannot call extern (C-ABI) function `NAME` on an interpreter; compile with `lullaby native` to link and run it

`lullaby check` still validates extern declarations and their call sites, so type errors at extern call sites are caught statically.

### Deferred FFI work

The integer-class C scalars (`i8`…`u64`, `isize`/`usize`, `bool`, `char`, `byte`) are now marshalled for **calling** C via `extern fn`; `i32`/`int` and `long`-width bindings therefore work. Deferred beyond this increment: `f32`/`f64` (floating) C parameter/return types (they need XMM argument routing), non-scalar/pointer/struct C parameter and return types, variadic C functions, callbacks and function-pointer arguments, string/buffer marshalling, and non-Windows C ABIs (System V on Linux/macOS). Exposing Lullaby functions **to** C is delivered for the i64-scalar case (widening the `export` marshalling to the same integer widths is a follow-up) — see below.

## C-ABI FFI (exposing Lullaby to C) (DELIVERED, first increment)

The native backend can expose a Lullaby function to C across the Win64 C ABI — the other FFI direction, C (or another object) calling **into** Lullaby.

### The `export` surface

An `export fn` marks a normal (bodied) Lullaby function as C-callable:

```lullaby
export fn add_seven x i64 -> i64
    x + 7
```

Rules:

- `export` prefixes a `fn` declaration (an optional `pub` may precede it: `pub export fn`). Unlike `extern`, an export **has a body** and is a full Lullaby function; `export` is mutually exclusive with `extern` (which imports a body-less C symbol) and with `async`.
- The Lullaby function name **is** the exported C symbol name. A C caller declares `extern <ret> NAME(<args>);` and links against the emitted object.
- Parameters and the return type must be in the delivered C-marshallable scalar set `i64`/`f64`/`f32` (`i64` in the integer register, `f64`/`f32` in the SSE registers). A non-scalar or generic export signature is rejected at check time with **`L0424`** rather than silently demoted.
- `export` is meaningful only to native codegen. On the AST/IR/bytecode interpreters an `export fn` runs exactly like an ordinary `fn`; the interpreters, IR, and bytecode execution are unchanged. `export_functions` on `IrModule`/`BytecodeModule` carries the export names (serde-defaulted for artifact compatibility).

### Win64 mapping and COFF symbol visibility

- An exported function is lowered like any native scalar function (standard Win64 prologue/epilogue). An integer parameter arrives in `rcx`/`rdx`/`r8`/`r9` and an integer result leaves in `rax`; a **float** parameter arrives in its positional SSE register (`xmm0..3`, spilled to its slot with `movsd`/`movss`) and a float result leaves in `xmm0`. No wrapper or thunk is generated — the plain Lullaby body *is* the C entry point.
- The COFF symbol table already emits every compiled function as storage-class `EXTERNAL` **defined** in `.text` (section number 1) under its plain, unmangled name. An export therefore surfaces to the linker exactly as `extern <ret> NAME(<args>);` expects. (`export` records user intent and gates the scalar-signature check; the symbol mechanism is the existing external-defined function symbol.)
- **Library objects.** An export-only program (no `main`) emits a *library object*: the emitter omits the `_lullaby_start` entry stub and its `ExitProcess` dependency entirely, so the object carries only the exported (and any called) function symbols. This lets a C `main` link against it without colliding on `main` or dragging in a second entry point. A program that has both `main` and exports keeps the entry stub (a runnable program) and additionally exposes the exports. `NativeProgram.entry_symbol` is empty for a library object; `lullaby native` then writes the object and reports it as a C-callable library rather than attempting to link an `.exe`.

### Testing

- **Structural (always runs):** `exports_function_as_external_defined_text_symbol` in `crates/lullaby_ir/src/native_object.rs` emits a native program for `export fn add_seven x i64 -> i64` and asserts the COFF symbol table holds `add_seven` as an `EXTERNAL` (storage class 2) symbol defined in `.text` (section 1), with no `_lullaby_start`/`ExitProcess` in the library object. `export_alongside_main_keeps_the_entry_stub` checks the mixed case.
- **Execution (gated, skips gracefully):** `c_calls_into_exported_lullaby_function_when_compilable` in `crates/lullaby_cli/tests/cli.rs` native-compiles `tests/fixtures/native_only/export_add_seven.lby` to a library object, then — if a C compiler (`cl` or `clang`) is discoverable — compiles a tiny C program (`extern long long add_seven(long long); int main(void){ return (int)add_seven(35); }`), links it against the Lullaby object, runs the result, and asserts exit code `42`. It skips the compile+link+run with a message when no C compiler is found.

### Deferred export work

Deferred beyond this increment: the sub-`i64` integer widths and `bool`/`char`/`byte` as export parameter/return types (the extern-call direction already marshals them; widening the export gate to match is a follow-up), pointer/struct export types, callbacks (a Lullaby function value handed to C as a function pointer), string/buffer marshalling across the boundary, and non-Windows C ABIs.

## Inline Assembly (raw byte emission) (DELIVERED, first increment)

The native backend can emit raw x86-64 machine-code bytes verbatim into a function's `.text`. This is the first inline-assembly increment: a trusted, native-only escape hatch, gated by `unsafe`.

### The `asm` surface

An `asm` statement takes a comma-separated list of integer byte literals (each an `i64` in `0..=255`) and emits those bytes verbatim into the current function's `.text` at that point:

```lullaby
fn main -> i64
    unsafe
        asm 72, 199, 192, 42, 0, 0, 0
```

The seven bytes above are `48 C7 C0 2A 00 00 00` = `mov rax, 42`. Because the Win64 epilogue returns `rax`, a trailing `asm` that leaves a value in `rax` makes the function return it — observable through the process exit code (this program exits `42`). The bytes are emitted as-is: they are not decoded, relocated, register-modeled, or validated beyond the range check.

### Unsafe gating and shape validation

- `asm` is inherently `unsafe`, so it must appear inside an `unsafe` block, exactly like the raw-pointer builtins `ptr_read`/`ptr_write`. Using it outside `unsafe` is `L0330` ("`asm` inline assembly requires an `unsafe` block").
- `lullaby check` still validates the construct's shape: the statement must emit at least one byte and every byte must be in `0..=255`. An empty or out-of-range `asm` is `L0425` at check time.
- A trailing `asm` is treated as divergent-like (as `throw` is), so it satisfies a non-void function's final-value requirement: the programmer is trusted to leave the return value in `rax`.

### Native-only behavior and codegen

- **Native-only.** Like `extern`, the AST, IR, and bytecode interpreters cannot execute raw machine code, so any `asm` statement is rejected at runtime with diagnostic **`L0425`** ("cannot execute an `asm` (inline assembly) statement on an interpreter; compile with `lullaby native` to emit and link the machine code"). It runs only after native codegen + linking.
- **Codegen.** `emit_alpha1_native_program` copies the `asm` bytes verbatim into the function's `.text` at the statement's position. When `asm` is a function's last statement, the emitter emits it followed by the normal Win64 epilogue (restore `rsp`/`rbp`, `ret`) so `rax` is returned intact rather than clobbered by the fallthrough `xor eax,eax`; the programmer must therefore not emit their own `ret`. A non-tail `asm` is emitted inline between the surrounding statements.

### Testing

- The native-only fixture `tests/fixtures/native_only/asm_mov.lby` (`main`'s `unsafe` `asm` emits `mov rax, 42`) lives outside `tests/fixtures/valid/` because it cannot run on the interpreters (it would break the cross-backend parity harness). `asm_emits_raw_bytes_when_linkable` in `crates/lullaby_cli/tests/cli.rs` checks it, asserts `L0425` on every interpreter backend, and — when `rust-lld` + `kernel32.lib` are available — native-links and runs it, asserting exit code `42`; it skips the link+run gracefully otherwise.
- `asm_bytes_are_emitted_verbatim_into_text` in `crates/lullaby_ir/src/native_object.rs` asserts the exact `mov rax, 42` byte pattern appears in the emitted object. Semantics/runtime unit tests cover the `unsafe` gate, the byte-range check, and the interpreter `L0425` rejection.

### Deferred inline-assembly work

Deferred beyond this increment: no register/clobber modeling, no operand substitution (no way to reference Lullaby locals/values from the bytes), no assembly-text parsing (bytes only, not mnemonics), no verification that the bytes form valid instructions or preserve the frame, no relocation of the bytes, and non-Windows/non-x86-64 targets.

## Freestanding / no-std mode (`--freestanding`) (DELIVERED, first increment)

`lullaby native --freestanding` (alias `--no-std`) builds a native executable with **no C-runtime dependency**. This is the first freestanding increment: it formalizes, guarantees, and tests what the default native path already does — the emitted `.exe` links `kernel32.lib` only, zero CRT libraries (`ucrt`/`vcruntime`/`msvcrt`).

### The no-CRT guarantee and minimal-OS-import model

- The default native path is already CRT-free for a program with no `extern fn`: the entry stub `_lullaby_start` bypasses the C runtime (`/entry:_lullaby_start`, not `mainCRTStartup`), calls `main`, moves the result into `ecx`, and terminates through the imported `ExitProcess`. The only linked import library is `kernel32.lib`.
- `--freestanding` turns that into a checked contract. The emitted executable's **only** OS dependency is `kernel32!ExitProcess` — the minimal import needed to terminate the process. No CRT startup, no `ucrt`/`vcruntime`/`msvcrt`, and no CRT-provided `main` entry.
- What "freestanding" means **here**: this is still a Windows PE that uses `kernel32` for process exit. `ExitProcess`-via-`kernel32` is an accepted minimal-OS dependency for this first increment, not a bare-metal, OS-free binary.
- Combined with inline `asm`, a freestanding `main` can even terminate by leaving its value in `rax` and returning it through the CRT-free entry stub to `ExitProcess` (see `tests/fixtures/native_only/asm_mov.lby`, which exits `42`). The stub — not the C runtime — provides the process lifecycle.

### The extern-conflict diagnostic

An `extern fn` call requires the C runtime import library `ucrt.lib` (it is how the external C symbol resolves), which contradicts the no-CRT guarantee. `lullaby native --freestanding` on a program that declares any `extern fn` is therefore rejected at the CLI with diagnostic **`L0426`** rather than silently linking the CRT:

> freestanding (`--freestanding`) native build cannot depend on the C runtime, but this program requires the C runtime import library `ucrt.lib` (via an `extern fn`)

Non-freestanding `native` is unchanged: it still links `ucrt.lib` for `extern fn` calls. `export fn` (C-callable exports), stack aggregates, and the string-constant heap step are all CRT-free and remain allowed in freestanding mode. The default (non-freestanding) `native` behavior is byte-for-byte unchanged — `--freestanding` only adds the CRT-dependency check and a status notice; it does not alter code generation or the object bytes.

### Testing

- `native_freestanding_has_no_crt_dependency_when_linkable` in `crates/lullaby_cli/tests/cli.rs` native-compiles `tests/fixtures/valid/native_scalars.lby` with `--freestanding` and asserts (always) that the emitted object contains **no** C-runtime marker (`ucrt`/`vcruntime`/`msvcrt`/`api-ms-win-crt`) while still importing `kernel32!ExitProcess`; when `rust-lld` + `kernel32.lib` are available it additionally asserts the linked image carries no CRT DLL import and that its exit code equals the interpreter's `main` result (`39`) mod 256. It skips only the link+run when the toolchain is unavailable — the object-level no-CRT assertion always runs.
- `native_freestanding_rejects_extern_fn_with_l0426` asserts `--freestanding` on `tests/fixtures/native_only/ffi_llabs.lby` (an `extern fn` program) fails with `L0426` and names `ucrt.lib`.

### Deferred toward true bare-metal

This increment is a verifiable no-CRT Windows PE, not a bare-metal port. True bare-metal / other-OS support is deferred: a raw-syscall (or other OS-primitive) process exit with **no OS imports at all** (no `kernel32`); ELF / Mach-O / raw-binary object formats instead of Windows COFF/PE; a custom entry point and linker script; freestanding for non-x86-64 targets; and any freestanding intrinsics (no libc, no allocator beyond the existing bump heap). None of this bypasses the AST runtime, typed IR validation, bytecode VM, or existing release verification.

## Debug info (`--debug`) (DELIVERED, first increment)

`lullaby native --debug` (alias `-g`) emits native **source-line debug info** so a debugger can map native code addresses back to `.lby` source lines. This is the first debug-info increment: a per-function line table plus the source file name, in CodeView, gated behind `--debug`.

### Format: CodeView `.debug$S`

The chosen format is **CodeView** in COFF — the Windows-native debug format that `rust-lld`/`link.exe` fold into a PDB and that `llvm-readobj`/`llvm-pdbutil`/`cdb`/WinDbg consume. DWARF (`.debug_line`) is the portable alternative and is deferred; CodeView is emitted here because the native target is COFF→PE and CodeView is what the Windows link+debug toolchain reads back into a PDB.

With `--debug`, the object gains one extra section, `.debug$S` (`IMAGE_SCN_CNT_INITIALIZED_DATA | MEM_READ | MEM_DISCARDABLE`), carrying a `CV_SIGNATURE_C13` stream of CodeView subsections:

- `DEBUG_S_SYMBOLS` — a minimal `S_COMPILE3` (machine `CV_CFL_X64`) so the stream is a well-formed CodeView symbol subsection.
- `DEBUG_S_LINES` — **one subsection per compiled function**. Its header's function-offset and segment fields are patched by an `IMAGE_REL_AMD64_SECREL` + `IMAGE_REL_AMD64_SECTION` relocation pair against that function's `.text` symbol, and it records the function's code size plus one line record at code offset `+0` mapping to the function's declaration line (`IsStatement`).
- `DEBUG_S_FILECHKSMS` — a single source-file entry (checksum kind `None`) referenced by every `DEBUG_S_LINES` block.
- `DEBUG_S_STRINGTABLE` — holds the source file name.

The line numbers come from `BytecodeFunction.span.line` (the AST/IR/bytecode span already threaded through the pipeline); the emitter reads `function.span.line` into each `LoweredNativeFunction`.

### Line-table granularity: per function

Granularity is **per function** for this increment: each function symbol gets one line record at its entry offset (offset `+0` → its declaration line). That lets a debugger place a breakpoint at a function and show the corresponding `.lby` source line. **Per-statement** line mapping (a line record at each statement's code offset) is deferred; the span data to support it already reaches the bytecode instructions, so it is a follow-up increment rather than a schema change.

### The `--debug` flag and additive emission

`--debug`/`-g` is **opt-in**. Without it, no `.debug$S` section is produced and the emitted object is **byte-for-byte identical** to the default native object, so the existing native snapshot/structural tests are unaffected. `emit_alpha1_native_program` keeps its exact prior behavior; a new `emit_alpha1_native_program_with_debug(module, Some(DebugOptions { source_file }))` adds the section. The CLI passes the `.lby` path as the source file and prints a `debug info: CodeView ...` notice; the object is written and linked exactly as before (the discardable `.debug$S` section links cleanly into the PDB when a PDB is produced, and is otherwise dropped).

### What a debugger can do with it

After linking (`rust-lld` consumes `.debug$S` into a PDB), a debugger can set a breakpoint at any compiled function by name and, on a hit, display the correct `.lby` source line for that function's entry. `llvm-readobj --codeview <obj>` (or `llvm-pdbutil`) reads the records back directly and shows each function's `FunctionLineTable` (`LineNumberStart` = the declaration line) and the recorded source filename.

### Testing

- **Structural (always runs):** `emits_codeview_debug_section_with_per_function_line_info` in `crates/lullaby_ir/src/native_object.rs` emits a `--debug` object for a two-function program and asserts the `.debug$S` section exists, leads with the C13 signature, holds `DEBUG_S_SYMBOLS`/`DEBUG_S_FILECHKSMS`/`DEBUG_S_STRINGTABLE` plus one `DEBUG_S_LINES` per function with 4 header relocations, records the source file name, and maps each function's entry offset to its exact declaration line. `debug_off_is_byte_for_byte_identical_to_default` asserts the no-debug path is unchanged. `native_debug_emits_codeview_line_info` in `crates/lullaby_cli/tests/cli.rs` drives `lullaby native --debug` end to end, asserts the `.debug$S` section + source name are present and the default object has none, and — when `llvm-readobj` (bundled with the rustc toolchain) or `llvm-pdbutil` is discoverable — decodes the CodeView stream and asserts it surfaces the source file and `main`'s declaration line, skipping the readback gracefully otherwise.

### Deferred debug-info work

Deferred beyond this increment: a full PDB emitted by the compiler itself (this increment relies on the linker to build the PDB from `.debug$S`); per-statement line records (finer than per-function); variable/parameter/local names, types, and lexical scopes (`S_LOCAL`/`S_REGREL32`, `.debug$T` type records); inlined-frame and optimized-code stepping info; a live end-to-end debugger session assertion; and DWARF (`.debug_line`/`.debug_info`) for non-Windows / ELF-Mach-O targets.

## Growable `list<T>` (scalar element) (DELIVERED)

`emit_alpha1_native_program` compiles the core growable-list operations for a **scalar element type** (`i64`, every fixed-width integer, `bool`/`char`/`byte`, and `f32`/`f64`): `list_new`, `push`, `get`, `set`, `len`, and `pop`. This mirrors the WASM backend's `list<T>` support and agrees with the AST/IR/bytecode interpreters bit-for-bit, **including value semantics**. It is additive: a function that also uses lists now compiles; a `list<string>`/`list<struct>`/`list<list<…>>`/`list<map<…>>` (a **heap element**) is DEFERRED — the function skips gracefully with a clear reason and still runs on the interpreters, never miscompiled.

### List layout

A `list<T>` value is a **heap pointer** — one 8-byte word, held in an integer register (or a frame slot) exactly like a scalar pointer. It points at a header:

- **`[len i64]`** at offset `0` — the current element count.
- **`[cap i64]`** at offset `8` — the allocated capacity, in elements.
- **`[elem slots…]`** starting at offset `16` — `cap` 8-byte element slots. Element `i` lives at `16 + i * 8`. Every field is an 8-byte word, so the whole block is naturally `i64`-aligned and a scalar element (including an `f32`/`f64`, stored bit-for-bit in its low bytes) moves as a flat word.

The layout uses `i64` headers (versus the WASM backend's `i32` headers) so every field is a uniform 8-byte word, matching how the native backend already lays out struct/array/enum slots. `list_new()` allocates an empty block with `len = 0`, `cap = 4` (`LIST_INITIAL_CAP`), so a handful of pushes do not each reallocate.

### Value semantics

Lullaby lists are value-semantic (`l = push(l, x)` returns a NEW list). Every mutating op — `push`, `set`, `pop` — first **deep-copies** its source list (via the `__lullaby_list_copy` helper) and mutates the fresh copy; the read ops `get`/`len` never mutate. Because the mutators always copy, sharing a list pointer across a `let b = a` binding or a call boundary is safe without any extra copy: `let b = a` then `set(b, …)` leaves `a` untouched (because `set` copies `b`), and a function taking a list parameter cannot alter the caller's list through the shared pointer (its mutators copy). This is exactly the interpreters' `Value::clone`-on-mutate discipline, so a list parameter/return/argument passes as a plain pointer word in an integer register — no by-pointer aggregate ABI needed. The bump heap never reclaims, so a grown or copied list orphans its old block, like the existing string-constant heap growth.

### Runtime helpers and codegen

Three `.text` runtime helpers back the small inline call-site codegen (emitted alongside the string helpers whenever the heap path runs, since a list-using program needs the bump allocator + `.bss` heap even when it interns no string constants):

- **`__lullaby_list_new() -> ptr`** — bump-allocates a `[0][LIST_INITIAL_CAP][slots]` block via `__lullaby_alloc` and returns its pointer.
- **`__lullaby_list_copy(rcx = src) -> rax = copy`** — allocates a block with the source's `cap`, copies the `len`/`cap` headers and the `len` live element words (a flat 8-byte word copy is an exact deep copy — list elements are always scalar).
- **`__lullaby_list_grow(rcx = list) -> rax`** — returns the list unchanged when `len < cap`; otherwise allocates a block with `new_cap = (cap == 0 ? LIST_INITIAL_CAP : cap * 2)`, copies the live elements and the `len` header, writes the new `cap`, and returns the fresh block (orphaning the old one).

Call-site lowering: `push(l, x)` deep-copies `l`, grows the copy, stores `x` into slot `len`, and bumps `len`; `set(l, i, x)` deep-copies `l` and stores `x` into element slot `i`; `pop(l)` deep-copies `l` and decrements its `len`; `get(l, i)` loads element `i` (a float element is moved back into `xmm0`); `len(l)` loads the `len` header word. A float element's value is moved through a GPR (`movq`/`movd`) so the 8-byte store/load preserves its exact bits. **Bounds are not checked** for `get`/`set` in native code (consistent with native array indexing): an out-of-range index reads/writes past the live elements rather than trapping; the interpreters still bounds-check with `L0413`, so a program relying on trapping must run on an interpreter.

### Deferred list work

Deferred beyond this increment: **lists of heap elements** (`list<string>`/`list<struct>`/`list<list<…>>`/`list<map<…>>`) — the element deep-copy would need per-element type dispatch; trapping native bounds checks for `get`/`set`; and a heap `free`/reclamation path (grown/copied blocks are orphaned). (Scalar-key/value `map<K, V>` is now **delivered** — see "Growable `map<K, V>` (scalar key/value)".)

### Verification

The fixtures `tests/fixtures/valid/native_list_build.lby` (build a `list<i64>` via `list_new`/`push` past the initial capacity, then `set`/`get`/`len`/`pop`; `main` returns 134) and `tests/fixtures/valid/native_list_value_semantics.lby` (an aliasing check: `let b = a` then `push(b, …)`/`set(b, …)` must not affect `a`, plus a list-parameter `mutate`; `main` returns 56) run on all interpreter backends for ground truth and are native-compiled, linked, and run by `native_list_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which sources MSVC's `LIB` when unset and asserts each `.exe`'s exit code equals the interpreter's `run` result (mod 256; gated on `rust-lld` + `kernel32.lib`, with the compile-not-skip and interpreter-truth assertions always running). Unit tests in `native_program_tests` assert that list functions (including a `list<i64>`-returning function, a list parameter, and a `list<f64>`) report `compiled` (not `skipped`), that the object emits the `__lullaby_list_new`/`_copy`/`_grow` helper symbols and the bump allocator + `.bss` heap, that a `push` call site references the copy and grow helpers, and that a heap-element `list<string>` skips with a clear reason.

## Growable `map<K, V>` (scalar key/value) (DELIVERED)

`emit_alpha1_native_program` compiles the core growable-map operations for an **integer-cell key** and a **scalar value**: `map_new`, `map_set`, `map_get`, `map_has`, and `map_len`. This mirrors the WASM backend's first `map<K, V>` increment and agrees with the AST/IR/bytecode interpreters bit-for-bit — an **insertion-ordered association list** scanned linearly with value equality — **including value semantics**. It is additive: a function that also uses maps now compiles; a `map<string, V>`/`map<K, list<…>>`/… (a **heap key or value**) or a **float-key** map is DEFERRED — the function skips gracefully with a clear reason and still runs on the interpreters, never miscompiled.

### Map layout

A `map<K, V>` value is a **heap pointer** — one 8-byte word, held in an integer register (or a frame slot) exactly like a scalar pointer or a native list. It points at a header:

- **`[len i64]`** at offset `0` — the current entry count.
- **`[cap i64]`** at offset `8` — the allocated capacity, in entries.
- **`[entries…]`** starting at offset `16` — `cap` two-word entries. Entry `i` lives at `16 + i * 16`; its **key** word is at `+0` and its **value** word at `+8`. Every field is an 8-byte word (uniform with the native list/struct/enum slots), so a scalar value — including an `f32`/`f64`, stored bit-for-bit — moves as a flat word.

`map_new()` allocates an empty block with `len = 0`, `cap = 4` (`MAP_INITIAL_CAP`), so a handful of inserts do not each reallocate. Because `MAP_DATA_OFF == LIST_DATA_OFF == 16`, the shared word-copy loop copies map entry words too (a map with `len` entries copies `2 * len` words).

### Ordering, lookup, and key equality

The map is an **insertion-ordered association list**: `map_set` overwrites the value of an existing key **in place** (preserving its position) or appends a new `(key, value)` entry at the end; `map_get`/`map_has` scan entries **front-to-back** so the first matching key wins; `map_len` reads the `len` header. This matches the interpreters' `Value::Map` (a `Vec<(Value, Value)>`) bit-for-bit. Key equality is an exact **8-byte word compare** (`cmp`), which is exact for the integer-cell key types (`i64`, every fixed-width integer, `bool`/`char`/`byte` — all stored as normalized `i64` cells). Values may be any native scalar, **including a float** (stored/loaded bit-for-bit, never compared).

### Value semantics

Lullaby maps are value-semantic (`m = map_set(m, k, v)` returns a NEW map). The only mutator, `map_set`, first **deep-copies** its source map (via `__lullaby_map_copy`) and mutates the fresh copy; the read ops `map_get`/`map_has`/`map_len` never mutate. Because the mutator always copies, sharing a map pointer across a `let b = a` binding or a call boundary is safe without any extra copy — a map parameter/return/argument passes as a plain pointer word in an integer register, exactly like a native list. The bump heap never reclaims, so a grown or copied map orphans its old block.

### Runtime helpers and codegen

Four `.text` runtime helpers back the small inline call-site codegen (emitted alongside the list + string helpers whenever the heap path runs):

- **`__lullaby_map_new() -> ptr`** — bump-allocates a `[0][MAP_INITIAL_CAP][entries]` block via `__lullaby_alloc` and returns its pointer.
- **`__lullaby_map_copy(rcx = src) -> rax = copy`** — allocates a block with the source's `cap`, copies the `len`/`cap` headers and the `2 * len` live entry words.
- **`__lullaby_map_grow(rcx = map) -> rax`** — returns the map unchanged when `len < cap`; otherwise allocates a block with `new_cap = (cap == 0 ? MAP_INITIAL_CAP : cap * 2)`, copies the live entries and the `len` header, writes the new `cap`, and returns the fresh block (orphaning the old one).
- **`__lullaby_map_find(rcx = map, rdx = key) -> rax`** — linear-scans the entries front-to-back for the first key equal to `rdx`, returning its index, or the map's `len` if no key matches (the "found index else len" convention). Clobbers only `rax`/`r10`/`r11` and preserves `rcx`/`rdx`, so callers reuse the map pointer and key across the call.

Call-site lowering: `map_set(m, k, v)` deep-copies `m`, scans the copy; if the key is found it overwrites that entry's value slot in place, else it grows the copy when full and appends the `(k, v)` entry, bumping `len`, then returns the fresh map pointer. `map_get(m, k)` scans and materializes `some(v)` (loading the found entry's value slot) or `none` into the **native enum/option layout** (a tag word plus payload word), reachable both as a `let`/assign initializer and as a `match` scrutinee. `map_has(m, k)` scans and yields a `bool` (`found != len`). `map_len(m)` loads the `len` header word. A float value is moved through a GPR (`movq`/`movd`) so the 8-byte store/load and the `some(v)` payload preserve its exact bits.

### Deferred map work

Deferred beyond this increment: **string/heap keys** (`map<string, V>`) and **heap values** (`map<K, string>`/`map<K, list<…>>`/…) — string-key equality needs the string heap for content comparison, and a heap value would need per-type deep-copy dispatch — **float keys** (`map<f64, V>`/`map<f32, V>`, whose bit-pattern word compare would diverge from the interpreters' value equality on `±0.0`/NaN); the `map_keys`/`map_values`/`map_del` builtins; and a heap `free`/reclamation path (grown/copied blocks are orphaned).

### Verification

The fixtures `tests/fixtures/valid/native_map_build.lby` (build a `map<i64, i64>` via `map_new`/`map_set` past the initial capacity, update an existing key, then read via `map_get`/`map_has`/`map_len`; `main` returns 155) and `tests/fixtures/valid/native_map_value_semantics.lby` (an aliasing check: `let b = a` then `map_set(b, …)` must not affect `a`, plus a map-parameter `probe`; `main` returns 99) run on all interpreter backends for ground truth and are native-compiled, linked, and run by `native_map_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which sources MSVC's `LIB` when unset and asserts each `.exe`'s exit code equals the interpreter's `run` result (mod 256; gated on `rust-lld` + `kernel32.lib`, with the compile-not-skip and interpreter-truth assertions always running). Unit tests in `native_program_tests` assert that map functions (including a `map<i64, i64>`-returning function, a map parameter, a `match map_get(...)`, and a `map<i64, f64>` float value) report `compiled` (not `skipped`), that the object emits the `__lullaby_map_new`/`_copy`/`_grow`/`_find` helper symbols and the bump allocator + `.bss` heap, that a `map_set` call site references the copy and find helpers, and that a string-key `map<string, i64>` skips with a clear reason.

## Deferred Native Work

Deferred beyond the current increment: more than four **effective** register arguments (stack arguments; the count now includes a hidden aggregate-return pointer), a top-level `f64`/`f32` **scalar** parameter/return across a function boundary (needs XMM argument routing), aggregates containing heap values as boundary values, trapping native array bounds checks, string/collection *values* (native string locals, concatenation, indexing, comparison), string/heap-keyed or float-keyed maps and heap map values, lists of heap elements, heap allocation exposed beyond the string-constant copy and the growable list/map helpers, a heap `free`/reclamation path, builtins beyond a constant-folded array `len`, string-literal `len`, and the delivered list/map ops, cross-platform ELF/Mach-O object emission, CRT-driven `mainCRTStartup` entry, and true bare-metal (no-OS-import) freestanding. (Scalar-field aggregates as parameters, returns, and call arguments; `f64`/`f32`/`bool`/`char`/`byte` scalar lowering within i64-signature functions; `match` over enums; growable `list<T>` with a scalar element; and growable `map<K, V>` with a scalar key/value are **delivered** — see the sections above.) This work must not bypass the AST runtime, typed IR validation, bytecode VM, or existing release verification.
