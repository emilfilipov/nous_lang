# Native Backend Contract

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document records the first native backend contract. The executable source of truth is `crates/lullaby_ir/src/native_contract.rs`.

## Status

Implemented now:

- A serializable `NativeBackendContract` data model in `lullaby_ir`.
- A deterministic `alpha1_native_backend_contract()` baseline.
- `alpha1_value_layout(TypeRef)` coverage for the current type surface: `void`, `i64`, `bool`, `string`, `array<T>`, and `ptr_*`.
- Unit tests for target selection, current value layouts, cleanup sequencing, and JSON round-trip stability.
- A checked-in JSON snapshot under `crates/lullaby_ir/tests/snapshots/alpha1_native_backend_contract.json`.
- A first `x86_64-pc-windows-msvc` COFF object emitter in `crates/lullaby_ir/src/native_object.rs` for zero-argument `main` functions that return a literal `i64`, literal `bool`, `void`, stack-backed `i64` local arithmetic, or straight-line `i64` local assignment arithmetic.
- Checked-in object-emission snapshots under `crates/lullaby_ir/tests/snapshots/alpha1_return_42.coff.json`, `crates/lullaby_ir/tests/snapshots/alpha1_locals_add.coff.json`, and `crates/lullaby_ir/tests/snapshots/alpha1_assignments.coff.json`.
- An extended multi-function native program emitter (`emit_alpha1_native_program`) for the **i64-scalar subset** with control flow, calls, division, an entry stub, `ExitProcess` import, and COFF relocations, plus a best-effort `rust-lld` link-to-`.exe` behind the `lullaby native` command. See "Extended Native Program Emission And Link-To-Executable" below.
- **Stack-allocated scalar aggregates** on top of the i64-scalar subset: all-i64 (optionally nested) structs and fixed-length `i64` arrays as function locals, laid out contiguously in the stack frame. See "Stack-Allocated Scalar Structs And Fixed Arrays" below.
- **First heap step: string constants + a bump heap.** String literals used by `len("...")` are emitted into a read-only `.rdata` section, referenced by REL32 relocations, copied into a `.bss` bump-allocated heap region at runtime, and scanned for their byte length so `main` can derive an i64 from string data. See "First Heap Step: String Constants And Bump Allocator" below.
- **C-ABI FFI (calling C).** A body-less `extern fn NAME params -> Ret` declares an imported C function; a call lowers to a `call` of an undefined external symbol and links against the C runtime (`ucrt.lib`). Each argument is routed to the register selected by its **position and type** (integer/pointer → `rcx`/`rdx`/`r8`/`r9`; `f32`/`f64` → `xmm0..3`); the return is read from `rax` (integer/pointer, narrow-normalized) or `xmm0` (float). Marshalling now spans **beyond scalars**: a raw pointer `ptr<T>` (a C `T*`, real bump-heap/`malloc` address), the `cstr` parameter marker (a Lullaby `string` → NUL-terminated `const char*` via `__lullaby_to_cstr`), a `void` return, and **more than four arguments** (the 5th+ spill onto the stack above the shadow space). Struct-by-value and callback (function-pointer) parameters are the deferred FFI tail, rejected at check time with `L0424`. Calling an extern on an interpreter is `L0423`. See "C-ABI FFI (calling C)" and "Pointer, `cstr`, and >4-argument marshalling" below.
- **C-ABI FFI (exposing Lullaby to C).** An `export fn NAME params -> Ret` is a normal (bodied) Lullaby function additionally exposed under its plain C name as an externally visible, defined `.text` symbol so C (or another object) can call **into** Lullaby. An export must have a C-marshallable scalar signature from the delivered set `i64`/`f64`/`f32` (`L0424` otherwise); a float parameter arrives in its positional SSE register and a float return leaves in `xmm0`. An export-only program (no `main`) emits a library object with no entry stub. See "C-ABI FFI (exposing Lullaby to C)" below.
- **Native source-line debug info (`--debug`).** `lullaby native --debug` (alias `-g`) emits a CodeView `.debug$S` section with a per-function line table mapping each compiled function's entry offset to its `.lby` declaration line, plus the source file name, so a debugger (via a linker-built PDB) can break at a function and show its source line. Opt-in: without `--debug` the object bytes are byte-for-byte unchanged. See "Debug info (`--debug`)" below.
- **Stack-allocated enums + `match` (scalar payloads).** Enum values whose payloads are native scalars — the built-in generics `option<T>`/`result<T, E>` (for scalar `T`/`E`) and user enums — are laid out on the stack as a tag word plus a shared payload region, and `match` dispatches on the tag with identical variant ordering to the interpreters. See "Stack-Allocated Enums And `match`" below.
- **Scalar-field aggregates across function boundaries.** Scalar-field structs, fixed arrays of scalars, and scalar-payload enums are now valid **parameters, return values, and call arguments** (not just locals), using a by-hidden-pointer ABI with copy-in value semantics. A function taking, returning, and mutating such an aggregate is compiled natively and agrees bit-for-bit with the interpreters. See "Scalar-Field Aggregates Across Function Boundaries" below.
- **Growable `list<T>` (scalar or `string` element).** A heap-backed, capacity-doubling growable list with a scalar element type — or a **`string`** element — is compiled to native machine code: `list_new`, `push`, `get`, `set`, `len`, and `pop`, with value semantics (`push`/`set`/`pop` return a new list) and lists crossing function boundaries. A `string` element is a single immutable pointer word stored and copied exactly like a scalar (shared on the flat word-copy deep copy, never deep-recursed). This brings the native backend to parity with the WASM backend's growable-list support including `list<string>`. See "Growable `list<T>` (scalar or `string` element)" below.
- **Growable `map<K, V>` (scalar key; scalar or `string` value).** A heap-backed, capacity-doubling, insertion-ordered association map with an integer-cell key and a scalar **or `string`** value is compiled to native machine code: `map_new`, `map_set`, `map_get`, `map_has`, and `map_len`, with value semantics (`map_set` returns a new map) and maps crossing function boundaries. A `string` value is a shared immutable pointer word. `map_get` returns `option<V>` (including `option<string>`) reusing the native enum/option layout. This brings the native backend to parity with the WASM backend's map increment including `map<K, string>`. String KEYS stay deferred (content equality). See "Growable `map<K, V>` (scalar key; scalar or `string` value)" below.
- **Enums with a `string` payload.** Enum values whose payload is a `string` — `option<string>` (the `map_get` result on a `map<K, string>`), `result<i64, string>` (and the scalar/string arms either way), and user enums with a `string`-carrying variant — compile natively: the payload slot holds the immutable string pointer, materialized on construction and bound in a `match` arm as a flat word (shared, never deep-recursed). A one-level MUTABLE-aggregate payload (`option<struct>`, `result<i64, list<i64>>`) is now delivered too (see the next bullet); nesting past one mutable level and a `map`/`array` payload stay deferred. See the enum layout section below.
- **MUTABLE-heap collection elements/values and enum payloads (`list<struct>`, `list<list<scalar>>`, `map<K, struct>`, `option<struct>`).** A growable collection whose element/value — or an enum whose payload — is a **one-level mutable aggregate** (a `struct` or a nested `list<scalar|string>`) is compiled to native machine code with correct value semantics, mirroring the WASM backend. A struct element/value/payload is laid out on the heap as a single pointer word to a `[nwords][field words]` block; the collection's value-semantic deep copy **recurses** into each mutable element (a fresh independent block via `__lullaby_struct_copy`, or a nested `__lullaby_list_copy`/`__lullaby_map_copy`) instead of sharing the pointer, so `get`/`map_get` return an independent deep copy, `push`/`set`/`map_set` deep-copy the stored value, and a `match` binds a mutable payload as an independent snapshot — matching the interpreters' recursive `Value::clone` bit-for-bit. Nesting past one mutable level (`list<list<list<…>>>`), a `map`/`array` element/value, and a mutable-aggregate map KEY stay deferred (skip gracefully). See "MUTABLE-heap collection elements and enum payloads" below.
- **First-class heap `string` values.** A `string` value is a heap pointer to a `[char_len i64][byte_len i64][utf8 bytes]` record. String literals used as values, string locals/parameters/returns/call arguments, runtime `+` concatenation, `len(s)` for any string value, and `to_string` for integers/`bool`/`char`/`byte` are compiled to native machine code, agreeing bit-for-bit with the interpreters (including UTF-8 char counting). Strings are immutable, so they pass/return by pointer with no deep copy. **Strings as `list`/`map` elements/values and enum payloads are now delivered** (`list<string>`, `map<K, string>`, `option<string>`, `result<i64, string>`, string-payload user enums — see the list/map/enum sections). `to_string(f64)`/`to_string(f32)`, the remaining string builtins (`replace`/`upper`/`lower`/`split`/`join`), string map keys, and strings as struct/array **fields/elements** remain deferred. See "First-class heap `string` values" below.

Now implemented (updated): `bool`/`char`/`byte` and the full fixed-width integer
lattice (`i8`…`u64`, `isize`/`usize`) and `f64`/`f32` floats are lowered as native
scalars within `i64`-signature functions — wrapping/normalized integer
arithmetic, signedness-correct comparison and division, bitwise and shifts, the
`to_<T>`/`to_f32`/`to_f64` conversions, the overflow-aware arithmetic builtins
(`checked_*`/`saturating_*`/`wrapping_*` for `add`/`sub`/`mul`), and SSE/XMM float
arithmetic/comparison.
Control flow (`if`/`while`/`loop`/`for`) and inter-function calls compile.
`extern fn` calls to C are compiled and linked for the **full scalar subset**:
all integer widths plus `bool`/`char`/`byte` (Win64 GPRs) and `f32`/`f64`
(SSE/XMM), including a top-level float scalar as an `extern`/`export`
parameter/return and a mixed float/int signature. `export fn` marshals the scalar
set `i64`/`f64`/`f32`.

Not implemented yet:

