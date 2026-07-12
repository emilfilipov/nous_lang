# Lullaby (lullaby) - Complete Language Specification

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Lullaby is a next-generation compiled systems programming language designed with three fundamental goals:

1. **Minimalistic Syntax**: No squiggly brackets, semicolons, or other noise. Clean, readable syntax optimized for human understanding.
2. **Token Efficiency**: Minimal token expenditure during generation by LLMs, enabling smaller models to generate correct code.
3. **LLM-Friendly Design**: Simple enough that even tiny language models (<1B parameters) can understand and write in the language without issues.

**Target Use Case**: Systems programming for operating system development and other low-level applications requiring type safety, performance optimization, and memory efficiency.

## Status

Lullaby is a real, working language in active development toward its 1.0 release. It is **not** a frozen prototype: the Rust toolchain compiles and runs a broad, cohesive language across multiple backends today, and the remaining work is a defined set of primitives on the road to 1.0. The plan and the definition of 1.0 are canonical in [roadmap_1_0.md](roadmap_1_0.md); the earliest installable surface is preserved for reference in [language_surface.md](language_surface.md).

1.0 is not yet released. Lullaby 1.0 is defined as being technically capable of expressing any program (shipping the spanning set of low-level primitives) *and* being easy to install across Windows, Linux, and macOS. This document describes the language as it exists now, and marks genuinely-planned work as roadmap items.

### Currently implemented

The language today includes:

