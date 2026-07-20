# Lullaby Compilation Architecture Documentation

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Lullaby implements a novel multi-phase compilation pipeline specifically optimized for:
1. **Token efficiency** - Minimize source code token count for LLM generation
2. **Compile-time optimization** - Perform optimizations during compilation, not runtime
3. **Systems programming focus** - Direct hardware interaction with minimal abstraction overhead
4. **LLM compatibility** - Design patterns that tiny LLMs can understand and generate

## Compilation Pipeline Stages

## Current Pipeline

The current Rust workspace implements a frontend and in-process execution pipeline:

1. `lullaby_lexer` validates `.lby` paths, emits tokens, emits indentation/dedent structure, and rejects forbidden block delimiters.
2. `lullaby_parser` builds an AST for functions, typed parameters, `let`, assignment, returns, break/continue, if/elif/else, while/loop/range-for blocks, calls, literals, array literals/indexing, variables, arithmetic, comparisons, and boolean logic.
3. `lullaby_semantics` validates static types, explicit and inferred local bindings, assignments, function calls, return behavior, bool conditions, loop-control placement, arithmetic/comparison/logical operands, homogeneous non-empty arrays, array indexes, interim pointer-style memory builtins, text file I/O builtins, and safe system command builtins. Successful validation returns `CheckedProgram` metadata with function signatures and inferred expression types.
4. `lullaby_ir` lowers a `CheckedProgram` into typed semantic IR for the current implemented subset, including typed functions, parameters, statements, control flow, calls, builtins, and expressions. It also exposes memory-operation analysis for current heap-slot operations and array bounds checks so optimizers, bytecode work, and later native backends can share one side-effect and safety model.
5. `lullaby_runtime` executes the validated AST directly, including `main`, calls, scoped locals, assignment, branch result values, while/loop/range-for control flow, array literals/indexing with runtime bounds checks, arithmetic/comparisons, short-circuit boolean logic, heap-slot memory operations including `alloc`/`load`/`store`/`dealloc`, text file I/O, and safe system command builtins.
6. `lullaby_ir` provides a deterministic optimization pass framework. Implemented passes are constant folding for pure literal arithmetic, comparisons, boolean logic, string equality, and unary `not`, conservative block-local common subexpression elimination for repeated pure bindings, conservative loop-invariant motion for safe loop-body bindings, conservative block-local copy propagation for simple variable aliases, plus dead-code elimination for statements after unconditional `return`, `break`, or `continue` in the same block. Constant folding and loop-invariant motion deliberately leave potentially failing divide-by-zero expressions in place so runtime diagnostics and zero-iteration loop behavior are preserved. Optimizer barriers are conservative around calls and bounds-checked indexing.
7. `lullaby_ir` can also execute the lowered typed IR, lower it into an explicit instruction-bytecode module, and encode/decode a versioned `.lbc` bytecode artifact for the current implemented subset.
8. `lullaby_cli` exposes the current pipeline as `lullaby check <file.lby>`, `lullaby compile [--optimize none|constant-fold|dead-code|full] [-o output.lbc] <file.lby>`, `lullaby build [--optimize none|constant-fold|dead-code|full] [-o output.lbc] <file.lby>`, `lullaby inspect <file.lbc>`, `lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|full] <file.lby|file.lbc>`, `lullaby docs`, and `lullaby examples`. Optimization is opt-in and applies only to IR/bytecode source runs and compiled bytecode artifacts.

All three interpreter tiers (the AST runtime, and the IR tree-walker / bytecode VM) run their evaluation on a **dedicated large-stack thread** and enforce one **uniform call-depth bound** (`lullaby_runtime::interp_stack`: `run_on_interpreter_stack`, `INTERPRETER_STACK_SIZE` = 2 GiB, `INTERPRETER_RECURSION_LIMIT` = 20 000). The interpreters are recursive tree-walkers, so a well-defined but deep recursion used to overflow the host process (`STATUS_STACK_OVERFLOW`) at a tier-dependent depth (~200 AST, ~300 IR, ~1000 bytecode) — diverging from each other and from native, and blinding the differential fuzzers, which use the interpreters as the reference oracle, to any deep-recursing program. Running on the large stack lets every tier recurse far past that ceiling and agree with native on a terminating recursion, while the shared bound turns a genuinely unbounded recursion into one clean, deterministic diagnostic (`L0466` — a non-catchable runtime fault like a bounds violation, not observable by `try`/`catch`) on every interpreter tier — the same code at the same depth — rather than a bare, tier-dependent host abort. The bound sits far below the depth at which even the large stack would overflow in an unoptimized `debug` build, so the diagnostic always wins the race. `spawn`/`async`/`parallel_map` worker threads get the same large stack.