- **Now delivered:** the overflow-aware arithmetic builtins `checked_<op>`, `saturating_<op>`, and `wrapping_<op>` for `add`/`sub`/`mul` on every fixed-width kind (`i8`…`u64`, `isize`/`usize`; `i64` is excluded by the type checker), signed and unsigned. `wrapping_*` reuses the default fixed-width `+`/`-`/`*` (wrap then normalize). `saturating_*`/`checked_*` share an inline overflow-detection core that uses the hardware `CF`/`OF` flags (and the widening `mul`/`imul`, high half in `rdx`) for the 64-bit kinds and an exact-compute-then-range-check for the narrow kinds — no division, so no case can trap. `saturating_*` clamps to `T`'s `MIN`/`MAX` (`0` for unsigned underflow); `checked_*` builds an `option<T>` record (`some(result)`/`none`) reusing the native enum/option layout, and is lowered in binding/return position and as a `match` scrutinee. Results are bit-identical to the interpreters' `overflow_arith` for every width and sign (native link-and-run parity test `native_overflow_arith_execution_parity_when_linkable`).
- **Now delivered:** more than four parameters. A native-eligible function with five or more scalar parameters (`i64`/fixed-width/`bool`/`char`/`byte`/`f64`/`f32`), and calls between such functions, pass the 5th+ arguments on the stack per the Win64 stack-argument ABI (the first four stay in `rcx`/`rdx`/`r8`/`r9` with floats positionally in `xmm0..3`). A hidden aggregate-return pointer counts as one register slot, so an aggregate-returning function may now also take four visible parameters (the fourth spilling to the stack). See "Win64 Stack Arguments (5th+ Parameter)" below. Still deferred: more than four arguments to an `extern`/`export` **C-ABI** call (the C-side stack/XMM spill for a 5th+ FFI argument is a separate follow-up).
- **Now delivered:** first-class heap `string` **values** — string locals/parameters/returns/call arguments, string-literal values, runtime `+` concatenation, `len(s)` over any string, and `to_string` for integers/`bool`/`char`/`byte` (see "First-class heap `string` values" below). Still deferred: `to_string(f64)`/`to_string(f32)` (dtoa), the remaining string builtins (`replace`/`upper`/`lower`/`split`/`join`), string indexing, string map keys, and strings as struct/array **fields/elements**. **Strings as `list`/`map` elements/values and enum payloads are now delivered** (`list<string>`, `map<K, string>`, `option<string>`, `result<i64, string>`, string-payload user enums). **Enum values with scalar OR `string` payloads and `match` over them are delivered** (see below); a one-level MUTABLE-aggregate payload (`option<struct>`, `result<i64, list<i64>>`) is now delivered too — deeper nesting and a `map`/`array` payload stay deferred.
- **Now delivered:** scalar-field aggregates (structs, fixed arrays of scalars, scalar-payload enums) as function **parameters, return values, and call arguments** — by a hidden-pointer ABI with copy-in value semantics. A function that returns an enum, and a `match` on the result of such a call, now compile. See "Scalar-Field Aggregates Across Function Boundaries". A top-level `f64`/`f32` **scalar** parameter/return is now delivered (routed through the SSE registers `xmm0..3`, positionally aligned with the integer registers, with the float return in `xmm0`). Still deferred at the aggregate boundary: aggregates containing heap values (`string`/`list`/`map`, or a heap element/field). A signature whose effective register arguments (parameters plus a hidden return pointer) exceed four is **no longer** deferred — the 5th+ effective argument spills to the stack (see "Win64 Stack Arguments (5th+ Parameter)").
- **Now delivered:** growable **`list<T>` with a scalar OR `string` element** (`i64`/fixed-width/`bool`/`char`/`byte`/`f32`/`f64`, or `string`) — `list_new`, `push`, `get`, `set`, `len`, and `pop`, with value semantics and lists crossing function boundaries. A `string` element is a shared immutable pointer word. See "Growable `list<T>` (scalar or `string` element)" below. **A one-level MUTABLE-aggregate element (`list<struct>`, `list<list<scalar>>`) is now delivered** — deep-copied per element on the value-semantic copy (see "MUTABLE-heap collection elements and enum payloads" below). Still deferred: lists whose element is a `map` (`list<map<…>>`), nesting past one mutable level (`list<list<list<…>>>`), and arrays whose length is not known from a literal initializer. The bump heap still has no `free`/reclamation (a grown or copied list orphans its old block).
- **Now delivered:** growable **`map<K, V>` with an integer-cell key and a scalar OR `string` value** — `map_new`, `map_set`, `map_get` (returning `option<V>`, including `option<string>`), `map_has`, and `map_len`, with insertion-ordered value semantics and maps crossing function boundaries. A `string` value is a shared immutable pointer word. See "Growable `map<K, V>` (scalar key; scalar or `string` value)" below. **A one-level MUTABLE-aggregate value (`map<K, struct>`) is now delivered** — deep-copied per value on the value-semantic copy, with `map_get` returning `option<struct>` (see "MUTABLE-heap collection elements and enum payloads" below). Still deferred: **string/heap KEYS** (`map<string, V>`) — string-key equality needs the string heap for content comparison, matching the WASM map — a `map`/`array`-typed value or nesting past one mutable level, a mutable-aggregate KEY, **float keys** (their bit-pattern word compare would diverge from the interpreters' value equality on `±0.0`/NaN; float *values* are fine, stored/loaded bit-for-bit), and `map_keys`/`map_values`/`map_del`.
- **Now delivered (structurally):** object-file writing for the non-COFF **x86-64** targets. The native backend emits a relocatable **ELF64** (`x86_64-unknown-linux-gnu`) or **Mach-O x86-64** (`x86_64-apple-darwin`) object in addition to COFF, selected by `lullaby native --target <triple>`. See "Object-Format Abstraction (COFF / ELF / Mach-O)" below. Still deferred for the x86-64 ELF/Mach-O objects: **link-and-run** verification (this is a Windows host with no x86-64 ELF/Mach-O runner — deferred to the Phase 9 cross-platform CI), a non-Windows C-runtime link workflow, and DWARF debug info (the `--debug` CodeView section is COFF-only).
- **Now delivered (link-and-run verified):** a second **instruction-set** backend — **AArch64 (ARM64) Linux** (`aarch64-unknown-linux-gnu`). A dedicated code generator (`crates/lullaby_ir/src/aarch64.rs`) consumes the same `BytecodeModule` and emits a freestanding **aarch64 ELF64** object (`EM_AARCH64`, `R_AARCH64_CALL26` `bl` relocations) for the `i64`-scalar core: `i64`/`bool` literals, locals, parameters (`x0..x7`) and returns (`x0`); `+ - * /`, bitwise `& | ^ << >>`, comparisons, and short-circuit `and`/`or`/`not`; `if`/`elif`/`else`, `while`, `loop`, inclusive range `for` with `break`/`continue`; inter-function `bl` calls under AAPCS64; and a freestanding `_start` that calls `main` and issues the Linux `exit` syscall (`x8=93`, `svc #0`). This is **actually linked and run**: a CLI test links the object with the cross-linker (`ld.lld -m aarch64linux`) and runs it under Docker's arm64 (QEMU) emulation, asserting the process exit code equals the interpreter's `run` result mod 256 (gated on Docker+arm64 and `ld.lld`; skipped gracefully otherwise). See "AArch64 (ARM64) instruction-set backend" below. Out of scope for this first AArch64 core (recorded as skipped functions, never miscompiled): heap/aggregates (`struct`/`array`/`list`/`map`), `string`/`char`/`float` values, `enum`/`match`, `extern` FFI, `throw`/`try`, inline `asm`, and `await`/closures.
- Native runtime packaging.
- ARM64 macOS (`aarch64-apple-darwin`) code generation and Mach-O arm64 object emission (a separate future effort; the triple is a declared target with no code generator yet, and `--target aarch64-apple-darwin` is rejected with `L0347`). The AArch64 machine-code encoder now exists — only the Mach-O arm64 container and macOS entry/exit remain.

## Targets

The default native target is `x86_64-pc-windows-msvc` with COFF object emission. The backend now emits relocatable objects for the two other x86-64 targets and for **AArch64 Linux** (the second instruction set); the `--target` flag selects both the instruction set and the object-file container. The `aarch64-apple-darwin` triple is a declared future target with no code generator yet.

| Triple | Object format | Object emission | Link + run |
| :--- | :--- | :--- | :--- |
| `x86_64-pc-windows-msvc` (default) | COFF | delivered | best-effort `rust-lld` → `.exe` on this host |
| `x86_64-unknown-linux-gnu` | ELF64 (System V AMD64) | delivered | deferred to Phase 9 CI (no cross-linker on a Windows host) |
| `x86_64-apple-darwin` | Mach-O x86-64 | delivered | deferred to Phase 9 CI (no cross-linker on a Windows host) |
| `aarch64-unknown-linux-gnu` | ELF64 (AAPCS64) | delivered (`i64`-scalar core) | **link+run verified** via `ld.lld` + Docker arm64 (QEMU) |
| `aarch64-apple-darwin` | Mach-O arm64 | pending (no Mach-O arm64 container) | pending |

All current contract targets are 64-bit little-endian targets.

## Calling Convention

The native backend uses an internal Lullaby ABI before adapting to platform object and linker conventions:

- Parameters lower in source order.
- `main` remains the zero-argument entry function for executable validation.
- Scalar and handle return values are returned directly.
- Variadic calls are not part of this backend.
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

The current value layouts are:

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

`lullaby_ir::native_object::emit_alpha1_native_program` extends the prototype into a linkable multi-function COFF object for the **i64-scalar subset** — the native mirror of the WASM backend's scalar subset, restricted to `i64` for this increment. It is exercised by `lullaby native` (see [language_surface.md](language_surface.md)).

Eligibility and lowering:

- source is still validated, lowered to typed IR, and lowered to bytecode before object emission
- a function is eligible when its parameters and return type are all `i64` (or, more broadly, native scalars/aggregates per the sections below); the first four register arguments use `rcx`/`rdx`/`r8`/`r9` and any 5th+ argument spills to the stack (see "Win64 Stack Arguments (5th+ Parameter)"), so there is no fixed parameter-count cap
- supported bodies: integer literals, params/`let` locals, `+ - * /` (signed `idiv`, dividend sign-extended with `cqo`; the `i64::MIN / -1` overflow case is guarded so it wraps to `i64::MIN` — matching the interpreters' `wrapping_div` — instead of raising a hardware `#DE`), comparisons producing `0`/`1`, short-circuiting `and`/`or` and `not`, `if`/`elif`/`else`, `while`, infinite `loop` with `break`/`continue`, range `for` (lowered to an `i64` counter loop mirroring the interpreter's inclusive range and optional step), `return`, a value-producing tail expression, and calls to other compiled i64 functions (including recursion)
- ineligible functions are recorded with a reason and still run on the interpreters; when no i64-scalar function (including `main`) is eligible the emitter returns an error carrying diagnostic code `L0339`

Code generation is a stack-machine model over `rax`: expressions evaluate into `rax`, binary operands spill to the stack with `push`/`pop rcx`, locals and spilled parameters live in `[rbp - slot]` frame slots, and the frame reserves 32 bytes of Win64 shadow space whenever the function issues a call so `rsp` stays 16-byte aligned at each `call`. Inter-function calls, the entry stub's call to `main`, and the entry stub's call to the imported `ExitProcess` are emitted as `call rel32` with `IMAGE_REL_AMD64_REL32` COFF relocations; symbol names longer than eight bytes are stored in the COFF string table. The emitted entry stub `_lullaby_start` calls `main`, moves the `i64` result into `ecx`, and calls `ExitProcess` (imported from `kernel32`), so the process exit code is `main`'s result mod 256.

Link-to-executable is best-effort. The CLI writes the COFF object unconditionally (the reliable floor), then attempts to link with `rust-lld` (`-flavor link`, `/subsystem:console`, `/entry:_lullaby_start`, the object, and `kernel32.lib`), discovering `rust-lld` under the rustc sysroot and library search paths from the MSVC `LIB` environment variable. When `rust-lld` or `kernel32.lib` cannot be located the command reports the object and explains that linking was unavailable rather than failing.

## Object-Format Abstraction (COFF / ELF / Mach-O) (DELIVERED, structural)

The generated x86-64 machine code — the entry stub, the compiled functions, and the heap/string runtime helpers — is **platform-agnostic**: the internal Lullaby-to-Lullaby calling convention is self-consistent, so the same `.text` bytes, symbol table, and REL32 relocation model can be written into any of the three x86-64 object containers. Only the **object-file wrapper** and the **entry/exit mechanism** differ. `lullaby native --target <triple>` selects the container; the default stays `x86_64-pc-windows-msvc` (COFF), so existing behavior and the byte-for-byte COFF snapshots are unchanged.

### Neutral object model

`crates/lullaby_ir/src/object_model.rs` defines a container-neutral `ObjectModel` — a list of sections (`.text`, `.rodata`, `.bss`), a flat global symbol table (each symbol tagged code vs data, defined-in-section vs undefined-external), a target-machine tag (`ObjectMachine::X86_64` / `Aarch64`), and per-section relocations tagged `Branch` (an x86-64 `call`/`jmp` to a function), `PcRel32` (a RIP-relative `lea`/`mov` to a data symbol), or `Aarch64Call26` (an AArch64 `bl` call site). `emit_alpha1_native_program_for_target` builds this model from the same lowered functions the COFF path uses, then serializes it with the format-specific writer; the AArch64 backend (`aarch64.rs`) builds its own `Aarch64`-tagged model and serializes it through the same ELF writer. The **COFF path deliberately keeps its own writer** (`native_object.rs`) rather than flowing through the model, so its snapshot tests stay exact; the model is the shared source of truth for ELF and Mach-O.

### ELF64 (`x86_64-unknown-linux-gnu`)

`crates/lullaby_ir/src/elf_object.rs` writes a relocatable (`ET_REL`) ELF64 object: an `Elf64_Ehdr`, a section header table (`.text` `SHT_PROGBITS`+`ALLOC`+`EXECINSTR`, `.rodata` when strings are used, `.bss` `SHT_NOBITS` for the bump heap, `.rela.text`, `.symtab`, `.strtab`, `.shstrtab`), a global symbol table, and `Elf64_Rela` relocations. The `e_machine` field and the relocation types come from the model's `ObjectMachine` tag. For `EM_X86_64`, a `call` site uses `R_X86_64_PLT32` and a data reference uses `R_X86_64_PC32`, both with addend `-4` (the field ends 4 bytes past `P`, so the displacement to `S` is `S − (P + 4)`). For `EM_AARCH64`, a `bl` call site uses `R_AARCH64_CALL26` (= 283) with addend `0` (the linker writes `(S + A − P) >> 2` into the instruction's low 26 bits). The same writer serializes both x86-64 and AArch64 models, so the aarch64 ELF is not a separate container implementation.