- **Source and scope.** `.lby` source files, indentation-only blocks (curly braces and semicolon terminators are compile errors), and a canonical formatter (`lullaby fmt`).
- **Functions.** `fn name param Type -> ReturnType` (the `-> ReturnType` clause is **optional** — the return type is inferred from the body when omitted, except for a recursive function, which must state it), last-expression return with explicit early `return`, `-> void` procedures, **first-class function values** (`fn(T) -> R` types; passing/returning functions), and **generic functions** (`fn name<T> ...` and bounded `<T: Trait>`) with call-site inference. `async fn`/`await`/`Future<T>` are supported on the interpreter backends.
- **Types and inference.** Scalars `i64`, `f64`, `bool`, `char`, `byte`, `string`, and `void`; local-binding type inference; nominal **structs** (positional and named construction, field access/mutation, UFCS methods); **enums** (tagged unions) with exhaustive **`match`**; built-in generic enums **`option<T>`** and **`result<T, E>`** with the postfix **`?`** error-propagation operator; **`list<T>`** (growable) and **`map<K, V>`** (hash map) with iteration and `sort`.
- **Traits and generics.** `trait` declarations, `impl Trait for Type`, receiver-type method dispatch, and trait bounds on generic functions (`<T: Ord>`).
- **Modules.** File-as-module with `import NAME` and `pub` exports, multi-file projects via a `lullaby.json` manifest with local path dependencies.
- **Control flow.** `if`/`elif`/`else`, `while`, range `for`, `loop`, `break`, `continue`, and `throw`/`try`/`catch` structured error handling, plus the **inline conditional** expression `THEN if COND else ELSE` (a Python-style ternary; scalar or `string` result).
- **Operators.** Arithmetic, comparison, logical `and`/`or`/`not`, the **membership** operator `VALUE in COLLECTION` (a `string`'s char/substring or a `list<T>` element; yields `bool`), and full **bitwise** operators `& | ^ ~ << >>` plus bit intrinsics (`rotate_left`/`rotate_right`, `count_ones`, `leading_zeros`, `trailing_zeros`, `reverse_bytes`). Integer literals support `_` digit separators and `0x`/`0b`/`0o` base prefixes.
- **Concurrency.** Threads via `spawn`/`task_join`, `i64` channels (`chan_new`/`send`/`recv`/`try_recv`), a shared `Mutex`, **atomics** (`atomic_i64` with load/store/swap/CAS/fetch-ops), and data-parallel `parallel_map`.
- **Memory.** Heap-slot builtins (`alloc`/`load`/`store`/`dealloc`), reference counting (`rc<T>`/`ref<T>` with `rc_new`/`rc_clone`/`rc_release`/`rc_get`/`rc_borrow`/`ref_get`), and `unsafe`-gated raw pointer read/write.
- **Standard library / builtins (the prelude).** Numeric conversions and a rich **math** library (`abs`/`min`/`max`/`pow`/`sqrt`/`floor`/`ceil`/`round` plus transcendental `sin`/`cos`/`tan`/`atan`/`atan2`/`exp`/`ln`/`log10`); **string** operations (`substring`/`find`/`contains`/`split`/`join`/`trim`/`replace`/`upper`/`lower`/`starts_with`/`ends_with`/`repeat`; `+`/`+=` concatenate two strings or a string and a `char`; slice syntax `s[start:end]`, `s[start:]`, `s[:end]`, `s[:]` yields a substring over the half-open char range; **string interpolation** `"a=${expr}"` embeds any `to_string`-able value — parse-time sugar that `lullaby fmt` normalizes to explicit `to_string`/`+` concatenation) and **string↔bytes/UTF-8** primitives (`to_bytes`/`from_bytes`/`byte_len`); number parsing (`parse_i64`/`parse_f64`); `list`/`map` utilities; and an `assert` testing builtin (with a `lullaby test` runner). The full catalog is in [standard_library.md](standard_library.md).
- **Platform I/O.** Text and binary file I/O, directory operations, TCP/UDP sockets, an **HTTP client and server**, process/environment access (`env`/`args`), time/clock (`mono_now`/`wall_now`/`sleep_millis`), and OS randomness (`os_random`).
- **Backends.** Five execution/codegen paths at parity: an **AST interpreter** (default), a **typed IR interpreter**, a **bytecode VM** (with a versioned `.lbc` artifact and a deterministic optimizer), a **WebAssembly** backend (scalar subset plus heap types in linear memory and JS/DOM host imports), and a **native x86-64** backend (COFF object + linking to a Windows `.exe`, with a **freestanding/no-std mode**, **inline assembly**, C-callable exports, and C-ABI FFI declarations). The interpreter grammar is drafted in [formal_grammar.md](formal_grammar.md).
- **Tooling.** A CLI (`check`/`compile`/`build`/`run`/`test`/`wasm`/`native`/`fmt`/`inspect`/`lsp`/`docs`/`examples`), a Language Server (`lullaby lsp`), and a self-contained offline HTML documentation bundle.

### On the roadmap to 1.0

The following are designed and planned but not yet fully implemented (see [roadmap_1_0.md](roadmap_1_0.md) for scope and order):

- Wider integer types (`i8`–`u64`, `usize`/`isize`) and `f32`, typed literal suffixes, and total conversion/cast rules.
- Environment-capturing closures; generic user *types*, trait objects/`dyn`, associated types, and default trait bodies.
- Broader FFI / C-ABI coverage across calling conventions, marshalling, callbacks, and header generation; native ARM64 and ELF/Mach-O output.
- The WASM heap/allocator phase and richer DOM interop; raw-memory completeness (`sizeof`/`alignof`/`offsetof`, volatile MMIO) and atomic memory orderings/fences.
- Packaging and ease-of-access work (installers, `lullaby new` scaffolding).

### CLI summary

Current commands: `lullaby check`, `compile`, `build`, `inspect`, `run` (with `--backend ast|ir|bytecode` and `--optimize none|constant-fold|dead-code|full`), `test`, `wasm`, `native` (with `--freestanding`/`--no-std`, `--debug`/`-g`), `fmt`, `lsp`, `docs`, `examples`, `help`, and `--version`. During development these are also available through `cargo run -p lullaby_cli -- ...`. `--format json` (alias `--diagnostic-format json`) selects deterministic JSON diagnostics.

## Design Material Notes

The sections below are the broader language and systems-programming design. Where a section shows syntax the current compiler does not yet accept (for example wider integer types or capturing closures), treat it as a roadmap illustration rather than current behavior; the "Currently implemented" list above and [standard_library.md](standard_library.md) are authoritative for what runs today.

## Language Philosophy

Lullaby rejects traditional design patterns that prioritize compiler convenience over code clarity:

- **No Braces**: Control blocks defined through indentation only (Python-inspired but simpler)
- **No Semicolons**: Line-based statements without terminator requirements
- **Flat Structure**: Single-level control flow instead of deep nesting
- **Type Inference**: Automatic type detection reducing explicit annotations
- **Reference Counting**: Automatic memory management without garbage collection pauses

## Core Language Components

### 1. Syntax Design (See: `lullaby_syntax_design.md`)

The syntax is intentionally minimal and predictable:

**Variables**:
```lullaby
type name = value   // Type prefix required for clarity
name               // Type inferred from context
```

**Functions**:
```lullaby
fn add x i64 y i64 -> i64
    x + y

fn log message string -> void
    io.println message
```

**Indentation-based Scoping**: Blocks defined by indentation levels only, no braces needed.

**Return Rule**: A non-void function returns the last reachable expression unless an explicit `return` exits earlier. A `void` function returns no value and may use bare `return` for early exit.

### 2. Memory Management (See: `lullaby_memory_management.md`)

Reference-counted memory with automatic lifetime management:

- No garbage collection pauses
- Deterministic object cleanup
- Region-based organization
- Type-safe operations

**Key Types** (current):
- `i64`, `f64` - Integer and floating-point numbers
- `string`, `char`, `byte` - Text and byte scalars
- `bool` - Boolean values
- `list<T>` - Growable lists (fixed `array<T>` literals also exist)
- `map<K, V>` - Key-value mappings
- `rc<T>`, `ref<T>`, `ptr<T>` - Reference-counted values, references, and raw pointers

### 3. Control Structures (See: `lullaby_control_structures.md`)

Flat control flow without nesting complexity:

**Conditionals**:
```lullaby
if condition
    then_statements
elif other_condition
    else_statements
else
    final_statements
```

**Loops**:
```lullaby
for variable from start to end [by step]:
    loop_body

while condition:
    loop_body

loop:
    unconditionally_executed_statements

break  // Exit loop
continue  // Skip remaining statements in iteration
```

### 4. Type System (See: `lullaby_type_system.md`)

Pragmatic type system for systems programming:

- **Static typing with inference**: Types are checked at compile time; local-binding annotations are inferred where the initializer type is unambiguous.
- **Nominal structs and enums**: `struct` records and `enum` tagged unions, decomposed with exhaustive `match`.
- **Generics and traits**: Generic functions with call-site inference, `trait`/`impl`, and trait bounds on type parameters.
- **Built-in generic enums**: `option<T>` and `result<T, E>` with the postfix `?` operator.

**Core scalar types** (current): `i64`, `f64`, `bool`, `char`, `byte`, `string`, `void`. Wider integer widths (`i8`–`u64`, `usize`/`isize`) and `f32` are on the roadmap.

### 5. I/O and Concurrency (See: `lullaby_input_output.md`)

Efficient systems programming primitives:

**File Operations**:
- Read/write with automatic buffering
- Type-aware file operations
- Memory-mapped access for large files
- Stream-based processing

**Concurrency Patterns**:
- Simple thread/async models
- Automatic synchronization
- I/O multiplexing
- Message queues and shared memory

## Planned Syntax Reference

### Primitives
```lullaby
# Boolean values

false, true

# Numeric literals

integer: 0, -123, +456
float: 3.14159, 2.71828

# String literals

"hello world"
'single quotes also supported'
"""multi-line strings"""

# None/null (no pointer dereferencing needed)

none  // Represents absence of value
```

### Operators

Implemented today:

**Arithmetic**: `+ - * / %` (`%` is integer remainder, truncated toward zero so it takes the sign of the dividend, like C/Rust; it requires two operands of the same integer type and is not defined on floats)
**Comparison**: `== != < > <= >=` (equality requires matching operand types; ordering is on numeric/char/byte operands)
**Logical**: `and or not` (short-circuiting)
**Assignment**: `= += -= *= /= %=` (compound operators on numeric locals; `%=` is integer-only)
**Bitwise**: `& | ^ ~ << >>`, plus bit intrinsics `rotate_left`/`rotate_right`/`count_ones`/`leading_zeros`/`trailing_zeros`/`reverse_bytes`
**Error propagation**: postfix `?` on `option`/`result`

Planned: `xor`, `^=`, and closure-based functional helpers (`map`/`reduce`/`filter`). `min`/`max`/`sum`-style helpers exist as math/collection builtins — see [standard_library.md](standard_library.md).

### Control Flow
```lullaby
if condition:
    statements

switch value:
    case pattern1:
        code_block
    case pattern2 when additional_condition:
        alternative_code

end_switch
```

### Functions
```lullaby
fn function_name param Type -> ReturnType
    # final expression is returned
    result_value

fn function_with_early_return value i64 -> i64
    if value < 0
        return 0

    value

fn side_effect message string -> void
    io.println message
```

### Structs and Objects
Structs are implemented as nominal records with indentation-only `name type` fields, positional and named construction, `.` field access and mutation, and UFCS methods (a `p.norm2()` call desugars to `norm2(p)`):
```lullaby
struct Point
    x i64
    y i64

fn norm2 p Point -> i64
    p.x * p.x + p.y * p.y

fn main -> i64
    let a Point = Point(3, 4)                  // positional
    let b Point = Point(y: 4, x: 3)            // named, any order
    a.norm2() + b.x                            // UFCS method + field access
```
See [struct_design.md](struct_design.md) for the full struct model.

### Collections
Fixed arrays, growable `list<T>`, and `map<K, V>` are implemented today:
```lullaby
let values array<i64> = [1, 2, 3]
values[0]              // bounds-checked indexing

let l list<i64> = list_new()
l = push(l, 10)        // push/get/set/pop/len, reverse/concat/slice, sort

let m map<string, i64> = map_new()
m = map_set(m, "a", 1) // map_get -> option<V>, map_has/map_len/map_del, map_keys/map_values
```
See [standard_library.md](standard_library.md) for the full collection builtin catalog. A closure-based `filter` awaits capturing closures.

### Memory Operations
```lullaby
alloc(size/type)      // Allocate memory
store(ptr, value)     // Replace value in allocated memory
dealloc(ptr)          // Free allocated memory
ref(ptr)             // Create reference copy
ptr(type)            // Create pointer
swap(a, b)           // Exchange values
duplicate(value)     // Deep copy
```

Current memory form:
```lullaby
fn main -> i64
    let ptr ptr_i64 = alloc(0)
    store(ptr, 41)
    let value i64 = load(ptr)
    dealloc(ptr)
    value + 1
```

### I/O Operations
Current text file and system command form:
```lullaby
fn main -> string
    write_file("target/example.txt", "alpha")
    append_file("target/example.txt", " beta")
    read_file("target/example.txt")
```

```lullaby
fn main -> i64
    sys_status("rustc", ["--version"])
```

Planned standard-library I/O forms:
```lullaby
io.read(path)         # Read entire file
io.readlines(path, max_lines=N)  # Read limited lines
io.open(path, mode)   # Open stream for reading/writing
io.write(path, data)  # Write to file

# Memory-mapped files

io.memory_map(path, size)
mm_data = mm_file.data_pointer
```

### Concurrency Primitives
Implemented today (see [concurrency_design.md](concurrency_design.md)):
```lullaby
# Threads + i64 channels
let ch Chan = chan_new()
let t Task = spawn(worker, ch)   # worker sends into ch
task_join(t)
let v i64 = recv(ch)             # also try_recv

# Shared mutex
let m Mutex = mutex_new()
mutex_add(m, 1)                  # mutex_get/mutex_set

# Atomics (SeqCst)
let a Atomic = atomic_new(0)
atomic_add(a, 1)                 # atomic_load/store/swap/cas/sub/and/or/xor

# Data parallelism (scoped threads)
let out list<i64> = parallel_map(square, inputs)
```
`async fn`/`await`/`Future<T>` also run on the interpreter backends. Generic channels, `select`, and cross-thread socket sharing are on the roadmap.

## Design Principles Summary

### 1. Minimalism
- Remove unnecessary syntax (no braces, no semicolons, no parentheses where not needed)
- Each line contains one clear operation
- Single keywords for common operations instead of verbose alternatives

### 2. Type Safety
- Compile-time type checking prevents errors before runtime
- Automatic inference reduces annotation burden
- Zero-cost abstractions maintain performance

### 3. Memory Efficiency
- Reference counting eliminates GC overhead
- Region-based organization improves cache locality
- Explicit lifetime management ensures determinism

### 4. LLM Optimization
- Predictable structure enables pattern-based generation
- Flat syntax reduces complexity for model understanding
- Limited keyword set (approx. 50 core keywords)

### 5. Systems Focus
- Designed specifically for systems programming needs
- Direct hardware abstraction without hidden layers
- Explicit memory and resource management

## Comparison with Existing Languages

| Feature | C/C++/Java | Python | Lullaby |
|---------|------------|--------|-----------|
| Block syntax | Brace-delimited blocks | Indentation | **Indentation only** |
| Statement terminator | Semicolon `;` | None (new line) | **No terminators** |
| Memory model | Manual/GC | GC (pause risks) | **Reference counting** |
| Type system | Static/Static | Dynamic | **Static+Inference hybrid** |
| Control flow | Nested blocks | Indentation | **Flat indentation** |
| Keyword count | 50-80 | 30-40 | **~25 core keywords** |

## Implementation Roadmap

The frontend (grammar, type system, semantic analysis), the runtime/VM, the standard-library builtins, the concurrency primitives, and the core data structures are already implemented and running across the backends described above. The remaining road to 1.0 — completing the spanning primitive set (wider numerics, FFI breadth, the WASM heap phase, raw-memory completeness) and making the toolchain easy to install everywhere — is planned in detail in [roadmap_1_0.md](roadmap_1_0.md). Granular tracking lives in the ClickUp `Lullaby` folder.

## Getting Started Examples

### Hello World
```lullaby
fn main -> string
    "Hello, Lullaby!"
```

### Simple Calculator
```lullaby
fn add x i64 y i64 -> i64
    x + y

fn main -> i64
    let value i64 = add(40, 2)
    value
```

### Branching
```lullaby
fn main -> i64
    if true
        42
    else
        0
```

### Loops
```lullaby
fn main -> i64
    let value i64 = 0
    while value < 4
        value += 1
    value
```

```lullaby
fn main -> i64
    let total i64 = 0
    for i from 1 to 3
        total += i
    total
```

```lullaby
fn main -> i64
    let value i64 = 0
    loop
        value += 1
        if value < 5
            continue
        break
    value
```

### Boolean Logic
```lullaby
fn main -> bool
    not false and true or false
```

### Arrays
```lullaby
fn main -> i64
    let values array<i64> = [2, 4, 6]
    values[2]
```

## Already Delivered From The Original Roadmap

Several items once listed as future work are now implemented:

- **WebAssembly target**: a WASM backend compiles the scalar subset plus heap types to `.wasm` with JS/DOM host imports.
- **Native code generation**: an x86-64 backend emits COFF objects and links Windows executables, with a freestanding/no-std mode and inline assembly.
- **Testing framework**: the `assert` builtin plus the `lullaby test` runner (`test_*` functions with pass/fail reporting).

## Future Extensions (Genuinely Planned)

- **Wider numeric lattice and FFI breadth**: full integer widths, `f32`, and broader C-ABI interop.
- **Generic user types, trait objects (`dyn`), and capturing closures**.
- **DSL / embedding support** and **profiling/optimization tooling**.

## Conclusion

Lullaby represents a fresh approach to systems programming, combining the performance and type safety of compiled languages with the simplicity and LLM-friendliness of modern design. By eliminating traditional sources of complexity (braces, semicolons, deep nesting) while maintaining rigorous type checking and memory safety, Lullaby enables developers to write clear, efficient code that can be reliably generated by smaller language models.

This specification provides the foundation for both human developers building complex systems programs and AI models generating correct, optimized code. The minimalist design philosophy ensures that as LLM capabilities improve, Lullaby will continue to benefit from more sophisticated generation while maintaining its core advantages of simplicity and efficiency.

---
**Status**: In active development toward 1.0 (1.0 not yet released). See [roadmap_1_0.md](roadmap_1_0.md) for scope and order.
**Design Goals**: Minimalism | Token Efficiency | LLM-Friendly | Type Safety | Memory Safety | Systems Programming