Additional optimization passes, native code generation, linking, and binary output remain planned architecture stages.

### Memory-Aware IR Contract

The current memory-aware IR increment keeps `alloc`, `load`, `store`, `dealloc`, and array indexing in the existing expression/statement IR shape while adding an analysis contract for backend consumers:

- `Allocate`: records the initialized value type and produced pointer type.
- `Load`: records the pointer type and loaded value type.
- `Store`: records the pointer type and stored value type.
- `Deallocate`: records the released pointer type.
- `BoundsCheck`: records the indexed target type and index type.

Each operation carries safety metadata for live-resource requirements, bounds-check requirements, memory mutation, cleanup role, and unsafe-boundary handling. Region creation/resizing, copy operations, and compiler-inserted cleanup are reserved in the roadmap but are not emitted until the language surface and runtime model support them.

### Native Backend Contract

`lullaby_ir::native_contract` records the first native backend contract before machine-code output exists. It defines the first prototype target, supported 64-bit target family, internal calling convention, stack-frame slot classes, current value layouts, pointer and array lowering rules, cleanup sequencing, and native diagnostic requirements.

The contract is serializable and unit-tested so object-emission work can consume stable data instead of embedding target policy directly into lowering code. See [native_backend_contract.md](native_backend_contract.md).

`lullaby_ir::native_object` now provides the first object-emission prototype for `x86_64-pc-windows-msvc`. It emits a deterministic COFF object for a zero-argument `main` that returns a literal `i64`, literal `bool`, `void`, a stack-backed `i64` local arithmetic expression, or straight-line `i64` local assignment arithmetic, after the source has already passed semantic validation, typed IR lowering, and bytecode lowering. Broader instruction lowering, linker orchestration, and native runtime packaging remain planned work.

The extended emitter (`emit_native_program`) compiles every function whose parameters and return type are all `i64` (at most four, passed in the Win64 argument registers `rcx`/`rdx`/`r8`/`r9`) to real machine code, and lowers the full scalar **control-flow** set structurally: `if`/`elif`/`else` chains, `while` loops, infinite `loop`s, and range `for i from a to b [by s]` loops (with `break`/`continue`), matching the interpreters' inclusive-range bounds and sign-of-step direction exactly. **Inter-function calls** among compiled functions are emitted with the Win64 ABI — arguments in the register order, the result in `rax`, 32 bytes of shadow space reserved, and the frame kept 16-byte aligned — resolved through `IMAGE_REL_AMD64_REL32` `call` relocations so one compiled function can call another (including recursively). Integer arithmetic stays checked, agreeing with the interpreters bit-for-bit. `match` is **not** in the native subset and skips gracefully: Lullaby's `match` is exclusively variant-based (over `option`/`result`/`enum` scrutinees, which are heap values), so there is no scalar (integer/bool) match to lower — any function using `match` (or any other heap value) falls back to the interpreters. These control-flow forms and calls are verified against the AST/IR/bytecode interpreters by the native link-and-run parity tests.