### Mach-O x86-64 (`x86_64-apple-darwin`)

`crates/lullaby_ir/src/macho_object.rs` writes a relocatable (`MH_OBJECT`) Mach-O 64 object: a `mach_header_64`, a single `LC_SEGMENT_64` holding the `__text`/`__const`/`__bss` (`S_ZEROFILL`) sections, an `LC_SYMTAB`, an `LC_DYSYMTAB` (defined symbols precede undefined ones), and `relocation_info` entries with `r_pcrel=1`, `r_length=2`, `r_extern=1` using `X86_64_RELOC_BRANCH` for calls and `X86_64_RELOC_SIGNED` for data references (the implicit `-4` comes from the PC-relative field ending after the instruction).

### Freestanding entry / exit

The internal calling convention is **kept unchanged** across all three x86-64 platforms; only the entry stub differs. The COFF stub `_lullaby_start` calls `main`, moves the result to `ecx`, and calls `kernel32!ExitProcess`. The freestanding non-Windows stubs (`_start` on ELF, `start` on Mach-O) instead reserve the internal ABI's shadow space (`sub rsp, 32`, landing `rsp` 16-aligned for the `call main` from the OS's 16-aligned entry), call `main`, move the exit code to `edi`, and issue a raw `exit` syscall — `mov eax, 60; syscall` on Linux, `mov eax, 0x2000001; syscall` on macOS — so the object needs no libc, mirroring the freestanding COFF approach. An `extern fn` C dependency remains a Windows-oriented feature; on the non-Windows targets the undefined external is still emitted for the linker but no C-runtime link workflow is provided yet.

## AArch64 (ARM64) instruction-set backend (DELIVERED, link-and-run verified)

`crates/lullaby_ir/src/aarch64.rs` is Lullaby's **second instruction set**. It consumes the *same* `BytecodeModule` the x86-64 backend lowers (`emit_alpha1_native_program_for_target` delegates to `aarch64::emit_aarch64_program` when the target architecture is `Aarch64`) and emits a freestanding **aarch64 ELF64** object through the shared neutral `ObjectModel` / `elf_object.rs` path.

### Covered subset (the `i64`-scalar core)

Mirroring the scalar core the x86-64 backend started from: `i64` (and `bool`, as a `0`/`1` word) literals, locals, parameters (in `x0..x7`) and returns (`x0`); arithmetic `+ - * /` (`add`/`sub`/`mul`/`sdiv`); bitwise `& | ^ << >>` (`and`/`orr`/`eor`/`lslv`/`asrv`); comparisons (`cmp` + `cset`); short-circuit `and`/`or`/`not`; control flow `if`/`elif`/`else`, `while`, `loop`, and inclusive range `for` with `break`/`continue` (`b`/`b.cond`/`cbz` with fixed-up 26-/19-bit offsets); and inter-function calls under AAPCS64 (arguments in `x0..x7`, result in `x0`, `bl` with an `R_AARCH64_CALL26` relocation, 16-byte stack alignment). The prologue is `stp x29, x30, [sp, #-16]!` + `sub sp, sp, #frame` + `mov x29, sp` with locals addressed at `[x29, #8*k]`; the epilogue restores and `ret`s. Binary operands spill through the 16-byte-aligned machine stack (`str x0, [sp, #-16]!` / `ldr x1, [sp], #16`), so alignment is preserved across nested calls.

### Freestanding entry / exit

`_start` (the ELF default entry) is `bl main; movz x8, #93; svc #0; brk #0` — it calls `main`, whose `i64` result is already the exit status in `x0`, then issues the Linux AArch64 `exit` syscall (`x8 = 93`). No libc is required.

### Skipped, never miscompiled

Everything outside the scalar core — heap/aggregates (`struct`/`array`/`list`/`map`), `string`/`char`/`float` values, `enum`/`match`, `extern` FFI, `throw`/`try`, inline `asm`, and `await`/closures — makes a function ineligible; it is recorded as a skipped function (like the x86-64 backend records its unsupported cases) rather than emitted incorrectly. A call to a skipped (or otherwise non-eligible) function demotes the caller through the same fixpoint the x86-64 path uses.

### Honest verification boundary

Unlike the x86-64 ELF/Mach-O objects (structural-only on this Windows host), the AArch64 object **is actually linked and run**. Unit tests in `aarch64.rs` pin the fixed instruction encodings (`stp`/`ldp`/`ret`/`svc`/`bl`/`movz`), assert the emitted object is a valid aarch64 ELF (`\x7fELF`, `EM_AARCH64` = 183), and check the `_start` stub words and the `R_AARCH64_CALL26` relocation. The end-to-end CLI test `native_aarch64_links_and_runs_under_docker` emits the object with `lullaby native --target aarch64-unknown-linux-gnu`, links it with the cross-linker (`ld.lld -m aarch64linux`, i.e. `rust-lld` in gnu flavor), runs it under Docker's `linux/arm64` (QEMU) emulation, and asserts the process **exit code equals the interpreter's `run` result mod 256**. That test is gated on Docker+arm64 emulation and `ld.lld` being available and skips gracefully otherwise, exactly like the node-gated WASM parity tests. On the development host this executed for real: the `native_scalars.lby` fixture (recursion, range `for`, inter-function calls) exits `39`, matching the interpreter.

### Honest verification boundary (x86-64 ELF / Mach-O)

The x86-64 ELF and Mach-O objects are still verified **structurally only** (this Windows host has no x86-64 ELF/Mach-O runner): unit tests in `elf_object.rs` and `macho_object.rs` parse the emitted bytes back and assert the magic numbers, class/endianness, header fields, section table, symbol table (names, types, defining sections), and relocation records (offsets, symbol indices, types, addends); CLI tests confirm `lullaby native --target …` produces a file beginning with the correct magic (`\x7fELF` / `\xCF\xFA\xED\xFE`) without attempting to link. **Link-and-run verification of the x86-64 ELF/Mach-O objects is deferred to the Phase 9 cross-platform CI** on real Linux/macOS runners. The CLI reports the deferral explicitly for those targets (it never claims to have linked or run an x86-64 cross-target object), while for the aarch64 target it reports the real link/run path.

## Win64 Stack Arguments (5th+ Parameter) (DELIVERED)

`emit_alpha1_native_program` passes a function's 5th and subsequent arguments on the stack per the Win64 stack-argument ABI, so there is **no fixed parameter-count cap** — a native-eligible function with five, six, eight, or more scalar parameters (`i64`/fixed-width/`bool`/`char`/`byte`/`f64`/`f32`), and calls between such functions, compile natively instead of demoting to the interpreters.

### Register vs. stack routing

- The first four **effective register arguments** use the Win64 registers: an integer/pointer/aggregate-copy pointer at effective position N uses integer register N (`rcx`/`rdx`/`r8`/`r9`), and a float at position N uses `xmm N` (positionally aligned with the integer registers). An aggregate-returning function's **hidden result pointer** is effective position 0 (`rcx`), shifting the visible parameters down by one position.
- The 5th+ effective argument (position ≥ 4) is passed on the stack **above the callee's 32-byte shadow space**. At a `call`, the caller reserves the 32-byte shadow plus `8 × (number of stack arguments)`; the Kth stack argument (0-indexed `pos − 4`) is written at `[rsp + 32 + 8×(pos−4)]`.

### Callee prologue (reading a stack parameter)

On entry the callee sees the return address at `[rsp]`; after `push rbp` + `mov rbp, rsp` the saved rbp is at `[rbp]`, the return address at `[rbp+8]`, the caller's shadow space at `[rbp+16 .. rbp+48]`, and the first stack argument at `[rbp+48]`. So the Kth stack parameter is read from **`[rbp + 48 + 8×(pos−4)]`** (a scalar loads one word into its frame slot; a float stack argument is copied as a raw bit-preserving word; an aggregate stack argument holds a pointer whose words are copied-in exactly like a register aggregate parameter).

### Caller call site (writing stack arguments)

The caller stages **every** argument onto the machine stack left-to-right (a scalar value via `rax`, a float word via `xmm0`, or an aggregate-copy pointer), which keeps a later argument's evaluation — including a nested call in argument position — from clobbering an already-placed register. It then distributes each staged word: effective positions < 4 load into their GPR/XMM argument register; each position ≥ 4 is copied into the outgoing area at `[rsp + 8n + 32 + 8×(pos−4)]` (which becomes `[rsp' + 32 + 8×(pos−4)]` after the `n` staging words are discarded), exactly where the callee reads it. The hidden aggregate-return pointer, when present, is loaded into `rcx` last. Frame planning reserves the outgoing area (sized to the widest single call's stack-argument count) at the bottom of the frame alongside the shadow space, and the 16-byte alignment at each `call` is preserved.

### Deferred

More than four arguments to an `extern`/`export` **C-ABI** call is still deferred (the C-side stack/XMM spill for a 5th+ FFI argument is a separate follow-up); such a call demotes to the interpreters. Internal Lullaby-to-Lullaby calls have no such cap.

### Verification

The fixture `tests/fixtures/valid/native_many_args.lby` defines `six` (six `i64` params), `eight` (eight `i64` params), and `scale` (a six-parameter mixed `i64`/`f64` signature), each called from `main` to compute a deterministic `i64` (98). It runs identically on AST/IR/bytecode (auto-discovered parity harness) and is native-compiled, linked, and run by `native_many_args_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts every `>4`-parameter function compiles (not skipped) and — when linkable — the `.exe` exit code equals the interpreter result (98). Unit tests in `native_program_tests` (`function_with_six_i64_params_compiles_with_stack_args`, `function_with_eight_i64_params_compiles_with_stack_args`, `function_with_mixed_int_float_params_beyond_four_compiles`, `aggregate_return_with_four_params_uses_a_stack_argument`) assert the callee reads its stack parameters from `[rbp+48]`, `[rbp+56]`, … and the caller writes its stack arguments to the outgoing area at `[rsp+0x50]`, `[rsp+0x58]`, ….

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

Supported native string surface (this section documents the **original first heap step**; first-class string **values** and the string operations described in the dedicated string-values section below now supersede its "no string value yet" scope):

- **`len(string_literal)`** is the one observable string operation. It is *not* constant-folded: the literal's bytes are interned into `.rdata`, `len` bump-allocates a heap copy of them, and scans the heap copy for its terminator to produce the returned `i64`. This exercises the whole first heap step end to end — a read-only constant, a REL32 relocation to its address, a real bump allocation into a writable region, and per-byte reads of both `.rdata` and the heap — and the result is observable through the process exit code. Only ASCII string literals are accepted so that the returned byte length equals the interpreter's char-count `len`; a non-ASCII literal demotes the function to the interpreters. `len` over a fixed native array still folds to a compile-time constant as before.
- There is **no native string value** yet: a string literal is legal only as the immediate argument of `len`. Assigning a string to a local, passing one to a function, returning one, concatenating (`+`), or indexing is still rejected, so such functions run on the interpreters (recorded as skipped; `L0339` if nothing is eligible).

Object layout and codegen (only when a program references string constants):

- The object gains two data sections and two `.text` helpers. `.rdata` (`IMAGE_SCN_CNT_INITIALIZED_DATA | MEM_READ`) holds the NUL-terminated string bytes; each unique literal is interned once (repeated literals dedup) and named `__str{i}`. `.bss` (`IMAGE_SCN_CNT_UNINITIALIZED_DATA | MEM_READ | MEM_WRITE`, `SizeOfRawData` = size, `PointerToRawData` = 0) reserves an 8-byte bump-pointer cell `__lullaby_heap_next` at offset 0 followed by a fixed 64 KiB heap region `__lullaby_heap_base` at offset 8.
- `__lullaby_alloc(size in rcx) -> ptr in rax` is a bump allocator: it reads `__lullaby_heap_next`, lazily seeds it to `&__lullaby_heap_base` on first use (a zero pointer means "uninitialized"), returns the current pointer, and advances it past an 8-byte-rounded allocation. It has **no `free`/reclamation** — allocations are never reclaimed for this increment.
- `__lullaby_strlen_copy(src in rcx) -> len in rax` measures the `.rdata` source length, calls `__lullaby_alloc` for `n + 1` bytes, `rep movsb`-copies the string (with terminator) into the heap, and scans the heap copy for its length. It saves/restores the non-volatile `rsi`/`rdi`/`rbx` and keeps `rsp` 16-aligned at the internal `call`.
- A `len(string_literal)` call site lowers to `lea rcx, [rip + __str{i}]` (a REL32 relocation to the `.rdata` symbol) followed by `call __lullaby_strlen_copy` (a REL32 relocation to the helper). Cross-section references reuse the existing `IMAGE_REL_AMD64_REL32` machinery; the `.rdata`/`.bss` data symbols carry COFF type `0` (not the function type `0x20`).
- The fixture `tests/fixtures/valid/native_strings.lby` (`main` returns `len("hello") + len("native") + len("") = 11`) runs on all backends for ground truth and is native-compiled, linked, and run by `native_strings_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts the `.exe` exit code equals the interpreter's `run` result mod 256 (gated on `rust-lld` + `kernel32.lib`).

## Stack-Allocated Enums And `match` (DELIVERED)

`emit_alpha1_native_program` lowers **enum values with scalar or `string` payloads** and **`match`** over them. This covers the built-in generic enums `option<T>` and `result<T, E>` (when `T`/`E` are native scalars **or `string`**) — including `option<string>` (the `map_get` result on a `map<K, string>`) and `result<i64, string>` — and **user enums** whose variant payloads are all native scalars or `string`. It is additive: a function using only these plus the already-supported i64-scalar/aggregate/control-flow subset now compiles; anything outside it (a MUTABLE-heap-payload enum, or a `match` on an enum-returning call whose *arguments* are aggregates) is skipped with a clear reason and runs on the interpreters — never miscompiled.

### Enum layout

An enum local occupies a **tag word** followed by a shared **payload region** sized to the widest variant:

- **Word 0 is the discriminant tag** — the variant's index in the interpreter/IR variant order. For a **user enum** this is declared order; for the built-ins it is `some`(0)/`none`(1) and `ok`(0)/`err`(1). Because the interpreters select a `match` arm by variant *name*, any consistent tag numbering is correct; the native backend fixes the numbering to the declared/built-in order so construction and dispatch always agree.
- **Payload words follow at word 1, 2, …** in field order. The payload region is `max` over the variants of that variant's total scalar payload words (each scalar payload field is one 8-byte word, like a struct field). A narrower variant leaves the trailing payload words untouched; `match` only reads the words the *matched* variant defines, so stale bytes are never observed.
- A payload word is an `i64`/fixed-width/`bool`/`char`/`byte` cell (stored through a GPR), an `f32`/`f64` (stored through XMM), or a **`string`** — an immutable heap pointer stored through a GPR as a flat word, exactly like an `i64` cell. Because strings are immutable, sharing the pointer IS the payload's value-semantic copy, so a `string` payload needs no deep-recurse on the flat word-copy enum deep copy (`option<string>`, `result<i64, string>`, and string-payload user enums are all supported). A MUTABLE-heap payload (`list`/`array`/`map`/nested struct/enum) is out of scope.

### Construction

`some(x)`, `none`, `ok(x)`, `err(e)`, and a user `Variant(payload…)` materialize directly into the local's words: `mov` the variant's tag into word 0, then evaluate each payload expression into its payload word (GPR for a scalar cell or a `string` pointer, XMM for a float). Unit variants (`none`, a payload-less user variant) write only the tag. A whole-value reassignment (`s = Pulse(n)`) re-materializes tag + payload in place.

### `match`

- The scrutinee is materialized into a stack region: a plain enum **local** is matched in place; a **freshly-constructed** enum scrutinee is spilled into a scratch region reserved during frame planning (one shared region sized to the widest temporary enum scrutinee in the function).
- Dispatch reloads the tag word and, per variant arm, emits `cmp rax, tag` + `jne next_arm`; the matched arm **binds the variant's payload words** into arm-scoped locals (a `load`/`store` per word — a GPR for a scalar cell or a `string` pointer, XMM for a float), lowers the arm body, then `jmp`s to the shared match end. A bound `string` local shares the pointer and is used like any other string value (e.g. `len(s)`). A `_` **wildcard** arm binds nothing and is an unconditional fall-in (it is terminal, so no later arm is emitted).
- A wildcard-free match is exhaustive (semantics guarantee it), so the impossible fallthrough after the last variant arm emits a `ud2` trap rather than running off the end.
- A value-producing `match` (its arms yield the function's result) leaves each arm's value in `rax`; the caller emits the return epilogue after the shared end. A void `match` is a statement whose arm results are discarded.

### Deferred enum work

- **`string` payloads are now delivered** — `option<string>`, `result<i64, string>` (and the scalar/string arms either way), and user enums with a `string`-carrying variant compile: the payload slot holds the immutable string pointer, shared on the flat word-copy deep copy. **A one-level MUTABLE-aggregate payload is now delivered too** — `option<struct>` (the `map_get` result on a `map<K, struct>`), `result<i64, list<i64>>`, and a user enum with a `struct`/nested-`list` variant compile: the payload slot holds a single pointer word, deep-copied on the enum's value-semantic copy (a `HeapStruct` via `__lullaby_struct_copy`, a nested `list` via `__lullaby_list_copy`), and a `match` binds a `HeapStruct` payload into a fresh stack `Struct` local (see "MUTABLE-heap collection elements and enum payloads"). Still deferred: nesting past one mutable level (`result<i64, list<list<list<…>>>>`) and a `map`/`array` payload — such an enum skips gracefully.
- **Enums as parameters, returns, and call arguments are delivered** — a scalar- or `string`-payload enum crosses a function boundary by the aggregate ABI (see "Scalar-Field Aggregates Across Function Boundaries"). A function returning an enum, and a `match` on the result of such a call (e.g. matching `lookup(k)` where `lookup -> option<i64>`), now compile. A string-payload enum's boundary copy is a flat word copy that shares the (immutable) string pointer, so its value semantics are exact. Matching a **local** enum or a **freshly-constructed** enum remains fully supported. A `match` on an enum-returning call whose *arguments* are themselves aggregates is deferred (the shared scratch region would need the scrutinee and argument copies simultaneously).
- **Nested enum values inside struct/array fields** are deferred.

### Verification

The fixtures `tests/fixtures/valid/native_enum_option.lby` (`option<i64>`, some=49-path), `native_enum_result.lby` (`result<i64, i64>`, ok/err scalar), and `native_enum_user.lby` (a user `Signal` enum with a scalar payload and a `_` arm) run on all interpreter backends for ground truth and are native-compiled, linked, and run by `native_enum_match_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts each `.exe` exit code equals the interpreter's `run` result mod 256 (gated on `rust-lld` + `kernel32.lib`). `tests/fixtures/valid/native_enum_returned.lby` (an `option<i64>`-returning `lookup` and a `match lookup(k)`) now native-compiles under the aggregate ABI. The **string-payload** case adds `tests/fixtures/valid/native_result_string.lby` (`result<i64, string>` matched over both arms, plus `option<string>` and a user `Tag` enum with a `string` payload; `main` returns 52), native-compiled/linked/run by `native_string_collections_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs` (same gating and mod-256 exit-code assertion). Unit tests in `native_program_tests` assert the option/result/user-enum functions (including the string-payload `result<i64, string>`, `option<string>`, and a user string-payload enum) report `compiled` (not `skipped`) and that the expected opcodes appear (tag load + `cmp`/`jne` dispatch, and a `ud2` for the wildcard-free exhaustive case), plus that a MUTABLE-heap-payload case (`result<i64, list<i64>>`) skips with a clear reason.