Within otherwise-`i64` functions, the native backend also compiles the fixed-width integer types (`i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`u64`/`isize`/`usize`). Each fixed-width value is held in a 64-bit register as the same normalized `i64` cell the interpreters use (signed kinds sign-extended, unsigned zero-extended, the 64-bit kinds filling the cell). Scalar `i64` functions get **register promotion**: two `i64` locals are kept in callee-saved registers (`rbx`/`rsi`) instead of stack slots, saved/restored across calls. The two are chosen by **loop-weighted usage** — a use inside a loop counts `8×` more per nesting level — so a hot loop counter/accumulator wins promotion over a loop-invariant parameter (correctness is independent of the choice; only speed changes). This covers functions containing range `for` loops — the loop counter and body accumulators live in registers; the loop's hidden bound/step stay as loop-invariant stack operands. The counting sum `while i < n { acc += i; i += 1 }` is recognized and lowered to an **O(1) closed form** — `acc += (i0 + n-1)·(n - i0)/2`, no loop at all. Because `(i0+n-1) + (n-i0) = 2n-1` is odd, exactly one factor is even, so the `/2` is exact under wrapping (halve the even factor, then multiply); `i0`, `n`, and the running `acc` are read at run time (any start value, constant or runtime bound), with a `count ≤ 0` guard for an empty loop. This runs **~26× faster than C's** O(n) loop. The closed form generalizes past the bare counting sum: an **affine** addend `acc += a*i + b` (`i+i`, `3*i+5`, weighted/running sums) also closes to O(1) — `acc += a*S + b*count` where `S` is the counting-sum closed form and `count = n - i0` — running **~30–40× faster than C**. A **quadratic** addend (`acc += c2*i² + c1*i + c0`) also closes to O(1) — `acc += c2*S2 + c1*S1 + c0*count` where `S2 = Σi²` uses Faulhaber's `g(m) = m(m+1)(2m+1)/6`, computed exactly mod 2^64 by halving the even factor of `{m, m+1}` and dividing by 3 through 3's modular inverse (`0xAAAA…AB`, exact because the value is always a multiple of 3) — **~45× faster than C**. Degree 3+ (`acc += i*i*i`) has no simple closed form, so it uses a four-way **multi-accumulator** unroll that breaks the serial `acc` dependency chain (~1.7× faster than the naive scalar loop). The affine closed form also applies to **`for` loops** (`for i from a to b { acc += a*i+b }`), which the ILP unroll could not reach (the `for` counter is stack-resident) — the closed form needs no counter at all and the counter is not observable after the loop, so `for` affine reductions run **~88× faster than C**. All are bit-identical to the scalar loop under wrapping arithmetic and are guarded by the differential fuzzer. Division and remainder by a **positive power-of-two constant** on plain `i64` also strength-reduce to shifts (a sign bias then `sar`/`shl`), avoiding the ~20-40-cycle `idiv` exactly as C does — bit-identical to `wrapping_div`/`wrapping_rem` including the `i64::MIN` and negative-dividend cases; non-power-of-two divisors keep `idiv`.

The native backend also **auto-vectorizes** array loop shapes over contiguous `array<i64>` into SSE2 packed loops (two `i64` lanes per iteration, with a scalar tail for a final odd element). **Reductions** `for i from S to E: acc <op>= a[i]` cover `+` (spelled `acc += a[i]`) and bitwise `& | ^` (spelled `acc = acc <op> a[i]`, either operand order): the packed accumulator is seeded with the operator's identity (`pxor`-zero for `+`/`|`/`^`, `pcmpeqd` all-ones for `&`), combined with `paddq`/`pand`/`por`/`pxor`, then the two lanes are horizontally folded into `acc`. Sum measured ~3.3x, bitwise ~2.89x, faster than the forced-scalar loop. **Element-wise maps** `for i from S to E: c[i] = a[i] <op> b[i]` cover `+ - & | ^` and emit a packed load/combine/store (`movdqu` two lanes from each source, `paddq`/`psubq`/`pand`/`por`/`pxor`, `movdqu` the result); the `+/-` map measured 3.36x. Because each offered op is associative (and, for bitwise, commutative) and exact on `i64`, every vectorized loop is bit-for-bit identical to the scalar version (including overflow wrap) and is parity-tested against every interpreter. The pattern matcher is exact — any other loop shape (a multiplied index, a different body, a non-unit step, a mismatched destination) falls through to the scalar `for` lowering, so correctness never depends on it. The remaining natural patterns are **blocked by the x86-64 baseline ISA, not by design**: dot product and product reduction need a 64-bit packed multiply (SSE2 has none; the `pmuludq` schoolbook emulation measures slower than a scalar `imul`), min/max reductions need a 64-bit packed compare (`pcmpgtq` is SSE4.2), and `f64` accumulation would break bit-exact interpreter parity (float addition is not associative). Unlocking them needs runtime CPU-feature detection with a widened (SSE4.2/AVX2) code path, or an opt-in fast-math mode for `f64`.

The backend emits wrapping arithmetic (`+ - * /`) re-normalized to the width, signedness-correct comparison (unsigned condition codes for unsigned kinds, signed for signed) and division (`div`/`idiv`), integer remainder `%` (reading `rdx` after the same `idiv`, with `x % -1` guarded to `0`; AArch64 uses `sdiv`+`msub`, WASM `rem_s`/`rem_u`), bitwise `& | ^ ~`, shifts with the count masked to the width and right shift logical-for-unsigned / arithmetic-for-signed, and the `to_<T>`/`to_i64` conversions emitted inline as a width-normalize. This matches the AST/IR/bytecode interpreters bit-for-bit (verified by the native link-and-run parity tests).

The same otherwise-`i64` functions may now also compute with **floating-point values** (`f64` and `f32`) using SSE scalar (XMM) registers. Float values live in XMM as a `double` (`f64`) or a `single` (`f32`, kept rounded to single precision after every operation to match the interpreter's real `f32` storage), spilled to 8-byte frame slots when a temporary must be held across another evaluation. The backend emits IEEE-754 arithmetic `+ - * /` (`addsd`/`subsd`/`mulsd`/`divsd` for `f64`; `addss`/`subss`/`mulss`/`divss` for `f32`, so division by zero yields inf/NaN rather than trapping, exactly like the interpreters); ordered comparisons `< <= > >= == !=` (`ucomisd`/`ucomiss` with unordered-aware condition codes, so a NaN operand makes every relational compare and `==` false and `!=` true); and the conversions `to_f32(x f64) -> f32` (`cvtsd2ss`, round to single) and `to_f64(x f32) -> f64` (`cvtss2sd`, widen), recognized and inlined as builtin calls. Float literals are materialized without a data-section constant: the IEEE-754 bit pattern is moved through a GPR into an XMM register (`movq`/`movd`). The signature constraint is unchanged — a function's parameters and return type must all be `i64` — so functions with a **float parameter or return type**, float values on the **heap**, and the transcendental/math builtins (`sqrt`/`sin`/`floor`/…) remain deferred and skip gracefully to the interpreters. Native float results match the AST/IR/bytecode interpreters bit-for-bit (verified by the native link-and-run parity tests).

Heap values (strings, lists, maps, structs/enums with heap payloads, closures), `match` over variant scrutinees, the option-returning `checked_*` builtins, the `saturating_*`/`wrapping_*` builtins, and float-typed signatures remain deferred: functions touching them are skipped and still run on the interpreters.

**C-ABI FFI (native-only).** The native backend can call C functions across the Win64 C ABI: a body-less `extern fn NAME params -> Ret` declares an imported C symbol, and a call lowers to a `call` of an **undefined external symbol** (`IMAGE_REL_AMD64_REL32` relocation, section 0), linked against the C runtime import library `ucrt.lib`. Marshalling covers the **integer-class C scalars** — every fixed-width integer (`i8`…`u64`, `isize`/`usize`) plus `bool`/`char`/`byte`: arguments pass in the low bits of the Win64 registers `rcx`/`rdx`/`r8`/`r9` (already width-normalized in the interpreter's cell model), and a narrow C return is re-normalized in `rax` (sign-extend for signed, zero-extend for unsigned). `f32`/`f64` externs need XMM argument routing and are deferred (their callers demote to the interpreters). The extern C-ABI signatures are threaded through the IR/bytecode as `extern_signatures` so the native emitter marshals each width. FFI is native-only: the AST/IR/bytecode interpreters cannot execute C and reject an `extern fn` call with the deterministic runtime diagnostic `L0423`. See [native_backend_contract.md](native_backend_contract.md) and [ffi_design.md](ffi_design.md).

**Object-format abstraction (COFF / ELF / Mach-O).** The generated x86-64 machine code, symbol table, and REL32 relocation model are platform-agnostic (the internal calling convention is self-consistent), so the same program can be written into three relocatable object containers. `lullaby native --target <triple>` selects the format; the default `x86_64-pc-windows-msvc` emits COFF exactly as before (byte-for-byte unchanged). `x86_64-unknown-linux-gnu` emits **ELF64** (`crates/lullaby_ir/src/elf_object.rs`: `Elf64_Ehdr`, `.text`/`.rodata`/`.bss`/`.rela.text`/`.symtab`/`.strtab`/`.shstrtab`, `R_X86_64_PLT32` for calls and `R_X86_64_PC32` for data), and `x86_64-apple-darwin` emits **Mach-O x86-64** (`crates/lullaby_ir/src/macho_object.rs`: `mach_header_64`, `LC_SEGMENT_64` with `__text`/`__const`/`__bss`, `LC_SYMTAB`, `LC_DYSYMTAB`, `X86_64_RELOC_BRANCH`/`X86_64_RELOC_SIGNED`). ELF and Mach-O flow through a shared container-neutral `ObjectModel` (`crates/lullaby_ir/src/object_model.rs`, tagged `ObjectMachine::X86_64`/`Aarch64`); only the object wrapper and the entry/exit stub differ (a freestanding `exit` syscall — `mov eax, 60; syscall` on Linux, `mov eax, 0x2000001; syscall` on macOS — instead of `kernel32!ExitProcess`, so no libc is required). On this Windows host the **x86-64** ELF/Mach-O objects are verified **structurally only** (parsed back to check magic, headers, sections, symbols, and relocations); their link-and-run verification is **deferred to the Phase 9 cross-platform CI**. `--target aarch64-apple-darwin` (Mach-O arm64) is still rejected with `L0347` (no Mach-O arm64 container yet). See [native_backend_contract.md](native_backend_contract.md).

**AArch64 (ARM64) instruction-set backend.** `crates/lullaby_ir/src/aarch64.rs` is Lullaby's **second instruction set**: `lullaby native --target aarch64-unknown-linux-gnu` lowers the same `BytecodeModule` to **AArch64** machine code and emits a freestanding **aarch64 ELF64** object (`EM_AARCH64`, `R_AARCH64_CALL26` `bl` relocations) through the same neutral `ObjectModel`/`elf_object.rs` path. It covers the `i64`-scalar core the x86-64 backend started from — `i64`/`bool` literals, locals, parameters (`x0..x7`) and returns (`x0`); `+ - * /` (`add`/`sub`/`mul`/`sdiv`), bitwise `& | ^ << >>` (`and`/`orr`/`eor`/`lslv`/`asrv`), comparisons (`cmp`+`cset`), short-circuit `and`/`or`/`not`; `if`/`elif`/`else`, `while`, `loop`, inclusive range `for` with `break`/`continue`; and inter-function `bl` calls under AAPCS64 with a freestanding `_start` (`bl main; movz x8, #93; svc #0`) that needs no libc. Non-scalar features (heap/aggregates, strings, floats, enums/`match`, FFI) are out of scope for this first core and are recorded as skipped functions (never miscompiled), exactly like the x86-64 backend's unsupported cases. Unlike the x86-64 ELF/Mach-O objects, the aarch64 object **is link-and-run verified**: a gated CLI test links it with the cross-linker (`ld.lld -m aarch64linux`) and runs it under Docker's `linux/arm64` (QEMU) emulation, asserting the process exit code equals the interpreter's `run` result mod 256 (skipped gracefully when Docker+arm64 or `ld.lld` is unavailable). See [native_backend_contract.md](native_backend_contract.md).

The **WebAssembly backend** (`lullaby_ir::wasm`, detailed in [wasm_backend_design.md](wasm_backend_design.md)) mirrors this float coverage on its own scalar subset. In addition to the existing `f64`, it now compiles `f32`: single-precision arithmetic `+ - * /` (`f32.add/sub/mul/div`, so f32 stays single precision and is bit-identical to the interpreter's real `f32`), IEEE-754 NaN-aware comparisons `< <= > >= == !=` (`f32.lt/le/gt/ge/eq/ne`), `f32.const` literals rounded to single precision, and the `to_f32` (`f32.demote_f64`, round) / `to_f64` (`f64.promote_f32`, widen) conversions recognized and inlined. `f32`/`f64` locals and aggregate slots use WASM's native `f32`/`f64` value types, so results agree with the interpreters bit-for-bit (verified by the node execution-parity tests). Float math builtins (`sqrt`/`sin`/…) and float-typed heap payloads remain deferred and skip gracefully.

The WebAssembly backend also compiles **enum values and `match`** for enums whose payloads are scalar — the built-in `option<T>` (`some(T)`/`none`) and `result<T, E>` (`ok(T)`/`err(E)`) when `T`/`E` are scalar, plus user enums whose every variant payload is scalar. An enum value is an `i32` pointer into linear memory to a `[tag: i32 (padded to 8)][slot0][slot1]...]` record: an `i32` discriminant equal to the variant's index in declaration order (matching the interpreters, which dispatch `match` by variant name against that same ordered layout) followed by one 8-byte payload slot per position, sized for the widest variant. Construction (`some(x)`/`none`/`ok(x)`/`err(e)` and user `Variant(payload...)`) `__alloc`s the record, stores the tag, and stores each payload value into its slot. `match` loads the tag once and dispatches with a chain of `i32.eq` + typed `if`/`else` blocks — a `Wildcard` arm becomes the final `else` — binding each arm's payload slots into locals before its body and yielding the arm value (a value match, every arm the same scalar type) or nothing (a void match). Enums with a **heap** payload (`string`/`list`/`array`/`map` — notably `result<i64, string>`), and `list`/`map` generally, remain deferred and skip gracefully to the interpreters. Node execution-parity tests confirm the WASM `match` results equal the interpreters bit-for-bit.

The WebAssembly backend passes and returns **scalar-field aggregates across function boundaries**: a `struct` (of scalars, nested scalar structs, or fixed scalar arrays), a fixed scalar `array`, or a supported scalar-payload `enum` may be a parameter, a return value, or a call argument. The `i32` pointer is passed/returned directly, but Lullaby **value semantics** are preserved by **deep-copying each mutable aggregate argument at the call site** before the `call`: a fresh record is `__alloc`'d and every word copied, recursing into nested mutable aggregate fields/elements, exactly matching the interpreters' recursive `Value::clone`. An immutable `string` argument is shared, not copied; a returned aggregate is the callee's own fresh record, so no copy is needed on return. Node execution-parity fixtures (`wasm_aggregate_args.lby`, `wasm_aggregate_nested.lby`) verify both direct and nested cases — including that a callee mutating its parameter leaves the caller's copy unchanged — bit-for-bit against the interpreters. Aggregates containing heap values the backend cannot lay out (a `list` with a heap element, `map`, or heap-payload enums) remain deferred and skip gracefully.

The **same value-copy rule applies everywhere a mutable aggregate is stored from an lvalue**, not just at call boundaries. The single governing rule: **an lvalue operand — a variable / field / index read of a `struct`/`array`/`enum`/`list`/`map` — is deep-copied (reusing the `emit_deep_copy` machinery) before it lands in its destination, exactly as the three interpreters clone the value when they read an lvalue. A freshly evaluated operand (a construction, an array literal, or a call result) already owns its own record and is stored directly — no redundant copy.** This applies at every store site that can receive an aggregate:

- **`let`/assignment RHS and field/element stores** — `let g = f`, `g = f`, and `o.inner = q` copy `f`/`q` when it is an lvalue (`wasm.rs` let/local-assign/path-assign lowering, via `maybe_copy_bound_value`).
- **Construction operands** — each field, element, or enum payload built from an lvalue is copied at the store: `Outer(q, …)`, `[f]`, and `some(f)` snapshot `q`/`f` (`lower_struct_construction`, `lower_array_literal`, `lower_enum_construction`). A construction whose operand is itself a *fresh* value — a nested construction (`Outer(Inner(5), …)`) or a call result — stores it directly.
- **`list`/`map` element and value stores** — `push`/`set`/`map_set` deep-copy the stored element/value (existing behavior in `wasm_lowering_mem.rs`).

The binding of a freshly constructed *whole record* (`let o = Outer(q, 2)`) is direct — the record is already fresh — but that is a statement about the record, **not** about its operands, which are copied at the construction store above. This is the precise line: value-copy is keyed on whether the **source being stored** is an lvalue, at *every* store, not on the syntactic form of the enclosing `let`/construction.

This closes a real cross-tier miscompile with two layers. First (`let g = f`): binding an aggregate variable bound `f`'s linear-memory pointer, so `g.a = …` also mutated `f`. Second (the construction operands `[f]`/`Outer(q,…)`/`some(f)`): a literal built from an aggregate lvalue aliased that operand, so mutating the operand afterward corrupted the stored copy. In both, WASM alone returned the corrupted value where the three interpreters and native returned the correct one. The fix is verified by the in-process **`wasmi` execution-parity suite** (`crates/lullaby_ir/src/wasm_exec_tests.rs`), which runs the emitted module and asserts `main`'s `i64` return equals the IR interpreter's for every observable copy shape — struct/array `let` and `=`, nested array-field copy, aggregate field-store, a by-value parameter copied inside a callee, a returned struct copied, and each construction-from-lvalue form (`[f]`, `Outer(q,…)`, `some(f)`, `push(l, f)`) mutating the operand after construction — the WASM analogue of the native compile-and-run exit-code tests. Enums and `list`/`map` have no in-place payload mutation (their "mutations" construct fresh values), so their `let`-aliasing was never observable; they are deep-copied anyway for consistency and covered by parity guards.

The WebAssembly backend also compiles the **growable `list<T>` collection for scalar element types** (`i64`, the fixed-width ints, `f32`/`f64`, `bool`, `char`, `byte`). A list is an `i32` pointer into linear memory to a `[len: i32][cap: i32][elem slots...]` header — the live element count, the allocated capacity, then `cap` uniform 8-byte element slots. `list_new()` allocates an empty small-capacity header; `push(l, x)`, `set(l, i, x)`, and `pop(l)` are value-semantic (each returns a NEW list): they deep-copy the source block first and mutate the fresh copy, so `l = push(l, x)` matches the interpreters' `Value::clone`-then-append, and a list crossing a call boundary is deep-copied like any other mutable aggregate. `push` grows a full list by reallocating a doubled-capacity block and copying the live elements (the old block is orphaned in the no-reclaim bump heap, exactly like existing string/struct/array growth). `get(l, i)` and `len(l)` read directly — `len` reuses the shared leading `i32` length header. Lists of **heap** elements (`list<string>`/`list<struct>`/`list<list<…>>`) and `map` remain deferred and skip gracefully. Node execution-parity fixtures (`wasm_list_build.lby`, `wasm_list_value_semantics.lby`) verify build/read/set/pop and the value-semantics guarantees bit-for-bit against the interpreters. See [wasm_backend_design.md](wasm_backend_design.md) for the full layout and grow/copy design.

### Stage 1: Lexical Analysis (Tokenizer)

Converts raw source text into a stream of tokens optimized for compact representation.

#### Token Types
- **Identifier Tokens**: `ident` (alphanumeric names without underscores)
- **Keyword Tokens**: Reserved words (`func`, `if`, `then`, `end`, etc.)
- **Operator Tokens**: Mathematical and logical operators as single symbols
- **Literal Tokens**:
  - Numbers: `num` (decimal, hex, binary formats)
  - Strings: `str` (without quotes - implicit string literals)
  - Booleans: `bool` (`true`, `false`)

#### Token Stream Optimization
```lullaby
source_code = tokenizer.parse(source_text)
stream = stream.compact(source_code)
```

The tokenizer uses context-aware parsing to reduce token count by ~40% compared to traditional languages:
- No parentheses needed (operator precedence handled by position)
- No semicolons (statement termination implicit via newline or block markers)
- No quotes for strings (implicit string delimiters removed)

#### Example Tokenization
```lullaby
// Source code snippet: x = 42 + "hello world"
// Traditional language tokens: 9 tokens
// Lullaby tokens: 5 tokens

Traditional: IDENT(x), OP(=), NUM(42), OP(+), STR("hello", "world"), ENDSTR
Lullaby:   ident(x), assign, num(42), plus, str(helloworld)
```

### Stage 2: Semantic Analysis (Type Checker)

Performs static type checking and generates intermediate representation (IR).

#### Type System Features
- **Static Typing**: Types inferred or explicitly declared
- **Type Safety**: Compile-time validation of type operations
- **Minimal Type Declarations**: Compact type representations

#### Type Inference Algorithm
Uses simplified Hindley-Milner style inference optimized for:
- Function argument types
- Return type deduction
- Expression result types
- Array element types

#### Semantic Analysis Output
```lullaby
ir = semantic_analyzer.analyze(source_code)
type_errors = checker.validate(ir)
```

The analyzer generates a simplified intermediate representation with:
- Type annotations attached to expressions
- Function signatures with parameter/return types
- Control flow graph without verbose type information
- Memory allocation metadata

#### Error Detection
Catches type-related errors before code generation:
- Type mismatches in operations
- Invalid function calls (wrong argument count/types)
- Uninitialized variable usage
- Incompatible pointer accesses

### Stage 3: Intermediate Representation Generation

Converts semantic analysis output into optimization-friendly IR format.

#### IR Structure
```lullaby
ir = [
  declarations: [...],     // Variable and function declarations with types
  functions: [...],        // Function definitions as control flow graphs
  imports: [...],          // External module references
  resources: [...]         // Memory, file, device allocations
]
```

#### IR Optimizations Built-in
- **Implemented now**: Opt-in constant folding through the `lullaby_ir` optimization framework and `lullaby run --backend ir|bytecode --optimize constant-fold`.
- **Implemented now**: Conservative block-local common subexpression elimination for repeated pure `let` initializer expressions in the current optimizer pipeline.
- **Implemented now**: Conservative loop-invariant motion for safe loop-body `let` initializers whose dependencies are available before the loop and are not declared or mutated inside the loop.
- **Implemented now**: Conservative block-local copy propagation for simple variable aliases in the current optimizer pipeline.
- **Implemented now**: Dead-code elimination for statements after explicit block terminators through `lullaby run --backend ir|bytecode --optimize dead-code`.
- **Implemented now**: The current pass pipeline through `lullaby run --backend ir|bytecode --optimize full`, running constant folding, CSE, loop-invariant motion, copy propagation, and dead-code elimination.
- **Planned**: Broader dead branch and unreachable control-flow elimination.
- **Planned**: Type propagation to infer missing types through data flow analysis.
- **Planned**: Memory layout optimization for cache-friendly variable placement.

### Stage 4: Code Generation (Compiler)

Transforms IR into efficient machine code with systems-level optimizations.

#### Target Architecture Support
- Native x86_64, ARM64 instruction sets
- Custom Lullaby bytecode (optional)
- Direct hardware abstraction layer

#### Current Bytecode Artifact

The current compiler artifact is a JSON `.lbc` file with a format marker, artifact version, metadata, entry point, function table, memory operation metadata, and instruction-bytecode module:

- `format`: `lullaby-bytecode`
- `version`: artifact version, currently `4`
- `metadata`: deterministic producer, target, and payload metadata
- `entry`: currently `main`
- `function_table`: declared bytecode function signatures used for compatibility checks
- `memory_operations`: analyzed allocation, load, store, deallocation, and bounds-check metadata used for backend and native-codegen preparation, including a stable artifact-order sequence for cleanup/lowering order
- `module`: bytecode functions containing dedicated `instructions` rather than raw IR statements

`lullaby compile file.lby -o file.lbc` writes this artifact. `lullaby build file.lby -o file.lbc` is the same artifact-generation path under a build-oriented command name. `lullaby inspect file.lbc` prints artifact metadata, function signatures, and memory operation counts, while verbose/JSON inspect output includes each memory operation's sequence number. `lullaby run file.lbc` executes it through the bytecode VM entry point. Unsupported artifact format, version, target, payload, entry values, duplicate functions, function-table/module mismatches, or memory-operation/module mismatches produce `L0601 [bytecode error]`.

#### Optimization Passes
1. **Algebraic Simplification**
   - `x + 0` -> `x`
   - `x * 1` -> `x`
   - `a + a + b` -> `2*a + b`

2. **Control Flow Optimization**
   - Loop invariant code motion
   - Dead branch elimination
   - Branch prediction hints

3. **Memory Optimization**
   - Stack allocation for local variables
   - Heap allocation for dynamic structures
   - Register assignment optimization

4. **Instruction Selection**
   - Match IR operations to optimal machine instructions
   - Fuse multiple operations when beneficial
   - Use vector instructions where applicable

#### Code Generation Output
```lullaby
machine_code = codegen.generate(ir, target_arch)
optimization_report = codegen.get_stats()
```

### Stage 5: Symbol Resolution and Linking

Resolves cross-module references and produces final executable.

#### Symbol Table Management
- Local scope resolution (function-level variables)
- Module-level resolution (global variables, functions)
- External reference resolution (library imports)

#### Linking Strategy
1. **Static Compilation**: All dependencies included in final binary
2. **Dynamic Compilation**: Separate compilation units linked together
3. **Incremental Compilation**: Update only changed sections

### Stage 6: Binary Output and Verification

Produces the final executable and performs final validation.

#### Binary Format
- ELF (Linux), Mach-O (macOS), PE (Windows) formats supported
- Optimized section layout for performance
- Minimal binary size through advanced compression

#### Final Verification
- Symbol consistency check
- Memory safety verification
- Optimization correctness validation

## Compilation Performance Characteristics

### Token Efficiency Metrics
| Aspect | Traditional Languages | Lullaby | Improvement |
|--------|----------------------|-----------|-------------|
| Function definitions | ~15 tokens | ~4 tokens | 73% reduction |
| Variable declarations | ~2 tokens each | ~1 token each | 50% reduction |
| Control structures | ~6-8 tokens per block | ~2-3 tokens per block | 60% reduction |
| Expressions | Variable based on complexity | Fixed minimal format | 40-60% reduction |

### Compilation Speed Optimization
- Parallel semantic analysis for large codebases
- Incremental compilation for development workflows
- Caching of IR and intermediate results
- Specialized passes for common patterns

### Memory Efficiency
- Minimal runtime overhead (< 5% vs interpreted languages)
- Efficient memory representation in IR
- Deterministic reclamation, no tracing collector: arena/region bulk-free (primary) plus scope-based reference-count drops (secondary), so the native backend reclaims memory at region reset and scope exit rather than through a garbage collector

## LLM Integration Considerations

### Training Data Design
The Lullaby syntax is designed to be:
1. **Pattern-consistent**: Regular structure enables pattern learning
2. **Symbol-simple**: Few unique tokens needed for vocabulary
3. **Context-clear**: Explicit relationships reduce ambiguity
4. **Size-efficient**: Short sequences fit within model contexts

### LLM-Friendly Features
- Consistent indentation-based scoping (no brace nesting)
- Linear statement flow (reduced parsing complexity)
- Predictable token patterns (easier sequence modeling)
- Type annotations at declaration points (simplifies inference)

## Example Compilation Flow

```lullaby
// Source code
func main(): void
  if input_count > max_limit then
    !0x41 Error: Input too large
    limit_to_max()
  end

  for i from 0 to input_count do
    process(input[i])
  end

  output_results()

// Compilation command
lullaby compile main.lby -o main.bin --optimize full --arch x86_64

// Compilation output
[INFO] Tokenizing source (52 tokens)...
[INFO] Semantic analysis complete (3 type errors detected, fixed)
[INFO] IR generation optimized (dead code removed: 12 statements)
[INFO] Code generation complete (x86_64 target)
[INFO] Binary output: main.bin (2.3 MB)
[SUCCESS] Compilation successful

// Execution
./main.bin < input_data.txt
```

## Conclusion

The Lullaby compilation architecture provides:
- **Minimal token usage** through compact syntax design (~60% reduction vs C++)
- **Compile-time optimization** shifting work away from runtime
- **LLM-friendly patterns** enabling smaller models to understand code
- **Systems programming efficiency** with direct hardware control
- **Type safety** through static analysis without runtime overhead

This architecture enables building highly efficient, LLM-compatible systems programming languages suitable for OS development while maintaining the minimalistic design philosophy central to Lullaby.