## Scalar-Field Aggregates Across Function Boundaries (DELIVERED)

`emit_alpha1_native_program` passes, returns, and mutates **scalar-field aggregates** — scalar-field structs (nested allowed), fixed arrays of scalars, and scalar-payload enums (`option`/`result`/user enums) — across native function boundaries, not just as locals. It preserves Lullaby's **value semantics** (an aggregate parameter/return/argument is an independent snapshot, exactly like the interpreters) and agrees bit-for-bit with the AST/IR/bytecode backends.

### ABI: by hidden pointer, copy-in value semantics

The aggregate boundary uses an internal by-pointer convention (all callers and callees are Lullaby, so it need not match the Win64 small-struct-in-register rule, only be self-consistent):

- **Aggregate parameter.** The caller materializes the aggregate into a caller-owned copy (in its frame or a scratch temp) and passes a **pointer** to it in the parameter's integer register (`rcx`/`rdx`/`r8`/`r9`). In its prologue the callee **copies the words in** to its own frame slots (`mov rcx, [rax - 8*k]` / `mov [rbp - slot], rcx`), so mutating the parameter never touches the caller's copy.
- **Aggregate return.** The caller allocates space for the result and passes its address as an implicit **hidden first argument** (`rcx`), shifting the visible parameters to the following registers. The callee writes the result words through that pointer (`mov [rax - 8*k], rcx`) and returns the pointer in `rax`. `main`'s scalar `i64` return path is unchanged (no hidden pointer).
- **Aggregate call argument.** The caller materializes a fresh copy in a scratch region, `lea`s its address, and passes that pointer per the parameter rule.
- **Word layout matches locals.** Aggregate words descend in memory (word `k` at the lower address `[base - 8*k]` from the word-0 pointer), matching the existing local layout, so field/element order and offsets are identical to the locals implementation.

### Arity and deferral

An aggregate return consumes one integer register for the hidden result pointer, so it counts as one **effective register argument** (parameters plus a hidden return pointer, if any). The first four effective arguments use `rcx`/`rdx`/`r8`/`r9`; a 5th+ effective argument spills to the outgoing stack area (see "Win64 Stack Arguments (5th+ Parameter)"), so arity no longer demotes the function. **Deferred / skipped:** aggregates containing heap values (`string`/`list`/`map`, or a heap element/field). A top-level `f64`/`f32` **scalar** parameter/return is delivered (XMM routing; float payloads *inside* a by-pointer aggregate are also fine, copied as raw bit-preserving words). None of the deferred cases miscompile; they run on the interpreters.

### Array-length inference

A fixed array carries no length in its `array<T>` type, so an array-typed parameter or return has its length inferred: a **returned** array's length comes from the function's returned array value (an array literal or a fixed-array local); a **parameter** array's length comes from every call site's argument in that position, which must all agree. A length that cannot be determined or that disagrees across callers demotes the function to the interpreters rather than guessing.

### Verification

The fixtures `tests/fixtures/valid/native_aggregate_params.lby` (a function taking a struct and returning an i64, one returning a struct, and a value-semantics check via `mutate_local`), `native_aggregate_array.lby` (taking and returning a fixed `i64` array plus a value-semantics check), `native_aggregate_enum.lby` (`option<i64>` as parameter and return, including a `match` on an enum-returning call), and `native_aggregate_value_semantics.lby` (a struct and an array whose callees clobber their parameter copies) each return a deterministic i64 < 256, run on all interpreter backends for ground truth, and are native-compiled, linked, and run by `native_aggregate_boundary_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, asserting the `.exe` exit code equals the interpreter's `run` result (gated on `rust-lld` + `kernel32.lib`; the compile-not-skip and interpreter-truth assertions always run). Unit tests in `native_program_tests` assert the struct/array/enum boundary functions report `compiled` (not `skipped`), that the hidden-return-pointer write (`mov [rax - 8], rcx`), the by-pointer argument `lea`, and the parameter copy-in read (`mov rcx, [rax + disp]`) appear in the code, and that heap-containing aggregates skip with clear reasons. An aggregate-returning function with four visible parameters (five effective register arguments, the fifth spilling to the stack) is now compiled rather than skipped (`aggregate_return_with_four_params_uses_a_stack_argument`).

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
- Marshalling: an extern's parameters and return type may be any **scalar C type** — every fixed-width integer (`i8`…`u64`, `isize`/`usize`) plus `bool` (`_Bool`, 0/1), `char` (`uint32_t`), `byte` (`uint8_t`), and the floats `f32` (`float`) / `f64` (`double`) — plus a **raw pointer** `ptr<T>` (a C `T*`, a machine-address word), the FFI-only **`cstr`** parameter marker (a Lullaby `string` materialized into a NUL-terminated `const char*`), and a **`void`** return. Arguments are routed by **position and type** with **no argument-count cap**: the first four use integer registers (`rcx`/`rdx`/`r8`/`r9`) for integers/pointers/`cstr` and `xmm N` for a float at position N (each position consuming its slot in exactly one register sequence, so `ldexp(double, int)` sends `double`→`xmm0`, `int`→`rdx`); the 5th+ argument spills onto the stack above the 32-byte shadow space (§ "Pointer, `cstr`, and >4-argument marshalling" below). The result is read from `rax` (integer/pointer, with a **narrow C return re-normalized in `rax`** — sign-extend for signed kinds, zero-extend for unsigned; `i64`/64-bit/pointer kinds a no-op) or from `xmm0` (a `f32`/`f64` return, `movsd`/`movss`), so downstream Lullaby code sees the same cell/value the interpreters produce. The extern C-ABI signatures are carried through the IR/bytecode as `extern_signatures` (serde-defaulted for artifact compatibility) so the native emitter can marshal each width. Struct-by-value parameters/returns and callback (function-pointer) parameters are **not** marshallable and are rejected at check time with `L0424` (they are the deferred FFI tail, § below).

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

### Pointer, `cstr`, and >4-argument marshalling (DELIVERED)

FFI now reaches beyond scalars — the universal escape hatch for OS APIs.

- **Raw pointer arguments/returns.** A `ptr<T>` (or the legacy `ptr_T` spelling `alloc` produces) marshals to a C `T*`: a single 64-bit machine-address word passed/returned in a GPR with no narrow normalization. Native pointers are real bump-heap/`malloc` addresses, so a Lullaby-held pointer round-trips through C unchanged. `resolve_native_type` maps `ptr<T>` to the `i64`-class word, so a pointer local/parameter/return reuses every scalar path; `ffi_scalar_class` classes it `Int(None)`.
- **`cstr` (NUL-terminated C string).** A `cstr` parameter accepts a Lullaby `string`; the caller evaluates the string to its heap record pointer and calls the `__lullaby_to_cstr` runtime helper, which bump-allocates `byte_len + 1` bytes, copies the UTF-8 bytes, and appends a NUL — a `const char*` the callee borrows for the call. An interior NUL is copied verbatim (standard `char*` truncation semantics). `cstr` is a parameter-only marker: an inbound C string is received as `ptr<byte>` and copied explicitly (owned-string conversion is deferred). The type checker admits a `string` argument only where an `extern`'s parameter is declared `cstr`.
- **More than four arguments.** `emit_extern_call` has no argument-count cap: the first four arguments use the Win64 registers and the 5th+ spill into the outgoing stack-argument area above the 32-byte shadow space, at `[rsp + 32 + 8·(pos−4)]` at the `call` — the same area (and the same `max_outgoing_stack_words` frame reservation) an internal >4-argument call uses, so the extern caller and an `export fn` callee agree bit-for-bit.

Verification (native link+run against `ucrt`, gated like the other `ffi_*` tests; sources MSVC's `LIB` when unset):

- `tests/fixtures/native_only/ffi_cstr_strlen.lby` — `extern fn strlen s cstr -> usize`, `strlen("lullaby")` exits `7` (a Lullaby `string` round-trips to C as a `char*`). `ffi_cstr_marshals_string_to_c_when_linkable`.
- `tests/fixtures/native_only/ffi_ptr_roundtrip.lby` — `malloc(16) -> ptr<byte>`, `strcpy(p, "hello") -> ptr<byte>` (a `cstr` source), `strlen(p) -> usize`; a Lullaby-controlled C pointer passes through three C calls, exiting `5`. `ffi_pointer_round_trips_through_c_when_linkable`.
- `tests/fixtures/native_only/ffi_extern_sum6.lby` + `ffi_export_sum6.lby` — a six-`i64` extern call linked against a six-`i64` `export fn` library object; `lullaby_sum6(1,2,4,8,16,32)` exits `63`, verifying the extern caller spills its 5th/6th arguments exactly where the export callee reads them (no C compiler needed — `rust-lld` links the two Lullaby objects). `ffi_extern_call_with_stack_args_when_linkable`.
- Native unit tests `cstr_extern_materializes_string_and_emits_helper`, `pointer_extern_param_and_return_compile`, and `six_arg_extern_call_spills_fifth_and_sixth_to_stack` in `crates/lullaby_ir/src/native_object.rs` assert the `__lullaby_to_cstr` helper is emitted, pointer params/returns compile (not skipped), and the 5th/6th arguments write to `[rsp+0x50]`/`[rsp+0x58]`. Semantics tests `validates_extern_with_pointer_and_cstr_signature`, `rejects_extern_with_nonmarshallable_param`, and `rejects_extern_callback_parameter` cover the `L0424` signature gate.

### Deferred FFI work

Delivered for **calling** C via `extern fn`: the full scalar set (`i8`…`u64`, `isize`/`usize`, `bool`, `char`, `byte`, `f32`, `f64`), raw pointers (`ptr<T>`), `cstr` string arguments, `void` returns, and any argument count. Still deferred (rejected cleanly at check time with `L0424`, never miscompiled): **struct-by-value** parameters/returns and **callbacks** (passing a Lullaby `fn` as a C function pointer, e.g. to `qsort`) — these are the ABI-fiddly FFI tail (register/eightbyte classification for aggregates; a trampoline and lifetime model for closures) and are not verifiable on this Windows host without a discovered C toolchain producing structs/callbacks to link against, so they are deferred rather than shipped unverified. `repr(C)` struct layout is additionally not yet parseable. Also deferred: an inbound owned `cstr`→`string` conversion, variadic C functions, and non-Windows C ABIs (System V on Linux/macOS). Exposing Lullaby functions **to** C is delivered for the `i64`/`f64`/`f32` scalar set (widening the `export` marshalling to pointers/other widths is a follow-up) — see below.

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

## Growable `list<T>` (scalar or `string` element) (DELIVERED)

`emit_alpha1_native_program` compiles the core growable-list operations for a **scalar element type** (`i64`, every fixed-width integer, `bool`/`char`/`byte`, and `f32`/`f64`) **or a `string` element**: `list_new`, `push`, `get`, `set`, `len`, and `pop`. This mirrors the WASM backend's `list<T>` support (including `list<string>`) and agrees with the AST/IR/bytecode interpreters bit-for-bit, **including value semantics**. A `string` element is a single immutable heap pointer stored in one slot exactly like a scalar and, because strings are immutable, is shared (its pointer copied by value, never deep-recursed into the string record) on the value-semantic deep copy — matching the interpreters' cheap shared `Value::String` clone. It is additive: a function that also uses lists now compiles; a `list<struct>`/`list<list<…>>`/`list<map<…>>` (a **MUTABLE heap element**) is DEFERRED — it would need a recursive per-element deep copy, so the function skips gracefully with a clear reason and still runs on the interpreters, never miscompiled.

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
- **`__lullaby_list_copy(rcx = src) -> rax = copy`** — allocates a block with the source's `cap`, copies the `len`/`cap` headers and the `len` live element words (a flat 8-byte word copy is an exact deep copy — a list element is a scalar or a `string` pointer, and a `string` is immutable so sharing its pointer IS its value-semantic copy, so no per-element type dispatch is needed).
- **`__lullaby_list_grow(rcx = list) -> rax`** — returns the list unchanged when `len < cap`; otherwise allocates a block with `new_cap = (cap == 0 ? LIST_INITIAL_CAP : cap * 2)`, copies the live elements and the `len` header, writes the new `cap`, and returns the fresh block (orphaning the old one).

Call-site lowering: `push(l, x)` deep-copies `l`, grows the copy, stores `x` into slot `len`, and bumps `len`; `set(l, i, x)` deep-copies `l` and stores `x` into element slot `i`; `pop(l)` deep-copies `l` and decrements its `len`; `get(l, i)` loads element `i` (a float element is moved back into `xmm0`; a `string` element loads its pointer word into `rax`); `len(l)` loads the `len` header word. A float element's value is moved through a GPR (`movq`/`movd`) so the 8-byte store/load preserves its exact bits; a `string` element is stored/loaded as a flat pointer word through `rax` (the `push`/`set` value expression — a literal, `+` concat, or `to_string` — already leaves the string record pointer in `rax`). **Bounds are not checked** for `get`/`set` in native code (consistent with native array indexing): an out-of-range index reads/writes past the live elements rather than trapping; the interpreters still bounds-check with `L0413`, so a program relying on trapping must run on an interpreter.

### Deferred list work

Deferred beyond this increment: **lists of MUTABLE heap elements** (`list<struct>`/`list<list<…>>`/`list<map<…>>`) — the element deep-copy would need a recursive per-element type dispatch; trapping native bounds checks for `get`/`set`; and a heap `free`/reclamation path (grown/copied blocks are orphaned). (`list<string>` is now **delivered** — a `string` element is immutable, so it is shared on the flat word copy; and scalar-key/value `map<K, V>` and `map<K, string>` are **delivered** — see the map section.)

### Verification

The fixtures `tests/fixtures/valid/native_list_build.lby` (build a `list<i64>` via `list_new`/`push` past the initial capacity, then `set`/`get`/`len`/`pop`; `main` returns 134) and `tests/fixtures/valid/native_list_value_semantics.lby` (an aliasing check: `let b = a` then `push(b, …)`/`set(b, …)` must not affect `a`, plus a list-parameter `mutate`; `main` returns 56) run on all interpreter backends for ground truth and are native-compiled, linked, and run by `native_list_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which sources MSVC's `LIB` when unset and asserts each `.exe`'s exit code equals the interpreter's `run` result (mod 256; gated on `rust-lld` + `kernel32.lib`, with the compile-not-skip and interpreter-truth assertions always running). The **`string`-element** case adds `tests/fixtures/valid/native_list_string.lby` (a `list<string>` with literal/`+`-concat/`to_string` elements, `get`/`len`/`set`/`pop`, a value-semantics probe across a call boundary; `main` returns 31), native-compiled/linked/run by `native_string_collections_execution_parity_when_linkable` (same gating and mod-256 assertion). Unit tests in `native_program_tests` assert that list functions (including a `list<i64>`-returning function, a list parameter, a `list<f64>`, and a `list<string>` returning/parameter function) report `compiled` (not `skipped`), that the object emits the `__lullaby_list_new`/`_copy`/`_grow` helper symbols and the bump allocator + `.bss` heap, that a `push` call site references the copy and grow helpers, that a `list<string>` still references the copy/grow helpers (its string pointer shared on the flat copy), and that a MUTABLE-heap-element `list<array<i64>>` skips with a clear reason.

## Growable `map<K, V>` (scalar key; scalar or `string` value) (DELIVERED)

`emit_alpha1_native_program` compiles the core growable-map operations for an **integer-cell key** and a **scalar or `string` value**: `map_new`, `map_set`, `map_get`, `map_has`, and `map_len`. This mirrors the WASM backend's `map<K, V>` increment (including `map<K, string>`) and agrees with the AST/IR/bytecode interpreters bit-for-bit — an **insertion-ordered association list** scanned linearly with value equality — **including value semantics**. A `string` value is a single immutable heap pointer stored in the entry's value word exactly like a scalar and, because strings are immutable, is shared (its pointer copied by value, never deep-recursed) on the flat two-word entry copy. It is additive: a function that also uses maps now compiles; a `map<string, V>` (a **heap KEY**), a `map<K, list<…>>`/`map<K, struct>` (a **MUTABLE heap value**), or a **float-key** map is DEFERRED — the function skips gracefully with a clear reason and still runs on the interpreters, never miscompiled.

### Map layout

A `map<K, V>` value is a **heap pointer** — one 8-byte word, held in an integer register (or a frame slot) exactly like a scalar pointer or a native list. It points at a header:

- **`[len i64]`** at offset `0` — the current entry count.
- **`[cap i64]`** at offset `8` — the allocated capacity, in entries.
- **`[entries…]`** starting at offset `16` — `cap` two-word entries. Entry `i` lives at `16 + i * 16`; its **key** word is at `+0` and its **value** word at `+8`. Every field is an 8-byte word (uniform with the native list/struct/enum slots), so a scalar value — including an `f32`/`f64`, stored bit-for-bit — or a `string` pointer value moves as a flat word.

`map_new()` allocates an empty block with `len = 0`, `cap = 4` (`MAP_INITIAL_CAP`), so a handful of inserts do not each reallocate. Because `MAP_DATA_OFF == LIST_DATA_OFF == 16`, the shared word-copy loop copies map entry words too (a map with `len` entries copies `2 * len` words).

### Ordering, lookup, and key equality

The map is an **insertion-ordered association list**: `map_set` overwrites the value of an existing key **in place** (preserving its position) or appends a new `(key, value)` entry at the end; `map_get`/`map_has` scan entries **front-to-back** so the first matching key wins; `map_len` reads the `len` header. This matches the interpreters' `Value::Map` (a `Vec<(Value, Value)>`) bit-for-bit. Key equality is an exact **8-byte word compare** (`cmp`), which is exact for the integer-cell key types (`i64`, every fixed-width integer, `bool`/`char`/`byte` — all stored as normalized `i64` cells). Values may be any native scalar, **including a float** (stored/loaded bit-for-bit, never compared), **or a `string`** (an immutable heap pointer stored/loaded as a flat word, shared on the entry copy, never compared).

### Value semantics

Lullaby maps are value-semantic (`m = map_set(m, k, v)` returns a NEW map). The only mutator, `map_set`, first **deep-copies** its source map (via `__lullaby_map_copy`) and mutates the fresh copy; the read ops `map_get`/`map_has`/`map_len` never mutate. Because the mutator always copies, sharing a map pointer across a `let b = a` binding or a call boundary is safe without any extra copy — a map parameter/return/argument passes as a plain pointer word in an integer register, exactly like a native list. The bump heap never reclaims, so a grown or copied map orphans its old block.

### Runtime helpers and codegen

Four `.text` runtime helpers back the small inline call-site codegen (emitted alongside the list + string helpers whenever the heap path runs):

- **`__lullaby_map_new() -> ptr`** — bump-allocates a `[0][MAP_INITIAL_CAP][entries]` block via `__lullaby_alloc` and returns its pointer.
- **`__lullaby_map_copy(rcx = src) -> rax = copy`** — allocates a block with the source's `cap`, copies the `len`/`cap` headers and the `2 * len` live entry words (a flat word copy — a value that is a `string` pointer is shared, not deep-recursed, since strings are immutable).
- **`__lullaby_map_grow(rcx = map) -> rax`** — returns the map unchanged when `len < cap`; otherwise allocates a block with `new_cap = (cap == 0 ? MAP_INITIAL_CAP : cap * 2)`, copies the live entries and the `len` header, writes the new `cap`, and returns the fresh block (orphaning the old one).
- **`__lullaby_map_find(rcx = map, rdx = key) -> rax`** — linear-scans the entries front-to-back for the first key equal to `rdx`, returning its index, or the map's `len` if no key matches (the "found index else len" convention). Clobbers only `rax`/`r10`/`r11` and preserves `rcx`/`rdx`, so callers reuse the map pointer and key across the call.

Call-site lowering: `map_set(m, k, v)` deep-copies `m`, scans the copy; if the key is found it overwrites that entry's value slot in place, else it grows the copy when full and appends the `(k, v)` entry, bumping `len`, then returns the fresh map pointer. `map_get(m, k)` scans and materializes `some(v)` (loading the found entry's value slot) or `none` into the **native enum/option layout** (a tag word plus payload word), reachable both as a `let`/assign initializer and as a `match` scrutinee. For a `map<K, string>` this yields `option<string>` — the `some` payload slot holds the found value's string pointer. `map_has(m, k)` scans and yields a `bool` (`found != len`). `map_len(m)` loads the `len` header word. A float value is moved through a GPR (`movq`/`movd`) so the 8-byte store/load and the `some(v)` payload preserve its exact bits; a `string` value is stored/loaded as a flat pointer word (the `map_set` value expression already leaves the string record pointer in `rax`).

### Deferred map work

Deferred beyond this increment: **string/heap KEYS** (`map<string, V>`) — string-key equality needs the string heap for content comparison (a byte-level compare of decoded strings, not the interned pointer), matching the WASM map — **MUTABLE heap values** (`map<K, list<…>>`/`map<K, struct>`/…), which would need a recursive per-value deep-copy dispatch — **float keys** (`map<f64, V>`/`map<f32, V>`, whose bit-pattern word compare would diverge from the interpreters' value equality on `±0.0`/NaN); the `map_keys`/`map_values`/`map_del` builtins; and a heap `free`/reclamation path (grown/copied blocks are orphaned). (`map<K, string>` is now **delivered** — a `string` value is immutable, so it is shared on the flat entry copy.)

### Verification

The fixtures `tests/fixtures/valid/native_map_build.lby` (build a `map<i64, i64>` via `map_new`/`map_set` past the initial capacity, update an existing key, then read via `map_get`/`map_has`/`map_len`; `main` returns 155) and `tests/fixtures/valid/native_map_value_semantics.lby` (an aliasing check: `let b = a` then `map_set(b, …)` must not affect `a`, plus a map-parameter `probe`; `main` returns 99) run on all interpreter backends for ground truth and are native-compiled, linked, and run by `native_map_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which sources MSVC's `LIB` when unset and asserts each `.exe`'s exit code equals the interpreter's `run` result (mod 256; gated on `rust-lld` + `kernel32.lib`, with the compile-not-skip and interpreter-truth assertions always running). The **`string`-value** case adds `tests/fixtures/valid/native_map_string.lby` (a `map<i64, string>` with literal/`to_string`/`+`-concat values, `map_set` insert + update-in-place, `map_get` → `option<string>` matched, `map_has`/`map_len` across a call boundary; `main` returns 23), native-compiled/linked/run by `native_string_collections_execution_parity_when_linkable` (same gating and mod-256 assertion). Unit tests in `native_program_tests` assert that map functions (including a `map<i64, i64>`-returning function, a map parameter, a `match map_get(...)`, a `map<i64, f64>` float value, and a `map<i64, string>` build/probe pair) report `compiled` (not `skipped`), that the object emits the `__lullaby_map_new`/`_copy`/`_grow`/`_find` helper symbols and the bump allocator + `.bss` heap, that a `map_set` call site references the copy and find helpers, that a `map<i64, string>` still references the copy/find helpers, and that a string-key `map<string, i64>` and a MUTABLE-heap-value `map<i64, array<i64>>` skip with clear reasons.

## MUTABLE-heap collection elements and enum payloads (DELIVERED)

`emit_alpha1_native_program` compiles growable collections and enums whose element/value/payload is a **one-level MUTABLE aggregate** — a `struct` or a nested `list<scalar|string>` — with correct value semantics, mirroring the WASM backend's `list<struct>`/`list<list<scalar>>`/`map<K, struct>`/`option<struct>` increment. Previously these deferred because the collection's value-semantic deep copy flat-copied each element/value word, which would SHARE a mutable element's pointer and let a mutation of one copy leak into another. Now the copy **recurses**, matching the interpreters' recursive `Value::clone` bit-for-bit.

### Heap-struct element representation

The native backend lays a struct **local/parameter/return** out stack-flattened (one frame word per field). But a collection element slot is a single 8-byte word, so a struct used as a collection element/value/enum payload is laid out on the **heap** instead: a `[nwords i64]` header followed by `nwords` field words (one 8-byte word per field, in declared order). The element-slot value is a pointer to **field 0** (`alloc_base + 8`), so field `k` is at `[ptr + 8*k]` and the word count sits at `[ptr - 8]`. Storing the count in the block lets a single type-agnostic `__lullaby_struct_copy(rcx = src) -> rax` deep-copy any heap struct — its fields are always scalars or shared immutable strings at the one-level nesting bound, so a flat word copy IS an exact deep copy. A nested `list<…>`/`map<…>` element is already a single heap-pointer word (its own representation) and is deep-copied via `__lullaby_list_copy`/`__lullaby_map_copy`.

Classification is bounded by `MAX_COLLECTION_NEST_DEPTH = 1` (`native_collection_slot`, the native mirror of WASM's `collection_slot_type`): depth 0 is the collection's own element/value slot, and a struct field or a nested list element consumes one level, so `list<struct>`, `list<list<scalar>>`, `map<K, struct>`, and `option<struct>` are accepted while `list<list<list<…>>>`, `list<map<…>>`, a `map`/`array`-typed element, and a struct field that is itself a list/map/struct are rejected (the enclosing function skips gracefully, never miscompiled).

### Recursive deep copy (value semantics)

The type-agnostic `__lullaby_list_copy`/`_grow` and `__lullaby_map_copy` helpers still flat-copy element/entry words. A **per-element deep-copy fixup** is emitted inline at each mutating site after the flat copy: it walks the copied collection's `len` live elements (or entry values) and replaces each mutable slot with an INDEPENDENT deep copy of itself (`emit_list_deep_fixup`/`emit_map_deep_fixup` → `emit_heap_slot_deep_copy`). So:

- **`get(l, i)` / `map_get(m, k)`** return an independent deep copy of a mutable element/value (the interpreters' `values[i].clone()`), so mutating the retrieved copy never touches the collection.
- **`push` / `set` / `map_set`** deep-copy both the copied collection's existing elements and the incoming value (a struct constructor `Point(1, 2)` is built fresh on the heap — already independent; any other aggregate expression is evaluated and deep-copied), so a later mutation of the source value never leaks into the collection.
- **`pop`** deep-copies the copied list's remaining elements.
- **enum construction** (`some(v)` / `err(m)` / `Variant(v)`) stores an independent copy of a mutable-aggregate payload; **`match`** binds a `HeapStruct` payload by bridging it into a fresh STACK `Struct` local (a flat field-word copy `[ptr + 8*k]` → the arm local's slots), so the bound value is an independent snapshot that field access and the by-pointer call ABI consume directly; a nested `list`/`map` payload binds its deep-copied pointer.

### Heap↔stack bridge

Because a struct value is stack-flattened everywhere except in a collection slot, a `get(list<struct>, i)` (or a `some(struct)` `match` binding) that flows into a `Struct`-typed local, call argument, or field access is bridged from the heap representation to the stack representation: `lower_aggregate_init` (for a `let p Struct = get(…)`) and the `match` payload binding flat-copy each heap field word `[ptr + 8*k]` into the destination stack slots. Field access directly on an unbound `get(…)` result (`get(xs, 0).v` without a local) stays deferred (native field access must be rooted at a local), so it skips gracefully.

### Deferred

Nesting past one mutable level (`list<list<list<…>>>`), a `map`/`array` as a collection element/value, a mutable-aggregate map KEY (the semantic layer already restricts keys to `i64`/`string`), `map_keys`/`map_values`/`map_del`, and field access directly on an unbound `get`/`map_get` result. None of these miscompile; the enclosing function runs on the interpreters.

### Verification

The fixture `tests/fixtures/valid/native_list_struct.lby` builds a `list<Point>` (push structs, `get` a field via `point_sum(get(ps, i))`, `set` an element, cross a `grow_probe` boundary), a `list<list<i64>>` (nested deep copy summed by `nested_sum`), a `map<i64, Point>` (insert + update-in-place + `map_get` matched via `map_point_value`), and — crucially — a **value-semantics probe**: `get` a struct element, mutate the retrieved copy (`mutated.x = 100`), re-`get` the same index, and confirm the original element is unchanged (`elem_unchanged`). It runs identically on the AST/IR/bytecode interpreters (deterministic `main = 96`, auto-discovered parity harness) and is native-compiled, linked, and run by `native_list_struct_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which asserts every function compiles natively (not skipped) and — when `rust-lld` + `kernel32.lib` are available (sourcing MSVC's `LIB` if unset) — the `.exe` exit code equals `96`. Unit tests in `native_program_tests` assert that a `list<struct>`, a `list<list<i64>>`, a `map<i64, struct>`, and a `result<i64, list<i64>>` all report `compiled` (not `skipped`), that the object emits the `__lullaby_struct_copy` helper (which allocates a fresh block via the bump allocator — the machine-code proof the element deep copy is recursive) and that list/map deep copies reference the copy helpers, and that a `list<map<…>>` element and an over-deep `result<i64, list<list<list<i64>>>>` payload skip with clear reasons.

## First-class heap `string` values (DELIVERED)

`emit_alpha1_native_program` compiles first-class heap `string` **values**: a string is now a real value that can be a local, a parameter, a return, and a call argument, built by string literals, runtime `+` concatenation, and `to_string`, and read by `len`. This mirrors the WASM backend's string design and agrees with the AST/IR/bytecode interpreters bit-for-bit (including UTF-8 char counting). It is additive: a string-free native program keeps its exact prior layout, and a function using an unsupported string feature skips gracefully to the interpreters — never miscompiled.

### String layout

A `string` value is a **heap pointer** — one 8-byte word, held in an integer register (or a frame slot) exactly like a native list/map pointer. It points at a record:

- **`[char_len i64]`** at offset `0` — the Unicode scalar (char) count.
- **`[byte_len i64]`** at offset `8` — the UTF-8 byte length.
- **`[utf8 bytes]`** starting at offset `16` — the encoded bytes.

The layout uses `i64` headers (versus the WASM backend's `i32` headers) so every field is a uniform 8-byte word, matching the native list/map/struct/enum slot discipline. `len(s)` reads the `char_len` header for **any** string value, so it is a Unicode scalar count for arbitrary UTF-8, not only ASCII.

### Value semantics (immutable — no copy)

Lullaby strings are **immutable**, so — unlike lists and maps — a string value needs **no deep copy** when bound (`let b = a`), passed as an argument, or returned: sharing the record pointer is already value-equivalent (exactly the interpreters' behavior and the WASM backend's, which also never copies a string argument). A string therefore crosses a function boundary as a plain pointer word in an integer register, never as a by-pointer aggregate. It is classified as a scalar (register value), not an aggregate, in the signature eligibility check. The bump heap never reclaims, so a concatenation or conversion orphans no-longer-referenced records, like every other native heap value.

### Runtime helpers and codegen

Ten `.text` runtime helpers back the small inline call-site codegen (emitted alongside the list/map + string-constant helpers whenever the heap path runs). The five constructors build a fresh record via `__lullaby_alloc`; the five index-based operations scan the existing records (only `substring` allocates):

- **`__lullaby_str_lit(rcx = .rdata ptr) -> rax`** — materializes a string **literal** value: scans the interned NUL-terminated `.rdata` bytes for the byte length and the UTF-8 char count (a byte is a char boundary when `(b & 0xC0) != 0x80`), allocates a record, writes the headers, and copies the bytes. The `.rdata` layout is unchanged (raw NUL-terminated bytes, shared with the existing `len("literal")` fast path), so a literal used as a value materializes through this helper at runtime.
- **`__lullaby_str_concat(rcx = a, rdx = b) -> rax`** — runtime `+` concatenation: allocates `16 + byte_a + byte_b`, sums the char/byte headers, and byte-copies both operands' UTF-8 ranges (so multi-byte text concatenates correctly). Mirrors the WASM concat.
- **`__lullaby_str_from_int(rcx = value, rdx = signed_flag) -> rax`** — integer `to_string`: a two-pass itoa (count digits, then write them backward into the fresh record). `signed_flag` nonzero formats a signed `i64` (leading `-` for a negative value, magnitude computed as a `u64` so `i64::MIN` formats correctly); zero formats an unsigned `u64`. `byte` uses the unsigned path; every fixed-width integer uses signed/unsigned per its kind, matching the interpreters' integer `Display`.
- **`__lullaby_str_from_bool(rcx = 0/1) -> rax`** — `to_string(bool)` = `"false"`/`"true"`, materialized from immediates (no `.rdata` constant needed).
- **`__lullaby_str_from_char(rcx = code point) -> rax`** — `to_string(char)`: UTF-8-encodes the Unicode scalar value into a one-char record (1–4 bytes, `char_len = 1`), matching Rust's `char` Display.
- **`__lullaby_str_substring(rcx = s, rdx = start, r8 = end) -> rax`** — char-indexed `[start, end)` slice. Bounds-checks exactly like the interpreters (`start < 0 || end < 0 || start > end || end > char_count`) and **traps with `ud2`** on a violation, mirroring the interpreters' `L0413` (never a wrong slice). Otherwise it maps the char indices to byte offsets by walking the UTF-8 (advance past one lead byte, then over all `(b & 0xC0) == 0x80` continuation bytes, per char), allocates `16 + slice_bytes`, writes the sliced `char_len`/`byte_len` headers, and byte-copies the slice. The only scan helper that allocates; keeps `rsp` 16-byte aligned at its internal `__lullaby_alloc` call.
- **`__lullaby_str_find(rcx = haystack, rdx = needle) -> rax`** — the **char** index of the first **byte-level** needle match, or `-1` if absent. Byte-searches every start `0..=(hay_len - needle_len)` for the first full match, then counts the non-continuation bytes before the matched byte offset (`text[..byte].chars().count()`). An empty needle matches at byte `0` (char index `0`). A leaf (no allocation).
- **`__lullaby_str_contains(rcx = s, rdx = sub) -> rax`** — byte-exact substring test (`0`/`1`): the same byte search as `find`, returning its found flag. An empty substring is contained. A leaf.
- **`__lullaby_str_starts_with(rcx = s, rdx = prefix) -> rax`** — byte-exact prefix test (`0`/`1`): `0` if `prefix_len > s_len`, else whether the prefix bytes match at byte `0`. An empty prefix matches. A leaf.
- **`__lullaby_str_ends_with(rcx = s, rdx = suffix) -> rax`** — byte-exact suffix test (`0`/`1`): `0` if `suffix_len > s_len`, else whether the suffix bytes match at byte `s_len - suffix_len`. An empty suffix matches. A leaf.

Call-site lowering: a **string literal value** lowers to `lea rcx, [rip + __str{i}]` (a REL32 relocation to the `.rdata` symbol) + `call __lullaby_str_lit`; `a + b` on two strings evaluates both to record pointers and calls `__lullaby_str_concat`; `to_string(x)` dispatches by the argument's type to the matching helper (identity on a `string` — the argument's pointer is already the value); `len(s)` on any string value loads the `char_len` header word. `substring(s, start, end)` stages `s`/`start`/`end` into `rcx`/`rdx`/`r8` and calls `__lullaby_str_substring`; `find`/`contains`/`starts_with`/`ends_with` stage their two string operands into `rcx`/`rdx` and call the matching helper (leaving an `i64` char index or a `0`/`1` bool in `rax`). Each is guarded by its argument types (string operands, i64 indices) so a same-named user function still resolves as an ordinary call. `bool`/`char` literal values also lower (as normalized `i64` cells) so `to_string(true)`/`to_string('x')` work. A function using a string value reports a `call` to the frame planner (so it reserves Win64 shadow space and stays 16-byte aligned at each helper `call`).

### Deferred string work

Deferred beyond this increment: **`to_string(f64)`/`to_string(f32)`** (needs dtoa) — such a function skips gracefully to the interpreters; the remaining **string builtins** (`replace`/`upper`/`lower`/`split`/`join`/`chars`/`string_from_chars`); **string indexing** and **string comparison** (`==`/`<`); **string map KEYS** (`map<string, V>` — content-comparison equality needs the string heap, matching the WASM map); and **strings as struct/array fields/elements** (a heap-value field inside a struct/array aggregate is rejected so the enclosing function skips — the aggregate copy paths move flat words and would not deep-copy a mutable heap field; a string field is immutable but still out of this increment's aggregate-of-heap scope). None of these miscompile. (Index-based `substring`/`find`/`contains`/`starts_with`/`ends_with`, **`string` `list`/`map` elements/values** (`list<string>`, `map<K, string>`), and **`string` enum payloads** (`option<string>`, `result<i64, string>`, string-payload user enums) are now **delivered** — see the list/map/enum sections above.)

### Verification

The fixture `tests/fixtures/valid/native_string_build.lby` (a `greeting` function that returns a concatenated string, a `measure` function that takes a string and returns its `len`, and a `main` that builds strings via `+`, `to_string(i64)`, and `to_string(bool)`, passes a string across a boundary, and derives `17`) runs on all interpreter backends for ground truth and is native-compiled, linked, and run by `native_string_build_execution_parity_when_linkable` in `crates/lullaby_cli/tests/cli.rs`, which sources MSVC's `LIB` when unset and asserts the `.exe` exit code equals the interpreter's `run` result (mod 256; gated on `rust-lld` + `kernel32.lib`, with the all-functions-compile and interpreter-truth assertions always running). The index-based string operations add `tests/fixtures/valid/native_string_ops.lby` (char-indexed `substring`/`find` over the multi-byte `"café"` — where `é` is 2 bytes — an empty needle, present/absent `find`, and true/false cases of every predicate, combined into `11`), run identically on the AST/IR/bytecode interpreters and native-compiled, linked, and run by `native_string_ops_execution_parity_when_linkable` (same MSVC/`rust-lld`/`kernel32` gating and mod-256 exit-code assertion; the all-fifteen-functions-compile-natively and interpreter-truth assertions always run). The existing `native_strings.lby` (`len("literal")`) fixture and its parity test still pass unchanged. Unit tests in `native_program_tests` assert that string-value functions (a string return, a string parameter, `+` concat, `to_string`, `len`) report `compiled` (not `skipped`), that the object emits the `__lullaby_str_lit`/`_concat`/`_from_int`/`_from_bool`/`_from_char` **and** `_substring`/`_find`/`_contains`/`_starts_with`/`_ends_with` helper symbols and the bump allocator + `.bss` heap, that a concat call site references the concat helper, that every index-string call site references its helper (and the using function compiles natively), that the `substring` helper carries the `ud2` `L0413` trap and calls the allocator while the four scan helpers are leaf functions (no relocations, ending in `ret`), that a `string` is a scalar (not a by-pointer aggregate) with one word, and that `to_string(f64)` and a heap-field aggregate skip with clear reasons.

## Deferred Native Work

Deferred beyond the current increment: more than four **effective** register arguments (stack arguments; the count now includes a hidden aggregate-return pointer), a top-level `f64`/`f32` **scalar** parameter/return across a function boundary (needs XMM argument routing), aggregates (structs/arrays) containing heap values as boundary values, trapping native array bounds checks, `to_string(f64)`/`to_string(f32)` and the remaining string builtins (`replace`/`upper`/`lower`/`split`/`join`/`chars`/`string_from_chars`), string indexing/comparison, strings as struct/array fields/elements, string-keyed or float-keyed maps, a mutable-aggregate map KEY, collection elements/values or enum payloads nested past **one** mutable-aggregate level (`list<list<list<…>>>`) or of `map`/`array` type, field access directly on an unbound `get`/`map_get` result, heap allocation exposed beyond the delivered string/list/map/struct helpers, a heap `free`/reclamation path, cross-platform ELF/Mach-O object emission, CRT-driven `mainCRTStartup` entry, and true bare-metal (no-OS-import) freestanding. (First-class `string` **values** — locals/parameters/returns/arguments, literal values, `+` concatenation, `len`, `to_string` for integers/`bool`/`char`/`byte`, and the index-based `substring`/`find`/`contains`/`starts_with`/`ends_with` operations; scalar-field aggregates as parameters, returns, and call arguments; `f64`/`f32`/`bool`/`char`/`byte` scalar lowering within i64-signature functions; `match` over enums with a scalar, `string`, **or one-level mutable-aggregate** payload; growable `list<T>` with a scalar, `string`, **or one-level mutable-aggregate** (`list<struct>`, `list<list<scalar>>`) element; and growable `map<K, V>` with a scalar key and a scalar, `string`, **or one-level mutable-aggregate** (`map<K, struct>`) value are **delivered** — see the sections above.) This work must not bypass the AST runtime, typed IR validation, bytecode VM, or existing release verification.
